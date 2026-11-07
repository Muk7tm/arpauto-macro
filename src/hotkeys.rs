//! Global hotkeys, key capture and window detection on a dedicated X11 thread.
//!
//! Hotkeys are passive key grabs on the root window (`XGrabKey`), registered once for every
//! combination of the lock modifiers (CapsLock/NumLock/ScrollLock) so they work regardless
//! of lock state.
//!
//! When a passive grab fires, the X server *actively grabs the whole keyboard* until the
//! hotkey is released. While that grab exists the game would receive none of the user's
//! keys (movement keys, for example) and sees a keyboard focus change. The service
//! therefore releases the grab immediately (`UngrabKeyboard`) and detects the physical
//! release of the hotkey by polling the server's key-state bitmap (`QueryKeymap`). That
//! one mechanism handles auto-repeat (repeated presses while held are ignored), toggle mode
//! and hold-to-run mode.

use crate::config::{ActivationMode, Hotkeys};
use crate::hold_engine::{self as engine, OWN_WINDOW_CLASS};
use crate::input::Keymap;
use crate::keys::{self, KeyCombo};
use crate::safety::{self, ModMasks, WindowInfo};
use std::os::fd::AsRawFd;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, GrabStatus, KeyPressEvent, ModMask, Window};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum HotkeyCommand {
    Configure { hotkeys: Hotkeys, mode: ActivationMode },
    /// Grab the keyboard and report the next key or combination pressed.
    Capture { purpose: String },
    CancelCapture,
    /// Report the focused window's class after `delay`.
    DetectWindow { delay: Duration },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HotkeyEvent {
    Registered { ok: bool, message: String },
    Captured { purpose: String, result: Result<KeyCombo, String> },
    Detected(Result<WindowInfo, String>),
    Fatal(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    StartStop,
    Emergency,
}

struct Binding {
    action: Action,
    keycodes: Vec<u8>,
    mods: u16,
}

struct Capture {
    purpose: String,
    deadline: Instant,
    pending_modifier: Option<keys::Keysym>,
}

struct Service {
    conn: RustConnection,
    root: Window,
    keymap: Keymap,
    masks: ModMasks,
    rx: Receiver<HotkeyCommand>,
    engine: Sender<engine::Command>,
    events: Box<dyn Fn(HotkeyEvent) + Send>,
    bindings: Vec<Binding>,
    mode: ActivationMode,
    /// Hotkey keys currently held down physically.
    held: Vec<(u8, Action)>,
    capture: Option<Capture>,
    detect_at: Option<Instant>,
}

fn x_err(e: impl std::fmt::Display) -> String {
    format!("X11 error: {e}")
}

impl Service {
    fn connect() -> Result<(RustConnection, Window, Keymap, ModMasks), String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("Cannot connect to the X server: {e}"))?;
        let root = conn.setup().roots[screen].root;
        let keymap = Keymap::load(&conn)?;
        let masks = ModMasks::load(&conn, &keymap);
        Ok((conn, root, keymap, masks))
    }

    fn new(
        (conn, root, keymap, masks): (RustConnection, Window, Keymap, ModMasks),
        rx: Receiver<HotkeyCommand>,
        engine: Sender<engine::Command>,
        events: Box<dyn Fn(HotkeyEvent) + Send>,
    ) -> Self {
        Service {
            conn,
            root,
            keymap,
            masks,
            rx,
            engine,
            events,
            bindings: Vec::new(),
            mode: ActivationMode::Toggle,
            held: Vec::new(),
            capture: None,
            detect_at: None,
        }
    }

    fn run(mut self) {
        loop {
            match self.step() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    tracing::error!("hotkey service stopped: {e}");
                    (self.events)(HotkeyEvent::Fatal(e));
                    break;
                }
            }
        }
        let _ = self.ungrab_all();
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }

    /// One iteration; returns false on shutdown.
    fn step(&mut self) -> Result<bool, String> {
        loop {
            match self.rx.try_recv() {
                Ok(HotkeyCommand::Shutdown) | Err(TryRecvError::Disconnected) => return Ok(false),
                Ok(cmd) => self.on_command(cmd)?,
                Err(TryRecvError::Empty) => break,
            }
        }
        // Round-trip work first, so events it buffers are drained below before sleeping.
        self.poll_held()?;
        self.check_timers()?;
        while let Some(ev) = self.conn.poll_for_event().map_err(x_err)? {
            self.on_event(ev)?;
        }
        self.conn.flush().map_err(x_err)?;
        let timeout_ms = if !self.held.is_empty() {
            10
        } else if self.capture.is_some() || self.detect_at.is_some() {
            20
        } else {
            50
        };
        let mut pfd = libc::pollfd { fd: self.conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the X socket.
        unsafe {
            libc::poll(&mut pfd, 1, timeout_ms);
        }
        Ok(true)
    }

    fn on_command(&mut self, cmd: HotkeyCommand) -> Result<(), String> {
        match cmd {
            HotkeyCommand::Configure { hotkeys, mode } => {
                self.mode = mode;
                let (ok, message) = match self.apply(&hotkeys) {
                    Ok(()) => (true, format!("{} start/stop · {} emergency stop", hotkeys.start_stop, hotkeys.emergency)),
                    Err(e) => (false, e),
                };
                (self.events)(HotkeyEvent::Registered { ok, message });
            }
            HotkeyCommand::Capture { purpose } => self.begin_capture(purpose)?,
            HotkeyCommand::CancelCapture => self.finish_capture(Err("Capture cancelled".into()))?,
            HotkeyCommand::DetectWindow { delay } => self.detect_at = Some(Instant::now() + delay),
            HotkeyCommand::Shutdown => {}
        }
        Ok(())
    }

    fn ungrab_all(&mut self) -> Result<(), String> {
        self.conn.ungrab_key(x11rb::protocol::xproto::Grab::ANY, self.root, ModMask::ANY).map_err(x_err)?;
        self.bindings.clear();
        self.held.clear();
        Ok(())
    }

    fn apply(&mut self, hotkeys: &Hotkeys) -> Result<(), String> {
        self.ungrab_all()?;
        if let Some(issue) = hotkeys.validate().into_iter().next() {
            return Err(issue);
        }
        self.keymap = Keymap::load(&self.conn)?;
        self.masks = ModMasks::load(&self.conn, &self.keymap);
        let mut problems = Vec::new();
        for (action, combo) in [(Action::StartStop, hotkeys.start_stop), (Action::Emergency, hotkeys.emergency)] {
            let keycodes = self.keymap.keycodes_for(combo.key);
            if keycodes.is_empty() {
                problems.push(format!("{combo} does not exist on this keyboard layout"));
                continue;
            }
            let mods = self.masks.for_mods(combo.mods);
            let mut grabbed = Vec::new();
            let mut conflict = false;
            for &kc in &keycodes {
                for ign in self.masks.ignored_combinations() {
                    let res = self
                        .conn
                        .grab_key(false, self.root, ModMask::from(mods | ign), kc, GrabMode::ASYNC, GrabMode::ASYNC)
                        .map_err(x_err)?
                        .check();
                    match res {
                        Ok(()) => {}
                        Err(ReplyError::X11Error(e)) if e.error_kind == ErrorKind::Access => conflict = true,
                        Err(e) => return Err(x_err(e)),
                    }
                }
                grabbed.push(kc);
            }
            if conflict {
                problems.push(format!("{combo} is already taken by another application"));
                for &kc in &grabbed {
                    for ign in self.masks.ignored_combinations() {
                        let _ = self.conn.ungrab_key(kc, self.root, ModMask::from(mods | ign));
                    }
                }
                continue;
            }
            self.bindings.push(Binding { action, keycodes: grabbed, mods });
        }
        self.conn.flush().map_err(x_err)?;
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    fn binding_for(&self, ev: &KeyPressEvent) -> Option<Action> {
        let state = u16::from(ev.state) & self.masks.relevant();
        self.bindings.iter().find(|b| b.mods == state && b.keycodes.contains(&ev.detail)).map(|b| b.action)
    }

    fn send(&self, cmd: engine::Command) {
        let _ = self.engine.send(cmd);
    }

    fn on_event(&mut self, ev: Event) -> Result<(), String> {
        match ev {
            Event::KeyPress(e) => {
                if self.capture.is_some() {
                    return self.capture_press(&e);
                }
                if let Some(action) = self.binding_for(&e) {
                    // Hand the keyboard straight back so synthetic input reaches the target.
                    self.conn.ungrab_keyboard(x11rb::CURRENT_TIME).map_err(x_err)?;
                    self.conn.flush().map_err(x_err)?;
                    if self.held.iter().any(|(kc, _)| *kc == e.detail) {
                        return Ok(()); // auto-repeat
                    }
                    self.held.push((e.detail, action));
                    match (action, self.mode) {
                        (Action::Emergency, _) => self.send(engine::Command::Emergency),
                        (Action::StartStop, ActivationMode::Toggle) => self.send(engine::Command::Toggle),
                        (Action::StartStop, ActivationMode::Hold) => self.send(engine::Command::HoldPress),
                    }
                }
            }
            Event::KeyRelease(e) => {
                if self.capture.is_some() {
                    return self.capture_release(&e);
                }
                self.released(e.detail);
            }
            Event::MappingNotify(_) => {
                self.keymap = Keymap::load(&self.conn)?;
                self.masks = ModMasks::load(&self.conn, &self.keymap);
            }
            Event::Error(e) => tracing::debug!("X error on hotkey connection: {e:?}"),
            _ => {}
        }
        Ok(())
    }

    fn released(&mut self, keycode: u8) {
        if let Some(pos) = self.held.iter().position(|(kc, _)| *kc == keycode) {
            let (_, action) = self.held.remove(pos);
            if action == Action::StartStop && self.mode == ActivationMode::Hold {
                self.send(engine::Command::HoldRelease);
            }
        }
    }

    /// Detect physical release of held hotkeys from the server's key bitmap.
    fn poll_held(&mut self) -> Result<(), String> {
        if self.held.is_empty() {
            return Ok(());
        }
        let keys = self.conn.query_keymap().map_err(x_err)?.reply().map_err(x_err)?.keys;
        let released: Vec<u8> = self
            .held
            .iter()
            .map(|(kc, _)| *kc)
            .filter(|kc| keys[usize::from(*kc / 8)] & (1 << (kc % 8)) == 0)
            .collect();
        for kc in released {
            self.released(kc);
        }
        Ok(())
    }

    fn check_timers(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if self.capture.as_ref().is_some_and(|c| now >= c.deadline) {
            self.finish_capture(Err("No key was pressed".into()))?;
        }
        if self.detect_at.is_some_and(|t| now >= t) {
            self.detect_at = None;
            let result = safety::focused_window_class(&self.conn, self.root).and_then(|w| match w {
                None => Err("No window is focused".to_string()),
                Some(w) if w.pid == Some(std::process::id()) || w.class.to_lowercase().contains(OWN_WINDOW_CLASS) => {
                    Err("Switch to the game window within 3 seconds".to_string())
                }
                Some(w) if w.class.is_empty() => Err("The focused window has no WM_CLASS".to_string()),
                Some(w) => Ok(w),
            });
            (self.events)(HotkeyEvent::Detected(result));
        }
        Ok(())
    }

    fn begin_capture(&mut self, purpose: String) -> Result<(), String> {
        if self.capture.is_some() {
            self.finish_capture(Err("Capture cancelled".into()))?;
        }
        let status = self
            .conn
            .grab_keyboard(false, self.root, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?
            .status;
        if status != GrabStatus::SUCCESS {
            (self.events)(HotkeyEvent::Captured {
                purpose,
                result: Err("Another application is holding the keyboard; try again".into()),
            });
            return Ok(());
        }
        self.keymap = Keymap::load(&self.conn)?;
        self.masks = ModMasks::load(&self.conn, &self.keymap);
        self.capture = Some(Capture { purpose, deadline: Instant::now() + CAPTURE_TIMEOUT, pending_modifier: None });
        Ok(())
    }

    fn finish_capture(&mut self, result: Result<KeyCombo, String>) -> Result<(), String> {
        let Some(cap) = self.capture.take() else { return Ok(()) };
        self.conn.ungrab_keyboard(x11rb::CURRENT_TIME).map_err(x_err)?;
        self.conn.flush().map_err(x_err)?;
        (self.events)(HotkeyEvent::Captured { purpose: cap.purpose, result });
        Ok(())
    }

    fn capture_press(&mut self, e: &KeyPressEvent) -> Result<(), String> {
        let state = u16::from(e.state);
        let sym = self.keymap.keysym_for_event(e.detail, state & self.masks.numlock() != 0);
        if sym == 0 {
            return Ok(());
        }
        if keys::is_modifier(sym) {
            if let Some(c) = self.capture.as_mut() {
                c.pending_modifier = Some(sym);
            }
            return Ok(());
        }
        let mods = self.masks.mods_from_state(state);
        if sym == keys::XK_ESCAPE && mods.is_empty() {
            return self.finish_capture(Err("Capture cancelled".into()));
        }
        self.finish_capture(Ok(KeyCombo::with(mods, sym)))
    }

    fn capture_release(&mut self, e: &KeyPressEvent) -> Result<(), String> {
        let sym = self.keymap.keysym(e.detail, 0);
        let pending = self.capture.as_ref().and_then(|c| c.pending_modifier);
        if pending.is_some_and(|p| p == sym) {
            // A modifier pressed and released on its own: capture the modifier key itself.
            let mut mods = self.masks.mods_from_state(u16::from(e.state));
            if let Some(m) = keys::modifier_of(sym) {
                mods.remove(m);
            }
            return self.finish_capture(Ok(KeyCombo::with(mods, sym)));
        }
        Ok(())
    }
}

/// Owner-side handle of the hotkey thread.
pub struct HotkeyHandle {
    tx: Sender<HotkeyCommand>,
    join: Option<JoinHandle<()>>,
}

impl HotkeyHandle {
    pub fn spawn(engine: Sender<engine::Command>, events: impl Fn(HotkeyEvent) + Send + 'static) -> std::io::Result<HotkeyHandle> {
        let (tx, rx) = mpsc::channel();
        let events: Box<dyn Fn(HotkeyEvent) + Send> = Box::new(events);
        let join = std::thread::Builder::new().name("hotkeys".into()).spawn(move || match Service::connect() {
            Ok(parts) => Service::new(parts, rx, engine, events).run(),
            Err(e) => {
                tracing::error!("hotkeys unavailable: {e}");
                events(HotkeyEvent::Fatal(format!("Hotkeys unavailable: {e}")));
            }
        })?;
        Ok(HotkeyHandle { tx, join: Some(join) })
    }

    pub fn send(&self, cmd: HotkeyCommand) {
        if self.tx.send(cmd).is_err() {
            tracing::warn!("hotkey service is not running");
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(HotkeyCommand::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
