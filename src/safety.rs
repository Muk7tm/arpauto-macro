//! Input-safety building blocks: focus and pointer tracking with the target-window policy,
//! bookkeeping of macro-held buttons, the single-instance lock, and termination signals.

use crate::keys::MouseButton;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{self, Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Window};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;

/// Identity of a top-level window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: Option<u32>,
    /// WM_CLASS instance and class joined with a space, e.g. "sober org.vinegarhq.Sober".
    pub class: String,
}

/// Source of "which window has focus" and "which window is under the pointer".
pub trait FocusProbe {
    fn active(&mut self) -> Result<Option<WindowInfo>, String>;
    /// Top-level ancestor (window-manager frame or the window itself) of `window`;
    /// `None` if the window no longer exists.
    fn top_level_of(&mut self, window: u32) -> Result<Option<u32>, String>;
    /// Top-level window currently under the mouse pointer, if any.
    fn pointer_top_level(&mut self) -> Result<Option<u32>, String>;
}

/// Decides which windows the macro may send input to.
#[derive(Clone, Debug)]
pub struct TargetPolicy {
    /// Lock onto the first eligible window and only send input while it is focused.
    pub protection: bool,
    /// Optional case-insensitive WM_CLASS substring (only used with protection).
    pub class_filter: String,
    pub own_pid: u32,
    /// Lowercase substring identifying this application's own window class.
    pub own_class: &'static str,
}

impl TargetPolicy {
    pub fn is_own(&self, w: &WindowInfo) -> bool {
        w.pid == Some(self.own_pid) || w.class.to_lowercase().contains(self.own_class)
    }

    /// May the macro start sending input to `w`? This application's own window never is.
    pub fn eligible(&self, w: &WindowInfo) -> bool {
        if self.is_own(w) {
            return false;
        }
        if self.protection && !self.class_filter.trim().is_empty() {
            return w.class.to_lowercase().contains(&self.class_filter.trim().to_lowercase());
        }
        true
    }
}

/// Focus tracking over a dedicated X connection. `_NET_ACTIVE_WINDOW` changes are observed
/// through PropertyNotify events, so a normal check costs no round trip; a periodic refresh
/// guards against missed events and window managers without EWMH support.
pub struct XFocus {
    conn: RustConnection,
    root: Window,
    net_active_window: Atom,
    net_wm_pid: Atom,
    cached: Option<WindowInfo>,
    last_query: Option<Instant>,
    dirty: bool,
}

const FOCUS_REFRESH: Duration = Duration::from_millis(250);

/// The window disappeared between two requests (it was closed). Not an error for us.
fn is_gone(e: &ReplyError) -> bool {
    matches!(e, ReplyError::X11Error(x) if matches!(x.error_kind, ErrorKind::Window | ErrorKind::Drawable))
}

fn x_error(e: impl std::fmt::Display) -> String {
    format!("X server request failed: {e}")
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, String> {
    conn.intern_atom(false, name.as_bytes())
        .map_err(|e| e.to_string())?
        .reply()
        .map(|r| r.atom)
        .map_err(|e| e.to_string())
}

impl XFocus {
    pub fn connect() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("Cannot connect to the X server: {e}"))?;
        let root = conn.setup().roots[screen].root;
        let net_active_window = intern(&conn, "_NET_ACTIVE_WINDOW")?;
        let net_wm_pid = intern(&conn, "_NET_WM_PID")?;
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        Ok(XFocus { conn, root, net_active_window, net_wm_pid, cached: None, last_query: None, dirty: true })
    }

    pub fn conn(&self) -> &RustConnection {
        &self.conn
    }

    fn drain_events(&mut self) -> Result<(), String> {
        while let Some(ev) = self.conn.poll_for_event().map_err(|e| format!("X connection lost: {e}"))? {
            if let Event::PropertyNotify(p) = ev {
                if p.window == self.root && p.atom == self.net_active_window {
                    self.dirty = true;
                }
            }
        }
        Ok(())
    }

    fn query(&self) -> Result<Option<WindowInfo>, String> {
        match active_window_id(&self.conn, self.root, self.net_active_window)? {
            Some(id) => window_info(&self.conn, id, self.net_wm_pid),
            None => Ok(None),
        }
    }

    /// Force the next `active()` call to query the server.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }
}

