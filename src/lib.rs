//! ARPAuto Macro: repeated, timed mouse-button holds for Linux X11.
//!
//! Module overview:
//! * [`hold_engine`]: the hold worker and its state machine
//! * [`calibration`]: guided search for the shortest recognized hold
//! * [`input`]: XTest / uinput mouse backends and the keymap snapshot
//! * [`timing`]: deadline-based scheduler
//! * [`hotkeys`]: global hotkeys, key capture and window detection
//! * [`safety`]: focus/pointer tracking, held-button bookkeeping, instance lock, signals
//! * [`keys`]: keysyms, modifiers, key-combination parsing and mouse buttons
//! * [`config`] and [`profiles`]: persistent settings and configuration profiles
//! * [`app`]: the Slint UI controller

pub mod app;
pub mod calibration;
pub mod config;
pub mod hold_engine;
pub mod hotkeys;
pub mod input;
pub mod keys;
pub mod profiles;
pub mod safety;
pub mod timing;

/// Generated Slint UI types.
pub mod ui {
    slint::include_modules!();
}
