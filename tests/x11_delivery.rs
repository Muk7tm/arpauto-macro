//! End-to-end tests against a real X server. Ignored by default because they need an X11
//! session, take keyboard focus and briefly move the pointer. Run them with:
//!
//! ```text
//! cargo test --test x11_delivery -- --ignored --test-threads=1
//! ```
//!
//! and do not touch the keyboard or mouse for a few seconds. Safety: the worker runs with
//! focus protection restricted to the WM_CLASS of a window created by the test and requires
//! the pointer to be over that window, so no click can land anywhere else. The pointer is
//! moved back afterwards.

use arpauto_macro::config::{ActivationMode, HoldSettings, HoldTiming, Hotkeys, InputConfig, RepeatConfig};
use arpauto_macro::hold_engine::{Command, EngineEvent, EngineHandle, RunSpec, StopReason, X11Platform};
use arpauto_macro::hotkeys::{HotkeyCommand, HotkeyEvent, HotkeyHandle};
use arpauto_macro::input::{BackendKind, Keymap};
use arpauto_macro::keys::MouseButton;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const TEST_CLASS: &str = "arpauto-x11-test";

struct TestWindow {
    conn: RustConnection,
    root: Window,
    win: Window,
    pointer_home: (i16, i16),
}

impl TestWindow {
    /// Create, focus and put the pointer over a test window.
    fn open() -> TestWindow {
        let (conn, screen) = x11rb::connect(None).expect("X11 display required");
        let s = &conn.setup().roots[screen];
        let (root, visual, black) = (s.root, s.root_visual, s.black_pixel);
        let win = conn.generate_id().unwrap();
        let mask = EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::ENTER_WINDOW
            | EventMask::STRUCTURE_NOTIFY;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            220,
            220,
            320,
            220,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new().background_pixel(black).event_mask(mask),
        )
        .unwrap();
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"arpauto-x11-test\0ArpautoX11Test\0")
            .unwrap();
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"ARPAuto X11 delivery test").unwrap();
        conn.map_window(win).unwrap();
        conn.flush().unwrap();
        let home = conn.query_pointer(root).unwrap().reply().unwrap();
        let tw = TestWindow { conn, root, win, pointer_home: (home.root_x, home.root_y) };
        tw.activate();
        tw.pointer_into();
        tw
    }

    fn atom(&self, name: &str) -> Atom {
        self.conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
    }

    fn active_window(&self) -> Option<Window> {
        let a = self.atom("_NET_ACTIVE_WINDOW");
        let r = self.conn.get_property(false, self.root, a, AtomEnum::WINDOW, 0, 1).unwrap().reply().unwrap();
        r.value32().and_then(|mut v| v.next())
    }

    fn activate(&self) {
        let net_active = self.atom("_NET_ACTIVE_WINDOW");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let ev = ClientMessageEvent::new(32, self.win, net_active, [2u32, 0, 0, 0, 0]);
            self.conn
                .send_event(false, self.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev)
                .unwrap();
            self.conn.flush().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            if self.active_window() == Some(self.win) {
                return;
            }
        }
        panic!("the window manager did not focus the test window; nothing was sent");
    }

    fn pointer_into(&self) {
        self.conn.warp_pointer(x11rb::NONE, self.win, 0, 0, 0, 0, 160, 110).unwrap();
        self.conn.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            while let Some(ev) = self.conn.poll_for_event().unwrap() {
                if matches!(ev, Event::EnterNotify(e) if e.event == self.win) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.restore_pointer();
        panic!("pointer is not over the test window; nothing was sent");
    }

    fn restore_pointer(&self) {
        let _ = self.conn.warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, self.pointer_home.0, self.pointer_home.1);
        let _ = self.conn.flush();
    }

    /// Button events (button, down, server time) received by the test window.
    fn collect(&self, settle: Duration) -> Vec<(u8, bool, u32)> {
        let end = Instant::now() + settle;
        let mut out = Vec::new();
        while Instant::now() < end {
            while let Some(ev) = self.conn.poll_for_event().unwrap() {
                match ev {
                    Event::ButtonPress(e) if e.event == self.win => out.push((e.detail, true, e.time)),
                    Event::ButtonRelease(e) if e.event == self.win => out.push((e.detail, false, e.time)),
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        out
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.restore_pointer();
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

fn spec(backend: BackendKind, button: MouseButton, hold_ms: u32, release_ms: u32, repeat: RepeatConfig) -> RunSpec {
    RunSpec {
        hold: HoldSettings { button, timing: HoldTiming { hold_ms, release_ms }, repeat, start_delay_ms: 0 },
        input: InputConfig { backend, target_class: TEST_CLASS.into(), ..InputConfig::default() },
        hotkeys: Hotkeys::default(),
        settle_ms: 0,
    }
}

fn spawn_engine() -> (EngineHandle, mpsc::Receiver<EngineEvent>) {
    let (tx, rx) = mpsc::channel();
    let engine = EngineHandle::spawn(X11Platform::default, move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();
    (engine, rx)
}

fn wait_finished(rx: &mpsc::Receiver<EngineEvent>, timeout: Duration) -> (StopReason, String) {
    let deadline = Instant::now() + timeout;
    loop {
        if let EngineEvent::Finished { reason, message, .. } =
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("engine finished")
        {
            return (reason, message);
        }
    }
}

fn holds_reach_window(backend: BackendKind) {
    let tw = TestWindow::open();
    tw.collect(Duration::from_millis(50));
    let (mut engine, rx) = spawn_engine();
    engine.send(Command::Update(Arc::new(spec(backend, MouseButton::Left, 120, 60, RepeatConfig { enabled: true, count: 4 }))));
    engine.send(Command::Start);
    let (reason, message) = wait_finished(&rx, Duration::from_secs(5));
    let events = tw.collect(Duration::from_millis(150));
    engine.shutdown();
    assert_eq!(reason, StopReason::Completed, "{message}");

    let edges: Vec<(u8, bool)> = events.iter().map(|(b, d, _)| (*b, *d)).collect();
    assert_eq!(edges, [(1, true), (1, false)].repeat(4), "one press and one release per hold");
    let mut holds = Vec::new();
    let mut releases = Vec::new();
    for (i, w) in events.windows(2).enumerate() {
        let dt = w[1].2.wrapping_sub(w[0].2);
        if i % 2 == 0 { holds.push(dt) } else { releases.push(dt) }
    }
    println!("[{backend:?}] server-measured holds (ms): {holds:?}, releases (ms): {releases:?}");
    assert!(holds.iter().all(|h| (119..=135).contains(h)), "holds {holds:?}");
    assert!(releases.iter().all(|r| (59..=75).contains(r)), "releases {releases:?}");
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn xtest_holds_reach_the_window_with_server_measured_timing() {
    holds_reach_window(BackendKind::Xtest);
}

#[test]
#[ignore = "needs an X11 session and write access to /dev/uinput; run with --ignored"]
fn uinput_holds_reach_the_window_with_server_measured_timing() {
    holds_reach_window(BackendKind::Uinput);
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn pointer_leaving_the_target_stops_and_releases() {
    let tw = TestWindow::open();
    tw.collect(Duration::from_millis(50));
    let (mut engine, rx) = spawn_engine();
    engine.send(Command::Update(Arc::new(spec(BackendKind::Xtest, MouseButton::Right, 5_000, 60, RepeatConfig::default()))));
    engine.send(Command::Start);
    let pressed = tw.collect(Duration::from_millis(150));
    assert_eq!(pressed.iter().map(|(b, d, _)| (*b, *d)).collect::<Vec<_>>(), [(3, true)]);
    // Move the pointer out. The release still goes to the test window: X keeps an implicit
    // pointer grab on the window that received the press.
    tw.conn.warp_pointer(x11rb::NONE, tw.win, 0, 0, 0, 0, 900, 900).unwrap();
    tw.conn.flush().unwrap();
    let (reason, message) = wait_finished(&rx, Duration::from_secs(2));
    let released = tw.collect(Duration::from_millis(100));
    engine.shutdown();
    assert_eq!(reason, StopReason::FocusLost, "{message}");
    assert!(message.contains("pointer"), "{message}");
    assert_eq!(released.iter().map(|(b, d, _)| (*b, *d)).collect::<Vec<_>>(), [(3, false)]);
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn hotkey_toggle_starts_and_stops() {
    let tw = TestWindow::open();
    let (mut engine, erx) = spawn_engine();
    let (htx, hrx) = mpsc::channel();
    let mut hotkeys = HotkeyHandle::spawn(engine.sender(), move |ev| {
        let _ = htx.send(ev);
    })
    .unwrap();
    let hk = Hotkeys::default();
    hotkeys.send(HotkeyCommand::Configure { hotkeys: hk, mode: ActivationMode::Toggle });
    match hrx.recv_timeout(Duration::from_secs(2)).expect("registration result") {
        HotkeyEvent::Registered { ok: true, .. } => {}
        other => panic!("hotkeys not registered (is ARPAuto Macro running?): {other:?}"),
    }
    engine.send(Command::Update(Arc::new(spec(BackendKind::Xtest, MouseButton::Left, 20, 20, RepeatConfig::default()))));
    tw.collect(Duration::from_millis(50));

    let keymap = Keymap::load(&tw.conn).unwrap();
    let f7 = keymap.keycodes_for(hk.start_stop.key)[0];
    let tap = || {
        tw.conn.xtest_fake_input(KEY_PRESS_EVENT, f7, 0, tw.root, 0, 0, 0).unwrap();
        tw.conn.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        tw.conn.xtest_fake_input(KEY_RELEASE_EVENT, f7, 0, tw.root, 0, 0, 0).unwrap();
        tw.conn.flush().unwrap();
    };
    tap();
    let running = tw.collect(Duration::from_millis(300));
    assert!(running.iter().filter(|(_, d, _)| *d).count() >= 4, "holds started by F7: {running:?}");
    tap();
    let (reason, _) = wait_finished(&erx, Duration::from_secs(2));
    assert_eq!(reason, StopReason::Stopped);
    let all: Vec<_> = running.into_iter().chain(tw.collect(Duration::from_millis(100))).collect();
    let downs = all.iter().filter(|(_, d, _)| *d).count();
    let ups = all.iter().filter(|(_, d, _)| !*d).count();
    assert_eq!(downs, ups, "every press released (physical mouse input during the test also counts): {all:?}");
    hotkeys.shutdown();
    engine.shutdown();
}

#[test]
#[ignore = "needs an X11 session; run with --ignored"]
fn closing_the_target_window_stops_cleanly() {
    let tw = TestWindow::open();
    tw.collect(Duration::from_millis(50));
    let (mut engine, rx) = spawn_engine();
    engine.send(Command::Update(Arc::new(spec(BackendKind::Xtest, MouseButton::Left, 5_000, 60, RepeatConfig::default()))));
    engine.send(Command::Start);
    let pressed = tw.collect(Duration::from_millis(150));
    assert_eq!(pressed.iter().map(|(b, d, _)| (*b, *d)).collect::<Vec<_>>(), [(1, true)]);
    tw.conn.destroy_window(tw.win).unwrap();
    tw.conn.flush().unwrap();
    let (reason, message) = wait_finished(&rx, Duration::from_secs(2));
    engine.shutdown();
    assert_eq!(reason, StopReason::FocusLost, "a closed window is a lost target, not an error: {message}");
}
