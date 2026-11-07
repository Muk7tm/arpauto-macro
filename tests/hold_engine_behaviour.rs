//! Hold-worker behaviour against a mock platform that records every button event with a
//! timestamp and lets tests move "focus" and "the pointer" between windows.

use arpauto_macro::config::{FocusLoss, HoldSettings, HoldTiming, Hotkeys, InputConfig, RepeatConfig};
use arpauto_macro::hold_engine::{Command, EngineEvent, EngineHandle, HoldStats, Platform, RunSpec, RunState, StopReason};
use arpauto_macro::input::{BackendKind, InputResult, MouseBackend};
use arpauto_macro::keys::MouseButton;
use arpauto_macro::safety::WindowInfo;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Log = Arc<Mutex<Vec<(Instant, MouseButton, bool)>>>;

struct MockBackend {
    log: Log,
}

impl MouseBackend for MockBackend {
    fn button(&mut self, button: MouseButton, down: bool) -> InputResult {
        self.log.lock().unwrap().push((Instant::now(), button, down));
        Ok(())
    }
    fn kind(&self) -> BackendKind {
        BackendKind::Xtest
    }
    fn is_ready(&mut self) -> InputResult<bool> {
        Ok(true)
    }
}

const OWN_PID: u32 = 1;

#[derive(Default)]
struct World {
    focus: Option<WindowInfo>,
    /// Top-level window under the pointer.
    pointer: Option<u32>,
    /// Client window that has been closed (queries about it find nothing).
    closed: Option<u32>,
}

struct MockPlatform {
    log: Log,
    world: Arc<Mutex<World>>,
    backend: Option<MockBackend>,
}

/// Mock frames: the frame of client window N is N + 1000.
fn frame(id: u32) -> u32 {
    id + 1000
}