impl FocusProbe for XFocus {
    fn active(&mut self) -> Result<Option<WindowInfo>, String> {
        self.drain_events()?;
        let stale = self.last_query.is_none_or(|t| t.elapsed() >= FOCUS_REFRESH);
        if self.dirty || stale {
            self.cached = self.query()?;
            self.last_query = Some(Instant::now());
            self.dirty = false;
        }
        Ok(self.cached.clone())
    }

    fn top_level_of(&mut self, window: u32) -> Result<Option<u32>, String> {
        let mut w = window;
        // Reparenting window managers nest clients a few levels deep; cap the walk.
        for _ in 0..16 {
            let tree = match self.conn.query_tree(w).map_err(x_error)?.reply() {
                Ok(t) => t,
                Err(e) if is_gone(&e) => return Ok(None),
                Err(e) => return Err(x_error(e)),
            };
            if tree.parent == self.root || tree.parent == x11rb::NONE {
                return Ok(Some(w));
            }
            w = tree.parent;
        }
        Ok(Some(w))
    }

    fn pointer_top_level(&mut self) -> Result<Option<u32>, String> {
        let p = self.conn.query_pointer(self.root).map_err(x_error)?.reply().map_err(x_error)?;
        Ok((p.same_screen && p.child != x11rb::NONE).then_some(p.child))
    }
}

/// Currently active top-level window: `_NET_ACTIVE_WINDOW`, else the core input focus.
pub fn active_window_id(conn: &RustConnection, root: Window, net_active_window: Atom) -> Result<Option<u32>, String> {
    let reply = conn
        .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)
        .map_err(x_error)?
        .reply()
        .map_err(x_error)?;
    if let Some(id) = reply.value32().and_then(|mut v| v.next()) {
        return Ok((id != 0).then_some(id));
    }
    let focus = conn.get_input_focus().map_err(x_error)?.reply().map_err(x_error)?.focus;
    // 0 = None, 1 = PointerRoot
    Ok((focus > 1 && focus != root).then_some(focus))
}

/// Identity of window `id`, or `None` if it no longer exists.
pub fn window_info(conn: &RustConnection, id: u32, net_wm_pid: Atom) -> Result<Option<WindowInfo>, String> {
    let pid = match conn.get_property(false, id, net_wm_pid, AtomEnum::CARDINAL, 0, 1).map_err(x_error)?.reply() {
        Ok(r) => r.value32().and_then(|mut v| v.next()),
        Err(e) if is_gone(&e) => return Ok(None),
        Err(e) => return Err(x_error(e)),
    };
    let class = match conn.get_property(false, id, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256).map_err(x_error)?.reply() {
        Ok(r) => r
            .value
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        Err(e) if is_gone(&e) => return Ok(None),
        Err(e) => return Err(x_error(e)),
    };
    Ok(Some(WindowInfo { id, pid, class }))
}

/// Look up the WM_CLASS of whatever window is focused (used by "Detect" in settings).
pub fn focused_window_class(conn: &RustConnection, root: Window) -> Result<Option<WindowInfo>, String> {
    let net_active = intern(conn, "_NET_ACTIVE_WINDOW")?;
    let net_pid = intern(conn, "_NET_WM_PID")?;
    match active_window_id(conn, root, net_active)? {
        Some(id) => window_info(conn, id, net_pid),
        None => Ok(None),
    }
}

/// Mouse buttons the macro currently holds down, in press order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HeldButtons {
    buttons: Vec<MouseButton>,
}

