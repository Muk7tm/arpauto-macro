//! The hold worker: one long-lived thread that owns the mouse backend and runs the
//! press → hold → release → wait cycle.
//!
//! ```text
//!   Idle ──start──▶ Arming (start delay) ──▶ WaitingForTarget ──▶ Running ──▶ Idle
//!                     │                         │                  │  ▲
//!                     └─────── stop ────────────┴──── stop ────────┤  │ target back
//!                                                                  ▼  │
//!                                                     Paused (focus/pointer lost, pause mode)
//! ```
//!
//! The button stays logically down for the whole hold (one press event, one release event);
//! holds are never emulated with repeated clicks. Every wait is a `recv_timeout` on the
//! command channel, so stop commands interrupt holds immediately, and whatever ends a run
//! releases the button.

use crate::config::{Config, FocusLoss, HoldSettings, Hotkeys, InputConfig};
use crate::input::{self, BackendKind, MouseBackend};
use crate::keys::MouseButton;
use crate::safety::{FocusProbe, HeldButtons, TargetPolicy, WindowInfo, XFocus};
use crate::timing::{self, ms, Scheduler, TimingStats};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often focus and pointer position are re-checked during holds and waits.
const FOCUS_SLICE: Duration = Duration::from_millis(10);
/// How long a new uinput device may take to appear in the X server.
const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(3);
/// Lowercase WM_CLASS fragment of this application's own window.
pub const OWN_WINDOW_CLASS: &str = "arpauto-macro";

/// Immutable snapshot of everything a run needs.
#[derive(Clone, Debug, PartialEq)]
pub struct RunSpec {
    pub hold: HoldSettings,
    pub input: InputConfig,
    pub hotkeys: Hotkeys,
    /// Pause between locking the target and the first press (calibration uses this so the
    /// click that focused the game is over before testing starts).
    pub settle_ms: u32,
}

impl RunSpec {
    pub fn from_config(c: &Config) -> RunSpec {
        RunSpec { hold: c.hold, input: c.input.clone(), hotkeys: c.hotkeys, settle_ms: 0 }
    }
}

#[derive(Debug)]
pub enum Command {
    /// New configuration snapshot, used from the next run on.
    Update(Arc<RunSpec>),
    Toggle,
    Start,
    Stop,
    HoldPress,
    HoldRelease,
    Emergency,
    /// Run a calibration test with its own settings.
    Calibrate(Arc<RunSpec>),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Arming,
    WaitingForTarget,
    Running,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Stopped,
    Emergency,
    FocusLost,
    Failed,
    Shutdown,
}

/// Measured button-down durations (between the X server accepting the press and the release).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HoldStats {
    pub count: u64,
    pub min: Duration,
    pub max: Duration,
    pub total: Duration,
}

impl HoldStats {
    fn add(&mut self, d: Duration) {
        if self.count == 0 || d < self.min {
            self.min = d;
        }
        if d > self.max {
            self.max = d;
        }
        self.count += 1;
        self.total += d;
    }

