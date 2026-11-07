//! Mouse-button injection backends and the X keymap snapshot used by the hotkey service.
//!
//! * **XTest** (default): synthetic button events through the XTEST extension. No
//!   privileges needed. Each event is a checked request, so the worker only continues once
//!   the X server has processed it; hold durations are measured from that point.
//! * **uinput** (optional): a virtual kernel mouse. Requires write access to `/dev/uinput`.

use crate::keys::{self, Keysym, MouseButton};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputError(pub String);

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

impl InputError {
    pub fn new(msg: impl Into<String>) -> Self {
        InputError(msg.into())
    }
}

pub type InputResult<T = ()> = Result<T, InputError>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    #[default]
    Xtest,
    Uinput,
}

impl BackendKind {
    pub const ALL: [BackendKind; 2] = [BackendKind::Xtest, BackendKind::Uinput];
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Xtest => "XTest",
            BackendKind::Uinput => "uinput",
        }
    }
    pub fn index(self) -> usize {
        BackendKind::ALL.iter().position(|b| *b == self).unwrap_or(0)
    }
    pub fn from_index(i: usize) -> Self {
        BackendKind::ALL.get(i).copied().unwrap_or_default()
    }
}

/// Something that can press and release mouse buttons.
pub trait MouseBackend: Send {
    fn button(&mut self, button: MouseButton, down: bool) -> InputResult;
    fn kind(&self) -> BackendKind;
    /// Whether events are delivered yet. A freshly created uinput device is only usable once
    /// the X server has opened it; until then its events are silently dropped.
    fn is_ready(&mut self) -> InputResult<bool>;
}

/// Snapshot of the X server's core keyboard mapping (used for hotkeys and key capture).
#[derive(Clone, Debug)]
pub struct Keymap {
    min_keycode: u8,
    per_keycode: usize,
    syms: Vec<u32>,
}

impl Keymap {
    pub fn load<C: Connection>(conn: &C) -> Result<Self, String> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max.saturating_sub(min).saturating_add(1);
        let reply = conn
            .get_keyboard_mapping(min, count)
            .map_err(|e| format!("GetKeyboardMapping failed: {e}"))?
            .reply()
            .map_err(|e| format!("GetKeyboardMapping failed: {e}"))?;
        Ok(Keymap { min_keycode: min, per_keycode: usize::from(reply.keysyms_per_keycode).max(1), syms: reply.keysyms })
    }

    /// Build from raw data (used by tests).
    pub fn from_raw(min_keycode: u8, per_keycode: usize, syms: Vec<u32>) -> Self {
        Keymap { min_keycode, per_keycode: per_keycode.max(1), syms }
    }

    fn keycode_count(&self) -> usize {
        self.syms.len() / self.per_keycode
    }

    /// Keysym at `col` (0 = base level, 1 = shifted) of `keycode`.
    pub fn keysym(&self, keycode: u8, col: usize) -> Keysym {
        if keycode < self.min_keycode || col >= self.per_keycode {
            return 0;
        }
        let idx = usize::from(keycode - self.min_keycode) * self.per_keycode + col;
        self.syms.get(idx).copied().unwrap_or(0)
    }

    /// Keysym produced by a key press given the NumLock state (for key capture).
    pub fn keysym_for_event(&self, keycode: u8, numlock_on: bool) -> Keysym {
        let base = self.keysym(keycode, 0);
        if numlock_on && keys::is_keypad(base) {
            let shifted = self.keysym(keycode, 1);
            if keys::is_keypad(shifted) {
                return shifted;
            }
        }
        base
    }

    /// Keycodes whose base level is `sym` (falls back to the shifted level).
    pub fn keycodes_for(&self, sym: Keysym) -> Vec<u8> {
        for col in 0..2.min(self.per_keycode) {
            let found: Vec<u8> = (0..self.keycode_count())
                .filter(|i| self.syms[i * self.per_keycode + col] == sym)
                .filter_map(|i| u8::try_from(i + usize::from(self.min_keycode)).ok())
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }
}

// ---------------------------------------------------------------------------------------
// XTest
// ---------------------------------------------------------------------------------------