impl HeldButtons {
    pub fn down(&mut self, b: MouseButton) {
        if !self.buttons.contains(&b) {
            self.buttons.push(b);
        }
    }
    pub fn up(&mut self, b: MouseButton) {
        self.buttons.retain(|x| *x != b);
    }
    pub fn is_empty(&self) -> bool {
        self.buttons.is_empty()
    }
    /// Take everything that is held, most recently pressed first.
    pub fn drain_reverse(&mut self) -> Vec<MouseButton> {
        let mut b = std::mem::take(&mut self.buttons);
        b.reverse();
        b
    }
}

/// Exclusive per-user lock preventing two instances from fighting over hotkeys and input.
pub struct InstanceLock {
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn acquire(app: &str) -> Result<InstanceLock, String> {
        let path = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir).join(format!("{app}.lock")),
            // SAFETY: getuid has no preconditions.
            None => std::env::temp_dir().join(format!("{app}-{}.lock", unsafe { libc::getuid() })),
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("Cannot create lock file {}: {e}", path.display()))?;
        // SAFETY: flock on a valid, owned fd.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!("{app} is already running"));
        }
        Ok(InstanceLock { _file: file, path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Signals that trigger an orderly shutdown (input release, config save).
const TERMINATION_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Block termination signals in the calling thread (and every thread spawned after it), so
/// they are only delivered to the watcher thread. Call at the very start of `main`.
pub fn block_termination_signals() {
    // SAFETY: sigset manipulation on a local, zero-initialised set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in TERMINATION_SIGNALS {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Spawn a thread that waits for a termination signal and then runs `on_signal` once.
pub fn spawn_signal_watcher(on_signal: impl FnOnce(i32) + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new().name("signals".into()).spawn(move || {
        // SAFETY: sigwait on a set of signals that are blocked in this thread.
        let sig = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in TERMINATION_SIGNALS {
                libc::sigaddset(&mut set, s);
            }
            let mut sig: libc::c_int = 0;
            if libc::sigwait(&set, &mut sig) != 0 {
                return;
            }
            sig
        };
        tracing::info!("received signal {sig}, shutting down");
        on_signal(sig);
    })?;
    Ok(())
}

/// X modifier masks for core-protocol grabs, derived from the server's modifier mapping.
#[derive(Clone, Copy, Debug)]
pub struct ModMasks {
    pub shift: u16,
    pub ctrl: u16,
    pub alt: u16,
    pub super_: u16,
    /// Masks to ignore when matching hotkeys (CapsLock, NumLock, ScrollLock).
    pub ignored: [u16; 3],
}

impl ModMasks {
    pub fn load(conn: &RustConnection, keymap: &crate::input::Keymap) -> ModMasks {
        use crate::keys::*;
        let mut m = ModMasks {
            shift: u16::from(xproto::ModMask::SHIFT),
            ctrl: u16::from(xproto::ModMask::CONTROL),
            alt: u16::from(xproto::ModMask::M1),
            super_: u16::from(xproto::ModMask::M4),
            ignored: [u16::from(xproto::ModMask::LOCK), u16::from(xproto::ModMask::M2), 0],
        };
        let Ok(reply) = conn.get_modifier_mapping().map_err(|_| ()).and_then(|c| c.reply().map_err(|_| ())) else {
            return m;
        };
        let per = reply.keycodes.len() / 8;
        if per == 0 {
            return m;
        }
        let mask_of = |syms: &[u32]| -> Option<u16> {
            (0..8).find_map(|i| {
                let kcs = &reply.keycodes[i * per..(i + 1) * per];
                kcs.iter().any(|kc| *kc != 0 && syms.contains(&keymap.keysym(*kc, 0))).then_some(1u16 << i)
            })
        };
        if let Some(v) = mask_of(&[XK_ALT_L, XK_ALT_R, XK_META_L, XK_META_R]) {
            m.alt = v;
        }
        if let Some(v) = mask_of(&[XK_SUPER_L, XK_SUPER_R]) {
            m.super_ = v;
        }
        if let Some(v) = mask_of(&[XK_NUM_LOCK]) {
            m.ignored[1] = v;
        }
        m.ignored[2] = mask_of(&[XK_SCROLL_LOCK]).unwrap_or(0);
        m
    }

    pub fn for_mods(&self, mods: crate::keys::Mods) -> u16 {
        use crate::keys::Mods;
        let mut v = 0;
        if mods.contains(Mods::SHIFT) {
            v |= self.shift;
        }
        if mods.contains(Mods::CTRL) {
            v |= self.ctrl;
        }
        if mods.contains(Mods::ALT) {
            v |= self.alt;
        }
        if mods.contains(Mods::SUPER) {
            v |= self.super_;
        }
        v
    }

    pub fn relevant(&self) -> u16 {
        self.shift | self.ctrl | self.alt | self.super_
    }

    pub fn numlock(&self) -> u16 {
        self.ignored[1]
    }

    /// Every combination of ignored lock masks (grab once per combination).
    pub fn ignored_combinations(&self) -> Vec<u16> {
        let masks: Vec<u16> = self.ignored.iter().copied().filter(|m| *m != 0).collect();
        let mut out = Vec::new();
        for bits in 0..(1u32 << masks.len()) {
            let mut v = 0;
            for (i, m) in masks.iter().enumerate() {
                if bits & (1 << i) != 0 {
                    v |= *m;
                }
            }
            out.push(v);
        }
        out
    }

    /// Modifiers set in an event state, as `Mods`.
    pub fn mods_from_state(&self, state: u16) -> crate::keys::Mods {
        use crate::keys::Mods;
        let mut m = Mods::NONE;
        if state & self.shift != 0 {
            m.insert(Mods::SHIFT);
        }
        if state & self.ctrl != 0 {
            m.insert(Mods::CTRL);
        }
        if state & self.alt != 0 {
            m.insert(Mods::ALT);
        }
        if state & self.super_ != 0 {
            m.insert(Mods::SUPER);
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(protection: bool, filter: &str) -> TargetPolicy {
        TargetPolicy { protection, class_filter: filter.into(), own_pid: 42, own_class: "arpauto-macro" }
    }

    fn win(pid: Option<u32>, class: &str) -> WindowInfo {
        WindowInfo { id: 7, pid, class: class.into() }
    }

    #[test]
    fn own_window_is_never_eligible() {
        assert!(!policy(false, "").eligible(&win(Some(42), "whatever")));
        assert!(!policy(true, "").eligible(&win(None, "arpauto-macro Arpauto-macro")));
        assert!(policy(true, "").eligible(&win(Some(1), "sober org.vinegarhq.Sober")));
    }

    #[test]
    fn class_filter_is_case_insensitive_substring() {
        let p = policy(true, " Sober ");
        assert!(p.eligible(&win(Some(1), "sober org.vinegarhq.Sober")));
        assert!(!p.eligible(&win(Some(1), "code Code")));
        // filter is ignored without focus protection
        assert!(policy(false, "sober").eligible(&win(Some(1), "code Code")));
    }

    #[test]
    fn held_buttons_release_in_reverse_order() {
        let mut h = HeldButtons::default();
        h.down(MouseButton::Left);
        h.down(MouseButton::Right);
        h.down(MouseButton::Left);
        h.up(MouseButton::Middle);
        assert_eq!(h.drain_reverse(), [MouseButton::Right, MouseButton::Left]);
        assert!(h.is_empty());
    }

    #[test]
    fn ignored_mask_combinations() {
        let m = ModMasks { shift: 1, ctrl: 4, alt: 8, super_: 64, ignored: [2, 16, 0] };
        let mut c = m.ignored_combinations();
        c.sort();
        assert_eq!(c, [0, 2, 16, 18]);
    }
}