    pub fn mean(&self) -> Duration {
        if self.count == 0 {
            Duration::ZERO
        } else {
            self.total / u32::try_from(self.count).unwrap_or(u32::MAX)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    State { state: RunState, detail: String },
    Finished { reason: StopReason, cycles: u64, holds: HoldStats, timing: TimingStats, message: String, calibration: bool },
}

/// Everything the engine needs from the outside world; mocked in tests.
pub trait Platform {
    fn backend(&mut self, kind: BackendKind) -> Result<&mut (dyn MouseBackend + 'static), String>;
    fn existing_backend(&mut self) -> Option<&mut (dyn MouseBackend + 'static)>;
    fn reset_backend(&mut self);
    fn focus(&mut self) -> Result<Option<WindowInfo>, String>;
    /// `None` if the window no longer exists.
    fn top_level_of(&mut self, window: u32) -> Result<Option<u32>, String>;
    fn pointer_top_level(&mut self) -> Result<Option<u32>, String>;
    fn own_pid(&self) -> u32 {
        std::process::id()
    }
}

/// Production platform: X11 focus/pointer tracking plus XTest/uinput backends.
#[derive(Default)]
pub struct X11Platform {
    focus: Option<XFocus>,
    backend: Option<Box<dyn MouseBackend>>,
}

impl X11Platform {
    fn x(&mut self) -> Result<&mut XFocus, String> {
        if self.focus.is_none() {
            self.focus = Some(XFocus::connect()?);
        }
        Ok(self.focus.as_mut().expect("just set"))
    }

    fn with_x<T>(&mut self, f: impl FnOnce(&mut XFocus) -> Result<T, String>) -> Result<T, String> {
        let result = f(self.x()?);
        if result.is_err() {
            self.focus = None; // reconnect next time
        }
        result
    }
}

impl Platform for X11Platform {
    fn backend(&mut self, kind: BackendKind) -> Result<&mut (dyn MouseBackend + 'static), String> {
        if self.backend.as_ref().is_some_and(|b| b.kind() != kind) {
            self.backend = None;
        }
        if self.backend.is_none() {
            self.backend = Some(input::create_backend(kind).map_err(|e| e.0)?);
        }
        Ok(self.backend.as_deref_mut().expect("just set"))
    }

    fn existing_backend(&mut self) -> Option<&mut (dyn MouseBackend + 'static)> {
        self.backend.as_deref_mut()
    }

    fn reset_backend(&mut self) {
        self.backend = None;
    }

    fn focus(&mut self) -> Result<Option<WindowInfo>, String> {
        self.with_x(|x| x.active())
    }

    fn top_level_of(&mut self, window: u32) -> Result<Option<u32>, String> {
        self.with_x(|x| x.top_level_of(window))
    }

    fn pointer_top_level(&mut self) -> Result<Option<u32>, String> {
        self.with_x(|x| x.pointer_top_level())
    }
}

enum Interrupt {
    Stop,
    Emergency,
    Shutdown,
    TargetLost(&'static str),
    Failed(String),
}

/// Where clicks may go during a running session.
struct Guard {
    protection: bool,
    target: u32,
    target_top: u32,
    policy: TargetPolicy,
}

pub struct Engine<P: Platform> {
    platform: P,
    rx: Receiver<Command>,
    events: Box<dyn Fn(EngineEvent) + Send>,
    spec: Option<Arc<RunSpec>>,
    held: HeldButtons,
    state: RunState,
    detail: String,
    backend_kind: BackendKind,
    /// Button of the run in progress (for emergency release).
    active_button: Option<MouseButton>,
}

impl<P: Platform> Engine<P> {
    pub fn new(platform: P, rx: Receiver<Command>, events: Box<dyn Fn(EngineEvent) + Send>) -> Self {
        Engine {
            platform,
            rx,
            events,
            spec: None,
            held: HeldButtons::default(),
            state: RunState::Idle,
            detail: String::new(),
            backend_kind: BackendKind::Xtest,
            active_button: None,
        }
    }

    /// Process commands until `Shutdown` or until every sender is dropped.
    pub fn run(mut self) {
        timing::tighten_timer_slack();
        while let Ok(cmd) = self.rx.recv() {
            let reason = match cmd {
                Command::Update(spec) => {
                    self.spec = Some(spec);
                    continue;
                }
                Command::Start | Command::Toggle | Command::HoldPress => match self.spec.clone() {
                    Some(spec) => self.session(&spec, false),
                    None => continue,
                },
                Command::Calibrate(spec) => self.session(&spec, true),
                Command::Stop | Command::HoldRelease => continue,
                Command::Emergency => {
                    self.release_all(true);
                    continue;
                }
                Command::Shutdown => break,
            };
            if reason == StopReason::Shutdown {
                break;
            }
        }
        self.release_all(false);
    }

    fn emit(&self, ev: EngineEvent) {
        (self.events)(ev);
    }

    fn set_state(&mut self, state: RunState, detail: impl Into<String>) {
        let detail = detail.into();
        if state != self.state || detail != self.detail {
            self.state = state;
            self.detail = detail.clone();
            self.emit(EngineEvent::State { state, detail });
        }
    }

    fn session(&mut self, spec: &RunSpec, calibration: bool) -> StopReason {
        let mut holds = HoldStats::default();
        let mut timing = TimingStats::default();
        let mut cycles = 0;
        let outcome = self.session_inner(spec, &mut holds, &mut timing, &mut cycles);
        let (reason, message) = match outcome {
            Ok(()) => (StopReason::Completed, String::new()),
            Err(Interrupt::Stop) => (StopReason::Stopped, String::new()),
            Err(Interrupt::Emergency) => (StopReason::Emergency, "Emergency stop".to_string()),
            Err(Interrupt::Shutdown) => (StopReason::Shutdown, String::new()),
            Err(Interrupt::TargetLost(why)) => (StopReason::FocusLost, format!("Stopped: {why}")),
            Err(Interrupt::Failed(e)) => (StopReason::Failed, e),
        };
        self.release_all(reason == StopReason::Emergency);
        if reason == StopReason::Failed {
            tracing::warn!("run failed: {message}");
            self.platform.reset_backend();
        }
        tracing::debug!(?reason, cycles, ?holds, ?timing, "session finished");
        self.active_button = None;
        self.set_state(RunState::Idle, "");
        self.emit(EngineEvent::Finished { reason, cycles, holds, timing, message, calibration });
        reason
    }

    fn session_inner(
        &mut self,
        spec: &RunSpec,
        holds: &mut HoldStats,
        timing_out: &mut TimingStats,
        cycles: &mut u64,
    ) -> Result<(), Interrupt> {
        if let Some(problem) = spec.hold.validate().into_iter().next() {
            return Err(Interrupt::Failed(problem));
        }
        let button = spec.hold.button;
        self.active_button = Some(button);
        self.backend_kind = spec.input.backend;
        self.platform.backend(spec.input.backend).map_err(Interrupt::Failed)?;

        let start_at = Instant::now() + ms(spec.hold.start_delay_ms);
        if start_at > Instant::now() {
            self.set_state(RunState::Arming, format!("{} ms", spec.hold.start_delay_ms));
            self.wait_until(start_at, None)?;
        }
        self.wait_backend_ready(spec.input.backend)?;

        let policy = TargetPolicy {
            protection: spec.input.focus_protection,
            class_filter: spec.input.target_class.clone(),
            own_pid: self.platform.own_pid(),
            own_class: OWN_WINDOW_CLASS,
        };
        let guard = self.acquire_target(policy, spec)?;
        if spec.settle_ms > 0 {
            self.set_state(RunState::Arming, format!("{} ms", spec.settle_ms));
            self.wait_until(Instant::now() + ms(spec.settle_ms), Some(&guard))?;
        }
        self.set_state(RunState::Running, "");

        let hold = ms(spec.hold.timing.hold_ms);
        let release = ms(spec.hold.timing.release_ms);
        let repeat = spec.hold.repeat;
        let mut sched = Scheduler::new(Instant::now());
        let result = loop {
            let more = repeat.enabled && (repeat.count == 0 || *cycles + 1 < u64::from(repeat.count));
            match self.cycle(button, hold, release, more, &guard, &mut sched, holds) {
                Ok(()) => {}
                Err(Interrupt::TargetLost(why)) if spec.input.focus_loss == FocusLoss::Pause => {
                    self.release_all(false);
                    self.set_state(RunState::Paused, format!("Paused: {why}"));
                    if let Err(e) = self.wait_for_target(&guard) {
                        break Err(e);
                    }
                    self.set_state(RunState::Running, "");
                    sched.reset(Instant::now());
                    continue; // the interrupted hold is repeated in full
                }
                Err(e) => break Err(e),
            }
            *cycles += 1;
            if !more {
                break Ok(());
            }
        };
        *timing_out = sched.stats();
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn cycle(
        &mut self,
        button: MouseButton,
        hold: Duration,
        release: Duration,
        trailing_release: bool,
        guard: &Guard,
        sched: &mut Scheduler,
        holds: &mut HoldStats,
    ) -> Result<(), Interrupt> {
        self.check_target(guard)?;
        self.held.down(button);
        self.send(button, true)?;
        let pressed_at = Instant::now();
        sched.mark_emitted(pressed_at);

        let deadline = sched.advance(hold);
        self.wait_until(deadline, Some(guard))?;

        self.send(button, false)?;
        self.held.up(button);
        let released_at = Instant::now();
        sched.mark_emitted(released_at);
        holds.add(released_at - pressed_at);

        if trailing_release {
            let deadline = sched.advance(release);
            self.wait_until(deadline, Some(guard))?;
        }
        Ok(())
    }

    fn send(&mut self, button: MouseButton, down: bool) -> Result<(), Interrupt> {
        let kind = self.backend_kind;
        self.platform
            .backend(kind)
            .map_err(Interrupt::Failed)?
            .button(button, down)
            .map_err(|e| Interrupt::Failed(e.0))
    }

    fn on_busy_command(&mut self, cmd: Command) -> Result<(), Interrupt> {
        match cmd {
            Command::Update(spec) => {
                self.spec = Some(spec);
                Ok(())
            }
            // Already running: never a second worker or overlapping run.
            Command::Start | Command::HoldPress | Command::Calibrate(_) => Ok(()),
            Command::Toggle | Command::Stop | Command::HoldRelease => Err(Interrupt::Stop),
            Command::Emergency => Err(Interrupt::Emergency),
            Command::Shutdown => Err(Interrupt::Shutdown),
        }
    }

    fn wait_until(&mut self, deadline: Instant, guard: Option<&Guard>) -> Result<(), Interrupt> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            let slice = (deadline - now).min(FOCUS_SLICE);
            match self.rx.recv_timeout(slice) {
                Ok(cmd) => self.on_busy_command(cmd)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(Interrupt::Shutdown),
            }
            if let Some(g) = guard {
                if Instant::now() < deadline {
                    self.check_target(g)?;
                }
            }
        }
    }

    fn wait_backend_ready(&mut self, kind: BackendKind) -> Result<(), Interrupt> {
        let deadline = Instant::now() + BACKEND_READY_TIMEOUT;
        loop {
            let ready = self.platform.backend(kind).map_err(Interrupt::Failed)?.is_ready().map_err(|e| Interrupt::Failed(e.0))?;
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Interrupt::Failed(
                    "The X server did not pick up the uinput device within 3 s; use the XTest backend".into(),
                ));
            }
            self.set_state(RunState::Arming, "Preparing the virtual mouse");
            self.wait_until(Instant::now() + Duration::from_millis(20), None)?;
        }
    }

    /// Is the guard's condition met right now? `Err` describes why not.
    fn target_ok(&mut self, guard: &Guard) -> Result<Result<(), &'static str>, Interrupt> {
        let active = self.platform.focus().map_err(Interrupt::Failed)?;
        if !guard.protection {
            // Without protection only this app's own window is off limits.
            return Ok(match active {
                Some(w) if guard.policy.is_own(&w) => Err("this window was focused"),
                _ => Ok(()),
            });
        }
        if active.map(|w| w.id) != Some(guard.target) {
            return Ok(Err("the target window lost focus"));
        }
        // Clicks go to the window under the pointer, not the focused one.
        if self.platform.pointer_top_level().map_err(Interrupt::Failed)? != Some(guard.target_top) {
            return Ok(Err("the pointer left the target window"));
        }
        Ok(Ok(()))
    }

    fn check_target(&mut self, guard: &Guard) -> Result<(), Interrupt> {
        match self.target_ok(guard)? {
            Ok(()) => Ok(()),
            Err(why) => Err(Interrupt::TargetLost(why)),
        }
    }

    /// Wait until an eligible window is focused (and, with protection, under the pointer).
    fn acquire_target(&mut self, policy: TargetPolicy, spec: &RunSpec) -> Result<Guard, Interrupt> {
        loop {
            if let Some(w) = self.platform.focus().map_err(Interrupt::Failed)? {
                if policy.eligible(&w) {
                    if !policy.protection {
                        return Ok(Guard { protection: false, target: w.id, target_top: 0, policy });
                    }
                    // `None`: the window was closed meanwhile; keep waiting.
                    if let Some(top) = self.platform.top_level_of(w.id).map_err(Interrupt::Failed)? {
                        if self.platform.pointer_top_level().map_err(Interrupt::Failed)? == Some(top) {
                            return Ok(Guard { protection: true, target: w.id, target_top: top, policy });
                        }
                    }
                    self.set_state(RunState::WaitingForTarget, "Move the pointer over the target window");
                    self.wait_until(Instant::now() + Duration::from_millis(25), None)?;
                    continue;
                }
            }
            let detail = if spec.input.focus_protection && !spec.input.target_class.trim().is_empty() {
                format!("Waiting for a \"{}\" window", spec.input.target_class.trim())
            } else {
                "Waiting for the target window".to_string()
            };
            self.set_state(RunState::WaitingForTarget, detail);
            self.wait_until(Instant::now() + Duration::from_millis(25), None)?;
        }
    }

    fn wait_for_target(&mut self, guard: &Guard) -> Result<(), Interrupt> {
        loop {
            if self.target_ok(guard)?.is_ok() {
                return Ok(());
            }
            self.wait_until(Instant::now() + Duration::from_millis(25), None)?;
        }
    }

    /// Release every held button. With `emergency`, also send a release for the run's button.
    fn release_all(&mut self, emergency: bool) {
        let mut buttons = self.held.drain_reverse();
        if emergency {
            if let Some(b) = self.active_button {
                if !buttons.contains(&b) {
                    buttons.push(b);
                }
            }
        }
        if buttons.is_empty() {
            return;
        }
        let Some(backend) = self.platform.existing_backend() else {
            tracing::warn!("cannot release held buttons: no backend");
            return;
        };
        for b in buttons {
            if let Err(e) = backend.button(b, false) {
                tracing::warn!("failed to release {} mouse: {e}", b.label());
            }
        }
    }
}

impl<P: Platform> Drop for Engine<P> {
    fn drop(&mut self) {
        // Covers panics inside the worker: never leave a button stuck down.
        self.release_all(false);
    }
}

/// Owner-side handle of the worker thread.
pub struct EngineHandle {
    tx: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl EngineHandle {
    /// Spawn the worker. `make_platform` runs on the worker thread.
    pub fn spawn<P, F>(make_platform: F, events: impl Fn(EngineEvent) + Send + 'static) -> std::io::Result<EngineHandle>
    where
        P: Platform + 'static,
        F: FnOnce() -> P + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let events: Box<dyn Fn(EngineEvent) + Send> = Box::new(events);
        let join = std::thread::Builder::new().name("hold-engine".into()).spawn(move || {
            Engine::new(make_platform(), rx, events).run();
        })?;
        Ok(EngineHandle { tx, join: Some(join) })
    }

    pub fn sender(&self) -> Sender<Command> {
        self.tx.clone()
    }

    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_err() {
            tracing::error!("hold worker is not running");
        }
    }

    /// Stop any run, release the button and join the worker.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(j) = self.join.take() {
            if j.join().is_err() {
                tracing::error!("hold worker panicked");
            }
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