pub struct XTestBackend {
    conn: RustConnection,
    root: Window,
}

impl XTestBackend {
    pub fn connect() -> InputResult<Self> {
        let (conn, screen) =
            x11rb::connect(None).map_err(|e| InputError::new(format!("Cannot connect to the X server: {e}")))?;
        let root = conn.setup().roots[screen].root;
        conn.extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(|e| InputError::new(format!("X connection error: {e}")))?
            .ok_or_else(|| InputError::new("The X server does not provide the XTEST extension"))?;
        conn.xtest_get_version(2, 2)
            .map_err(|e| InputError::new(format!("XTEST query failed: {e}")))?
            .reply()
            .map_err(|e| InputError::new(format!("XTEST query failed: {e}")))?;
        Ok(XTestBackend { conn, root })
    }
}

impl MouseBackend for XTestBackend {
    fn button(&mut self, button: MouseButton, down: bool) -> InputResult {
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.conn
            .xtest_fake_input(kind, button.x11_button(), x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| InputError::new(format!("XTEST request failed: {e}")))?
            .check()
            .map_err(|e| InputError::new(format!("X server rejected a synthetic event: {e}")))
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Xtest
    }

    fn is_ready(&mut self) -> InputResult<bool> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------------------
// uinput (virtual mouse)
// ---------------------------------------------------------------------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const BUS_VIRTUAL: u16 = 0x06;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((b'U' as u64) << 8) | nr
}
const UI_SET_EVBIT: u64 = ioc(1, 100, 4);
const UI_SET_KEYBIT: u64 = ioc(1, 101, 4);
const UI_SET_RELBIT: u64 = ioc(1, 102, 4);
const UI_DEV_SETUP: u64 = ioc(1, 3, std::mem::size_of::<UinputSetup>() as u64);
const UI_DEV_CREATE: u64 = ioc(0, 1, 0);
const UI_DEV_DESTROY: u64 = ioc(0, 2, 0);

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct RawInputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

/// Extra time after the device shows up in the X device list before it is used.
const UINPUT_SETTLE: Duration = Duration::from_millis(100);

pub struct UinputBackend {
    file: File,
    name: String,
    /// Connection used to watch for the device in the X server's device list.
    watch: Option<RustConnection>,
    seen_at: Option<Instant>,
}

impl UinputBackend {
    pub fn create(base_name: &str) -> InputResult<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        // A unique name lets readiness detection find exactly this device.
        let name = format!("{base_name} {}-{}", std::process::id(), SERIAL.fetch_add(1, Ordering::Relaxed));

        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")
            .map_err(|e| {
                InputError::new(format!(
                    "Cannot open /dev/uinput ({e}). Add your user to the 'input' group or install the udev rule from the README."
                ))
            })?;
        let fd = file.as_raw_fd();
        let ioctl_int = |req: u64, val: i32| -> InputResult {
            // SAFETY: valid fd; these uinput requests take an int argument by value.
            let rc = unsafe { libc::ioctl(fd, req as _, val as libc::c_int) };
            if rc < 0 {
                Err(InputError::new(format!("uinput ioctl failed: {}", std::io::Error::last_os_error())))
            } else {
                Ok(())
            }
        };
        ioctl_int(UI_SET_EVBIT, i32::from(EV_KEY))?;
        ioctl_int(UI_SET_EVBIT, i32::from(EV_SYN))?;
        for b in MouseButton::ALL {
            ioctl_int(UI_SET_KEYBIT, i32::from(b.evdev_code()))?;
        }
        // Relative axes make libinput classify the device as a pointer.
        ioctl_int(UI_SET_EVBIT, i32::from(EV_REL))?;
        ioctl_int(UI_SET_RELBIT, i32::from(REL_X))?;
        ioctl_int(UI_SET_RELBIT, i32::from(REL_Y))?;