impl Platform for MockPlatform {
    fn backend(&mut self, _kind: BackendKind) -> Result<&mut (dyn MouseBackend + 'static), String> {
        if self.backend.is_none() {
            self.backend = Some(MockBackend { log: self.log.clone() });
        }
        Ok(self.backend.as_mut().unwrap())
    }
    fn existing_backend(&mut self) -> Option<&mut (dyn MouseBackend + 'static)> {
        self.backend.as_mut().map(|b| b as &mut (dyn MouseBackend + 'static))
    }
    fn reset_backend(&mut self) {
        self.backend = None;
    }
    fn focus(&mut self) -> Result<Option<WindowInfo>, String> {
        Ok(self.world.lock().unwrap().focus.clone())
    }
    fn top_level_of(&mut self, window: u32) -> Result<Option<u32>, String> {
        let closed = self.world.lock().unwrap().closed == Some(window);
        Ok((!closed).then(|| frame(window)))
    }
    fn pointer_top_level(&mut self) -> Result<Option<u32>, String> {
        Ok(self.world.lock().unwrap().pointer)
    }
    fn own_pid(&self) -> u32 {
        OWN_PID
    }
}

fn game() -> WindowInfo {
    WindowInfo { id: 100, pid: Some(500), class: "sober org.vinegarhq.Sober".into() }
}
fn other_window() -> WindowInfo {
    WindowInfo { id: 200, pid: Some(600), class: "code Code".into() }
}
fn own_window() -> WindowInfo {
    WindowInfo { id: 300, pid: Some(OWN_PID), class: "arpauto-macro".into() }
}

struct Harness {
    engine: EngineHandle,
    events: Receiver<EngineEvent>,
    log: Log,
    world: Arc<Mutex<World>>,
}

struct Finished {
    reason: StopReason,
    cycles: u64,
    holds: HoldStats,
    message: String,
    calibration: bool,
}

impl Harness {
    /// Focus and pointer both on `w`.
    fn new(w: WindowInfo) -> Harness {
        let log: Log = Arc::default();
        let world = Arc::new(Mutex::new(World { pointer: Some(frame(w.id)), focus: Some(w), closed: None }));
        let (tx, events) = mpsc::channel();
        let (l, wo) = (log.clone(), world.clone());
        let engine = EngineHandle::spawn(move || MockPlatform { log: l, world: wo, backend: None }, move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        Harness { engine, events, log, world }
    }

    fn update(&self, spec: RunSpec) {
        self.engine.send(Command::Update(Arc::new(spec)));
    }

    fn focus(&self, w: Option<WindowInfo>) {
        self.world.lock().unwrap().focus = w;
    }

    fn pointer(&self, top: Option<u32>) {
        self.world.lock().unwrap().pointer = top;
    }

    fn wait_event(&self, timeout: Duration, pred: impl Fn(&EngineEvent) -> bool) -> EngineEvent {
        let deadline = Instant::now() + timeout;
        loop {
            match self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(ev) if pred(&ev) => return ev,
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for engine event"),
            }
        }
    }

    fn wait_state(&self, state: RunState) -> String {
        match self.wait_event(Duration::from_secs(3), |e| matches!(e, EngineEvent::State { state: s, .. } if *s == state)) {
            EngineEvent::State { detail, .. } => detail,
            _ => unreachable!(),
        }
    }

    fn wait_finished(&self, timeout: Duration) -> Finished {
        match self.wait_event(timeout, |e| matches!(e, EngineEvent::Finished { .. })) {
            EngineEvent::Finished { reason, cycles, holds, message, calibration, .. } => {
                Finished { reason, cycles, holds, message, calibration }
            }
            _ => unreachable!(),
        }
    }

    fn wait_log_len(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.log.lock().unwrap().len() < n {
            assert!(Instant::now() < deadline, "timed out waiting for {n} button events");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn events(&self) -> Vec<(Instant, MouseButton, bool)> {
        self.log.lock().unwrap().clone()
    }

    fn edges(&self) -> Vec<(MouseButton, bool)> {
        self.events().iter().map(|(_, b, d)| (*b, *d)).collect()
    }
}

fn spec(button: MouseButton, hold_ms: u32, release_ms: u32, repeat: RepeatConfig) -> RunSpec {
    RunSpec {
        hold: HoldSettings { button, timing: HoldTiming { hold_ms, release_ms }, repeat, start_delay_ms: 0 },
        input: InputConfig::default(),
        hotkeys: Hotkeys::default(),
        settle_ms: 0,
    }
}

const FOREVER: RepeatConfig = RepeatConfig { enabled: true, count: 0 };
const ONCE: RepeatConfig = RepeatConfig { enabled: false, count: 0 };
const L: MouseButton = MouseButton::Left;

#[test]
fn hold_release_cycles_have_configured_timing() {
    let h = Harness::new(game());
    h.update(spec(L, 30, 20, RepeatConfig { enabled: true, count: 5 }));
    h.engine.send(Command::Start);
    let f = h.wait_finished(Duration::from_secs(3));
    assert_eq!((f.reason, f.cycles), (StopReason::Completed, 5));
    assert_eq!(h.edges(), [(L, true), (L, false)].repeat(5));

    let ev = h.events();
    for (i, pair) in ev.windows(2).enumerate() {
        let dt = pair[1].0 - pair[0].0;
        let (min, what) = if i % 2 == 0 { (Duration::from_millis(29), "hold") } else { (Duration::from_millis(19), "release") };
        assert!(dt >= min, "{what} {i} too short: {dt:?}");
        assert!(dt < min + Duration::from_millis(20), "{what} {i} too long: {dt:?}");
    }
    assert_eq!(f.holds.count, 5);
    assert!(f.holds.min >= Duration::from_millis(29), "{:?}", f.holds);
    assert!(f.holds.max < Duration::from_millis(50), "{:?}", f.holds);
}

#[test]
fn hold_is_one_continuous_press_not_repeated_clicks() {
    let h = Harness::new(game());
    h.update(spec(MouseButton::Right, 250, 10, ONCE));
    h.engine.send(Command::Start);
    let f = h.wait_finished(Duration::from_secs(3));
    assert_eq!(f.reason, StopReason::Completed);
    assert_eq!(h.edges(), [(MouseButton::Right, true), (MouseButton::Right, false)]);
    let ev = h.events();
    assert!(ev[1].0 - ev[0].0 >= Duration::from_millis(249));
}

#[test]
fn stop_during_hold_releases_immediately() {
    let h = Harness::new(game());
    h.update(spec(L, 5_000, 60, FOREVER));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    let t = Instant::now();
    h.engine.send(Command::Toggle);
    assert_eq!(h.wait_finished(Duration::from_millis(500)).reason, StopReason::Stopped);
    let ev = h.events();
    assert_eq!(h.edges(), [(L, true), (L, false)]);
    assert!(ev[1].0.saturating_duration_since(t) < Duration::from_millis(20));
}

#[test]
fn focus_loss_stops_and_releases() {
    let h = Harness::new(game());
    h.update(spec(L, 5_000, 60, FOREVER));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.focus(Some(other_window()));
    let f = h.wait_finished(Duration::from_millis(500));
    assert_eq!(f.reason, StopReason::FocusLost);
    assert!(f.message.contains("lost focus"), "{}", f.message);
    assert_eq!(h.edges(), [(L, true), (L, false)]);
}

#[test]
fn pointer_leaving_target_stops_and_releases() {
    let h = Harness::new(game());
    h.update(spec(L, 5_000, 60, FOREVER));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.pointer(Some(frame(other_window().id)));
    let f = h.wait_finished(Duration::from_millis(500));
    assert_eq!(f.reason, StopReason::FocusLost);
    assert!(f.message.contains("pointer"), "{}", f.message);
    assert_eq!(h.edges(), [(L, true), (L, false)]);
}

#[test]
fn pause_mode_releases_and_resumes() {
    let h = Harness::new(game());
    let mut s = spec(L, 40, 10, FOREVER);
    s.input.focus_loss = FocusLoss::Pause;
    h.update(s);
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.pointer(None);
    let detail = h.wait_state(RunState::Paused);
    assert!(detail.contains("pointer"), "{detail}");
    assert_eq!(h.edges().last(), Some(&(L, false)));
    let n = h.events().len();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(h.events().len(), n, "no clicks while paused");
    h.pointer(Some(frame(game().id)));
    h.wait_state(RunState::Running);
    h.wait_log_len(n + 2);
    h.engine.send(Command::Stop);
    assert_eq!(h.wait_finished(Duration::from_millis(500)).reason, StopReason::Stopped);
    assert_eq!(h.edges().last(), Some(&(L, false)));
}

#[test]
fn waits_for_pointer_over_target_before_first_press() {
    let h = Harness::new(game());
    h.pointer(Some(frame(other_window().id)));
    h.update(spec(L, 10, 10, ONCE));
    h.engine.send(Command::Start);
    let detail = h.wait_state(RunState::WaitingForTarget);
    assert!(detail.contains("pointer"), "{detail}");
    std::thread::sleep(Duration::from_millis(40));
    assert!(h.events().is_empty());
    h.pointer(Some(frame(game().id)));
    assert_eq!(h.wait_finished(Duration::from_secs(1)).reason, StopReason::Completed);
    assert_eq!(h.edges().len(), 2);
}

#[test]
fn own_window_is_never_clicked() {
    let h = Harness::new(own_window());
    h.update(spec(L, 10, 10, ONCE));
    h.engine.send(Command::Start);
    h.wait_state(RunState::WaitingForTarget);
    std::thread::sleep(Duration::from_millis(40));
    assert!(h.events().is_empty());
    h.focus(Some(game()));
    h.pointer(Some(frame(game().id)));
    assert_eq!(h.wait_finished(Duration::from_secs(1)).reason, StopReason::Completed);
}

#[test]
fn without_protection_pointer_is_not_required() {
    let h = Harness::new(game());
    h.pointer(None);
    let mut s = spec(L, 10, 10, ONCE);
    s.input.focus_protection = false;
    h.update(s);
    h.engine.send(Command::Start);
    assert_eq!(h.wait_finished(Duration::from_secs(1)).reason, StopReason::Completed);
}

#[test]
fn class_filter_selects_target() {
    let h = Harness::new(other_window());
    let mut s = spec(L, 10, 10, ONCE);
    s.input.target_class = "sober".into();
    h.update(s);
    h.engine.send(Command::Start);
    h.wait_state(RunState::WaitingForTarget);
    assert!(h.events().is_empty());
    h.focus(Some(game()));
    h.pointer(Some(frame(game().id)));
    assert_eq!(h.wait_finished(Duration::from_secs(1)).reason, StopReason::Completed);
}

#[test]
fn invalid_settings_fail_without_clicking() {
    let h = Harness::new(game());
    h.update(spec(L, 2, 1, FOREVER));
    h.engine.send(Command::Start);
    let f = h.wait_finished(Duration::from_secs(1));
    assert_eq!(f.reason, StopReason::Failed);
    assert!(f.message.contains("at least"), "{}", f.message);
    assert!(h.events().is_empty());
}

#[test]
fn calibration_uses_its_own_settings() {
    let h = Harness::new(game());
    h.update(spec(MouseButton::Middle, 500, 500, FOREVER));
    let mut cal = spec(L, 25, 15, RepeatConfig { enabled: true, count: 3 });
    cal.settle_ms = 50;
    let t = Instant::now();
    h.engine.send(Command::Calibrate(Arc::new(cal)));
    let f = h.wait_finished(Duration::from_secs(2));
    assert!(f.calibration);
    assert_eq!((f.reason, f.cycles, f.holds.count), (StopReason::Completed, 3, 3));
    assert_eq!(h.edges(), [(L, true), (L, false)].repeat(3));
    assert!(h.events()[0].0 - t >= Duration::from_millis(50), "settle delay before the first press");
    // the regular configuration is untouched
    h.engine.send(Command::Start);
    h.wait_log_len(7);
    h.engine.send(Command::Stop);
    let f = h.wait_finished(Duration::from_secs(1));
    assert!(!f.calibration);
    assert_eq!(h.edges()[6], (MouseButton::Middle, true));
}

#[test]
fn repeated_start_never_overlaps() {
    let h = Harness::new(game());
    h.update(spec(L, 5, 5, FOREVER));
    h.engine.send(Command::Start);
    h.wait_state(RunState::Running);
    for _ in 0..5 {
        h.engine.send(Command::Start);
        h.engine.send(Command::HoldPress);
    }
    std::thread::sleep(Duration::from_millis(60));
    h.engine.send(Command::Toggle);
    assert_eq!(h.wait_finished(Duration::from_millis(500)).reason, StopReason::Stopped);
    for (i, (b, d)) in h.edges().iter().enumerate() {
        assert_eq!((*b, *d), (L, i % 2 == 0), "edge {i}");
    }
}

#[test]
fn hold_to_run_stops_on_release() {
    let h = Harness::new(game());
    h.update(spec(L, 10, 10, FOREVER));
    h.engine.send(Command::HoldPress);
    h.wait_state(RunState::Running);
    std::thread::sleep(Duration::from_millis(50));
    h.engine.send(Command::HoldRelease);
    assert_eq!(h.wait_finished(Duration::from_millis(500)).reason, StopReason::Stopped);
    assert_eq!(h.edges().last(), Some(&(L, false)));
}

#[test]
fn emergency_stop_releases_the_button() {
    let h = Harness::new(game());
    h.update(spec(MouseButton::Right, 5_000, 60, FOREVER));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.engine.send(Command::Emergency);
    let f = h.wait_finished(Duration::from_millis(500));
    assert_eq!(f.reason, StopReason::Emergency);
    assert_eq!(h.edges(), [(MouseButton::Right, true), (MouseButton::Right, false)]);
}

#[test]
fn start_delay_is_cancellable() {
    let h = Harness::new(game());
    let mut s = spec(L, 10, 10, ONCE);
    s.hold.start_delay_ms = 10_000;
    h.update(s);
    h.engine.send(Command::Start);
    h.wait_state(RunState::Arming);
    h.engine.send(Command::Stop);
    assert_eq!(h.wait_finished(Duration::from_millis(200)).reason, StopReason::Stopped);
    assert!(h.events().is_empty());
}

#[test]
fn shutdown_mid_hold_releases() {
    let mut h = Harness::new(game());
    h.update(spec(L, 5_000, 60, FOREVER));
    h.engine.send(Command::Start);
    h.wait_log_len(1);
    h.engine.shutdown();
    assert_eq!(h.edges(), [(L, true), (L, false)]);
}

#[test]
fn updates_do_not_affect_a_running_cycle() {
    let h = Harness::new(game());
    h.update(spec(L, 5, 5, FOREVER));
    h.engine.send(Command::Start);
    h.wait_log_len(4);
    h.update(spec(MouseButton::Right, 5, 5, FOREVER));
    std::thread::sleep(Duration::from_millis(40));
    h.engine.send(Command::Stop);
    h.wait_finished(Duration::from_millis(500));
    assert!(h.edges().iter().all(|(b, _)| *b == L));
}

#[test]
fn a_window_closed_during_target_acquisition_is_not_an_error() {
    let h = Harness::new(game());
    h.world.lock().unwrap().closed = Some(game().id);
    h.update(spec(L, 10, 10, ONCE));
    h.engine.send(Command::Start);
    h.wait_state(RunState::WaitingForTarget);
    std::thread::sleep(Duration::from_millis(40));
    assert!(h.events().is_empty());
    // a new, live game window appears
    h.world.lock().unwrap().closed = None;
    assert_eq!(h.wait_finished(Duration::from_secs(1)).reason, StopReason::Completed);
}