        let mut setup = UinputSetup {
            id: InputId { bustype: BUS_VIRTUAL, vendor: 0x1d6b, product: 0x4150, version: 1 },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(setup.name.len() - 1);
        setup.name[..n].copy_from_slice(&bytes[..n]);
        // SAFETY: setup is a properly laid out uinput_setup that lives across the call.
        if unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &setup as *const UinputSetup) } < 0 {
            return Err(InputError::new(format!("UI_DEV_SETUP failed: {}", std::io::Error::last_os_error())));
        }
        // SAFETY: argument-less request on a valid fd.
        if unsafe { libc::ioctl(fd, UI_DEV_CREATE as _) } < 0 {
            return Err(InputError::new(format!("UI_DEV_CREATE failed: {}", std::io::Error::last_os_error())));
        }
        tracing::info!("created uinput device \"{name}\"");
        Ok(UinputBackend { file, name, watch: None, seen_at: None })
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) -> InputResult {
        let ev = RawInputEvent { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, kind, code, value };
        // SAFETY: RawInputEvent is a plain repr(C) struct; we view it as bytes for write().
        let bytes = unsafe {
            std::slice::from_raw_parts((&ev as *const RawInputEvent).cast::<u8>(), std::mem::size_of::<RawInputEvent>())
        };
        self.file.write_all(bytes).map_err(|e| InputError::new(format!("uinput write failed: {e}")))
    }

    /// Has the X server opened the device (it appears in the XInput device list)?
    fn listed_by_x(&mut self) -> InputResult<bool> {
        use x11rb::protocol::xinput::ConnectionExt as _;
        if self.watch.is_none() {
            let (conn, _) =
                x11rb::connect(None).map_err(|e| InputError::new(format!("Cannot connect to the X server: {e}")))?;
            self.watch = Some(conn);
        }
        let conn = self.watch.as_ref().expect("just set");
        let reply = conn
            .xinput_list_input_devices()
            .map_err(|e| InputError::new(format!("XInput query failed: {e}")))?
            .reply()
            .map_err(|e| InputError::new(format!("XInput query failed: {e}")))?;
        Ok(reply.names.iter().any(|n| n.name == self.name.as_bytes()))
    }
}

impl MouseBackend for UinputBackend {
    fn button(&mut self, button: MouseButton, down: bool) -> InputResult {
        self.emit(EV_KEY, button.evdev_code(), i32::from(down))?;
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Uinput
    }

    fn is_ready(&mut self) -> InputResult<bool> {
        if let Some(t) = self.seen_at {
            return Ok(t.elapsed() >= UINPUT_SETTLE);
        }
        if self.listed_by_x()? {
            self.seen_at = Some(Instant::now());
            self.watch = None;
        }
        Ok(false)
    }
}

impl Drop for UinputBackend {
    fn drop(&mut self) {
        // SAFETY: argument-less request on a valid fd; the fd is closed by File's Drop.
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _);
        }
    }
}

/// Create the backend selected in the configuration.
pub fn create_backend(kind: BackendKind) -> InputResult<Box<dyn MouseBackend>> {
    match kind {
        BackendKind::Xtest => Ok(Box::new(XTestBackend::connect()?)),
        BackendKind::Uinput => Ok(Box::new(UinputBackend::create("ARPAuto Macro virtual mouse")?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_lookup() {
        // keycode 10: KP_Home / KP_7, keycode 11: F7
        let km = Keymap::from_raw(10, 2, vec![0xff95, 0xffb7, keys::XK_F1 + 6, 0]);
        assert_eq!(km.keycodes_for(keys::XK_F1 + 6), vec![11]);
        assert_eq!(km.keysym_for_event(10, true), 0xffb7);
        assert_eq!(km.keysym_for_event(10, false), 0xff95);
        assert!(km.keycodes_for(0x1234).is_empty());
    }

    #[test]
    fn uinput_ioctl_numbers_match_kernel_headers() {
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_RELBIT, 0x4004_5566);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(std::mem::size_of::<RawInputEvent>(), 24);
    }

    #[test]
    fn evdev_button_codes() {
        assert_eq!(MouseButton::Left.evdev_code(), 0x110);
        assert_eq!(MouseButton::Right.evdev_code(), 0x111);
        assert_eq!(MouseButton::Middle.evdev_code(), 0x112);
    }
}
