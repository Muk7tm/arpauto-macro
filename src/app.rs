//! Application controller: owns the configuration and connects the Slint UI with the hold
//! worker and the hotkey service. Runs entirely on the UI thread; background threads talk
//! to it through a channel drained via `slint::invoke_from_event_loop`.

use crate::calibration::{Calibrator, Outcome};
use crate::config::{self, ActivationMode, Config, ConfigStore, FocusLoss, Hotkeys, Preset};
use crate::hold_engine::{Command, EngineEvent, EngineHandle, HoldStats, RunSpec, RunState, StopReason, X11Platform};
use crate::hotkeys::{HotkeyCommand, HotkeyEvent, HotkeyHandle};
use crate::input::BackendKind;
use crate::keys::{KeyCombo, MouseButton};
use crate::profiles::{expand_home, Profile, ProfileStore};
use crate::ui::{AppState, HistoryRow, MainWindow};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

const MAIN_SIZE: (f32, f32) = (380.0, 260.0);
const LARGE_SIZE: (f32, f32) = (440.0, 600.0);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
/// Pause between the game getting focus and the first calibration hold.
const CALIBRATION_SETTLE_MS: u32 = 1000;

enum UiEvent {
    Engine(EngineEvent),
    Hotkey(HotkeyEvent),
}

type Shared = Rc<RefCell<Controller>>;

thread_local! {
    static CONTROLLER: RefCell<Option<Shared>> = const { RefCell::new(None) };
}

fn drain_events() {
    let ctrl = CONTROLLER.with(|c| c.borrow().clone());
    let Some(ctrl) = ctrl else { return };
    match ctrl.try_borrow_mut() {
        Ok(mut c) => c.process_events(),
        Err(_) => slint::Timer::single_shot(Duration::from_millis(1), drain_events),
    };
}

/// Calibration phases (mirrors `calib-phase` in ui/state.slint).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CalibPhase {
    Setup = 0,
    Ready = 1,
    Testing = 2,
    Verdict = 3,
    Done = 4,
    Never = 5,
}

struct Controller {
    ui: slint::Weak<MainWindow>,
    config: Config,
    store: ConfigStore,
    profiles: ProfileStore,
    engine: EngineHandle,
    hotkeys: HotkeyHandle,
    events: mpsc::Receiver<UiEvent>,
    save_timer: slint::Timer,
    dirty: bool,
    run_state: RunState,
    registered: Option<(Hotkeys, ActivationMode)>,
    hotkey_problem: Option<String>,
    engine_message: Option<(String, bool)>,
    notice: Option<String>,
    profile_names: Vec<String>,
    main_size: (f32, f32),
    page: i32,
    profiles_model: Rc<VecModel<SharedString>>,
    // calibration
    calibrator: Option<Calibrator>,
    calib_phase: CalibPhase,
    calib_start_ms: u32,
    calib_holds: u32,
    calib_suggested: u32,
    history_model: Rc<VecModel<HistoryRow>>,
}

fn s(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
}

fn clamp_u32(v: i32, lo: u32, hi: u32) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(0).clamp(lo, hi)
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

impl Controller {
    fn ui(&self) -> Option<MainWindow> {
        self.ui.upgrade()
    }

    // ------------------------------------------------------------------ syncing

    fn sync_all(&mut self) {
        self.sync_settings();
        self.sync_profiles();
        self.sync_run_label();
        self.sync_calibration();
        self.refresh_status();
    }

    fn sync_settings(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        let c = &self.config;
        st.set_button_index(c.hold.button.index() as i32);
        st.set_hold_ms(c.hold.timing.hold_ms as i32);
        st.set_release_ms(c.hold.timing.release_ms as i32);
        st.set_repeat_enabled(c.hold.repeat.enabled);
        st.set_repeat_count(c.hold.repeat.count.min(i32::MAX as u32) as i32);
        st.set_preset_index(c.preset.index() as i32);
        st.set_start_delay_ms(c.hold.start_delay_ms as i32);
        st.set_activation_index(c.activation.index() as i32);
        st.set_hotkey_start(s(c.hotkeys.start_stop.to_string()));
        st.set_hotkey_emergency(s(c.hotkeys.emergency.to_string()));
        st.set_backend_index(c.input.backend.index() as i32);
        st.set_focus_protection(c.input.focus_protection);
        st.set_focus_loss_index(c.input.focus_loss.index() as i32);
        st.set_target_class(s(&c.input.target_class));
        st.set_active_profile(s(c.active_profile.clone().unwrap_or_default()));
    }

    fn sync_profiles(&mut self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        self.profile_names = self.profiles.list();
        self.profiles_model.set_vec(self.profile_names.iter().map(s).collect::<Vec<_>>());
        if st.get_profile_index() >= self.profile_names.len() as i32 {
            st.set_profile_index(-1);
        }
        st.set_active_profile(s(self.config.active_profile.clone().unwrap_or_default()));
    }

    fn sync_run_label(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        let hk = self.config.hotkeys.start_stop.to_string();
        let label = match self.run_state {
            RunState::Idle => match self.config.activation {
                ActivationMode::Toggle => format!("Start  ·  {hk}"),
                ActivationMode::Hold => format!("Start  ·  hold {hk}"),
            },
            RunState::Arming => "Starting…  ·  click to cancel".to_string(),
            RunState::WaitingForTarget => "Waiting for target window…".to_string(),
            RunState::Running => format!("Stop  ·  {hk}"),
            RunState::Paused => format!("Paused  ·  {hk} stops"),
        };
        st.set_run_label(s(label));
        st.set_run_state(match self.run_state {
            RunState::Idle => 0,
            RunState::Arming => 1,
            RunState::WaitingForTarget => 2,
            RunState::Running => 3,
            RunState::Paused => 4,
        });
    }

    fn refresh_status(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        let (text, error) = if let Some((m, e)) = &self.engine_message {
            (m.clone(), *e)
        } else if let Some(problem) = self.config.hold.validate().first() {
            (problem.clone(), true)
        } else if let Some(p) = &self.hotkey_problem {
            (format!("Hotkeys: {p}"), true)
        } else if let Some(n) = &self.notice {
            (n.clone(), false)
        } else {
            (String::new(), false)
        };
        st.set_status_text(s(text));
        st.set_status_error(error);
    }

    // ------------------------------------------------------------------ changes

    fn changed(&mut self) {
        self.engine.send(Command::Update(Arc::new(RunSpec::from_config(&self.config))));
        let wanted = (self.config.hotkeys, self.config.activation);
        if self.registered != Some(wanted) {
            self.registered = Some(wanted);
            self.hotkeys.send(HotkeyCommand::Configure { hotkeys: wanted.0, mode: wanted.1 });
        }
        self.schedule_save();
        self.sync_settings();
        self.sync_run_label();
        self.refresh_status();
    }

    fn schedule_save(&mut self) {
        self.dirty = true;
        self.save_timer.start(slint::TimerMode::SingleShot, SAVE_DEBOUNCE, || {
            let ctrl = CONTROLLER.with(|c| c.borrow().clone());
            if let Some(ctrl) = ctrl {
                if let Ok(mut c) = ctrl.try_borrow_mut() {
                    c.save_now();
                }
            }
        });
    }

    fn save_now(&mut self) {
        if !self.dirty {
            return;
        }
        match self.store.save(&self.config) {
            Ok(()) => self.dirty = false,
            Err(e) => {
                tracing::error!("saving configuration failed: {e}");
                self.notice = Some(format!("Could not save settings: {e}"));
                self.refresh_status();
            }
        }
    }

    fn set_int(&mut self, key: &str, v: i32) {
        let (lo, hi) = config::DURATION_RANGE;
        let c = &mut self.config;
        match key {
            "button" => c.hold.button = MouseButton::from_index(v.max(0) as usize),
            "hold" => {
                c.hold.timing.hold_ms = clamp_u32(v, lo, hi);
                c.refresh_preset();
            }
            "release" => {
                c.hold.timing.release_ms = clamp_u32(v, lo, hi);
                c.refresh_preset();
            }
            "repeat-count" => c.hold.repeat.count = clamp_u32(v, 0, config::MAX_REPEAT_COUNT),
            "start-delay" => c.hold.start_delay_ms = clamp_u32(v, 0, config::MAX_START_DELAY_MS),
            "activation" => c.activation = ActivationMode::from_index(v.max(0) as usize),
            "backend" => c.input.backend = BackendKind::from_index(v.max(0) as usize),
            "focus-loss" => c.input.focus_loss = FocusLoss::from_index(v.max(0) as usize),
            "calib-start" => {
                self.calib_start_ms = clamp_u32(v, 1, crate::calibration::MAX_TEST_MS);
                self.sync_calibration();
                return;
            }
            "calib-holds" => {
                self.calib_holds = clamp_u32(v, 1, 50);
                self.sync_calibration();
                return;
            }
            other => {
                tracing::warn!("unknown int setting {other}");
                return;
            }
        }
        self.changed();
    }

    fn set_bool(&mut self, key: &str, v: bool) {
        match key {
            "repeat" => self.config.hold.repeat.enabled = v,
            "focus-protection" => self.config.input.focus_protection = v,
            other => {
                tracing::warn!("unknown bool setting {other}");
                return;
            }
        }
        self.changed();
    }

    fn set_text(&mut self, key: &str, text: &str) {
        match key {
            "hotkey-start" | "hotkey-emergency" => match KeyCombo::parse(text) {
                Ok(combo) => self.set_hotkey(key == "hotkey-start", combo),
                Err(e) => {
                    self.set_hotkey_status(false, &e);
                    self.sync_settings();
                }
            },
            "target-class" => {
                let t: String = text.trim().chars().take(config::MAX_TARGET_CLASS_LEN).collect();
                if t != self.config.input.target_class {
                    self.config.input.target_class = t;
                    self.changed();
                }
            }
            other => tracing::warn!("unknown text setting {other}"),
        }
    }

    fn set_hotkey(&mut self, start: bool, combo: KeyCombo) {
        let mut hk = self.config.hotkeys;
        if start {
            hk.start_stop = combo;
        } else {
            hk.emergency = combo;
        }
        if hk == self.config.hotkeys {
            self.sync_settings();
            return;
        }
        if let Some(problem) = hk.validate().into_iter().next() {
            self.set_hotkey_status(false, &problem);
            self.sync_settings();
            return;
        }
        self.config.hotkeys = hk;
        self.changed();
    }

    fn set_hotkey_status(&self, ok: bool, message: &str) {
        if let Some(ui) = self.ui() {
            let st = ui.global::<AppState>();
            st.set_hotkey_ok(ok);
            st.set_hotkey_status(s(message));
        }
    }

    fn select_preset(&mut self, i: i32) {
        self.config.apply_preset(Preset::from_index(i.max(0) as usize));
        self.changed();
    }

    fn reset_defaults(&mut self) {
        self.config = Config::default();
        self.notice = Some("Settings reset to defaults".into());
        self.changed();
    }

    fn navigate(&mut self, page: i32) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        if !st.get_capture_target().is_empty() {
            self.hotkeys.send(HotkeyCommand::CancelCapture);
        }
        let window = ui.window();
        let current = window.size().to_logical(window.scale_factor());
        if self.page == 0 && page != 0 {
            self.main_size = (current.width, current.height);
        }
        self.page = page;
        st.set_page(page);
        let (w, h) = if page == 0 { self.main_size } else { (current.width.max(LARGE_SIZE.0), current.height.max(LARGE_SIZE.1)) };
        window.set_size(slint::LogicalSize::new(w, h));
    }

    // ------------------------------------------------------------------ profiles

    fn profile_message(&self, ok: bool, msg: &str) {
        if let Some(ui) = self.ui() {
            let st = ui.global::<AppState>();
            st.set_profile_error(!ok);
            st.set_profile_message(s(msg));
        }
    }

    fn profile_name(&self, index: i32) -> Option<String> {
        usize::try_from(index).ok().and_then(|i| self.profile_names.get(i).cloned())
    }

    fn select_profile_named(&mut self, name: &str) {
        self.sync_profiles();
        if let Some(ui) = self.ui() {
            let idx = self.profile_names.iter().position(|n| n == name).map(|i| i as i32).unwrap_or(-1);
            ui.global::<AppState>().set_profile_index(idx);
        }
    }

    fn profile_save(&mut self, name: &str) {
        match self.profiles.save(&Profile::new(name, self.config.hold)) {
            Ok(()) => {
                let stored = crate::profiles::validate_name(name).unwrap_or_else(|_| name.to_string());
                self.config.active_profile = Some(stored.clone());
                self.changed();
                self.select_profile_named(&stored);
                self.profile_message(true, &format!("Saved the current settings as \"{stored}\""));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_load(&mut self, index: i32) {
        let Some(name) = self.profile_name(index) else { return };
        match self.profiles.load(&name) {
            Ok(p) => {
                self.config.hold = p.settings;
                self.config.refresh_preset();
                self.config.active_profile = Some(p.name.clone());
                self.changed();
                self.sync_profiles();
                self.profile_message(true, &format!("Loaded \"{}\"", p.name));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_rename(&mut self, index: i32, new: &str) {
        let Some(old) = self.profile_name(index) else { return };
        match self.profiles.rename(&old, new) {
            Ok(stored) => {
                if self.config.active_profile.as_deref() == Some(old.as_str()) {
                    self.config.active_profile = Some(stored.clone());
                    self.changed();
                }
                self.select_profile_named(&stored);
                self.profile_message(true, &format!("Renamed to \"{stored}\""));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_duplicate(&mut self, index: i32) {
        let Some(name) = self.profile_name(index) else { return };
        match self.profiles.duplicate(&name) {
            Ok(copy) => {
                self.select_profile_named(&copy);
                self.profile_message(true, &format!("Created \"{copy}\""));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_delete(&mut self, index: i32) {
        let Some(name) = self.profile_name(index) else { return };
        match self.profiles.delete(&name) {
            Ok(()) => {
                if self.config.active_profile.as_deref() == Some(name.as_str()) {
                    self.config.active_profile = None;
                    self.changed();
                }
                self.sync_profiles();
                if let Some(ui) = self.ui() {
                    ui.global::<AppState>().set_profile_index(-1);
                }
                self.profile_message(true, &format!("Deleted \"{name}\" (current settings are unchanged)"));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_export(&mut self, index: i32, path: &str) {
        let Some(name) = self.profile_name(index) else { return };
        if path.trim().is_empty() {
            self.profile_message(false, "Enter a file or folder path to export to");
            return;
        }
        match self.profiles.export(&name, &expand_home(path)) {
            Ok(out) => self.profile_message(true, &format!("Exported to {}", out.display())),
            Err(e) => self.profile_message(false, &e),
        }
    }

    fn profile_import(&mut self, path: &str) {
        match self.profiles.import(&expand_home(path)) {
            Ok(name) => {
                self.select_profile_named(&name);
                self.profile_message(true, &format!("Imported as \"{name}\". Select it and click Load to use it."));
            }
            Err(e) => self.profile_message(false, &e),
        }
    }

    // ------------------------------------------------------------------ calibration

    fn sync_calibration(&self) {
        let Some(ui) = self.ui() else { return };
        let st = ui.global::<AppState>();
        st.set_calib_phase(self.calib_phase as i32);
        st.set_calib_start_ms(self.calib_start_ms as i32);
        st.set_calib_holds(self.calib_holds as i32);
        st.set_calib_suggested_ms(self.calib_suggested as i32);
        if let Some(c) = &self.calibrator {
            st.set_calib_current_ms(c.current() as i32);
            st.set_calib_test_number(c.history().len() as i32 + 1);
            let rows: Vec<HistoryRow> = c
                .history()
                .iter()
                .map(|(ms, ok)| HistoryRow {
                    hold: s(format!("{ms} ms")),
                    verdict: s(if *ok { "recognized" } else { "not recognized" }),
                    ok: *ok,
                })
                .collect();
            self.history_model.set_vec(rows);
        } else {
            self.history_model.set_vec(Vec::new());
        }
    }

    fn calib_message(&self, msg: &str) {
        if let Some(ui) = self.ui() {
            ui.global::<AppState>().set_calib_message(s(msg));
        }
    }

    fn ready_message(&self) -> String {
        format!(
            "Click Run test, then click into the game window. {} holds start {:.0} s after the game is focused{}.",
            self.calib_holds,
            f64::from(CALIBRATION_SETTLE_MS) / 1000.0,
            if self.config.input.focus_protection { " with the pointer over it" } else { "" }
        )
    }

    fn calib_begin(&mut self) {
        self.calibrator = Some(Calibrator::new(self.calib_start_ms));
        self.calib_phase = CalibPhase::Ready;
        self.sync_calibration();
        self.calib_message(&self.ready_message());
    }

    fn calib_run(&mut self) {
        let Some(cal) = &self.calibrator else { return };
        let mut spec = RunSpec::from_config(&self.config);
        spec.hold.timing.hold_ms = cal.current();
        spec.hold.repeat = config::RepeatConfig { enabled: true, count: self.calib_holds };
        spec.hold.start_delay_ms = 0;
        spec.settle_ms = CALIBRATION_SETTLE_MS;
        self.engine.send(Command::Calibrate(Arc::new(spec)));
        self.calib_phase = CalibPhase::Testing;
        self.sync_calibration();
        self.calib_message(&format!("Testing {} holds of {} ms. Watch the game.", self.calib_holds, cal.current()));
    }

    fn calib_finished(&mut self, reason: StopReason, holds: HoldStats, message: &str) {
        if self.calibrator.is_none() {
            return;
        }
        if reason == StopReason::Completed {
            self.calib_phase = CalibPhase::Verdict;
            let measured = if holds.count > 0 {
                format!(
                    "Sent {} holds; measured {}–{} ms between press and release on the X server connection.",
                    holds.count,
                    fmt_ms(holds.min),
                    fmt_ms(holds.max)
                )
            } else {
                String::new()
            };
            self.sync_calibration();
            self.calib_message(&format!("{measured} Did the game react to every hold?"));
        } else {
            self.calib_phase = CalibPhase::Ready;
            self.sync_calibration();
            let why = if message.is_empty() { "stopped".to_string() } else { message.to_string() };
            self.calib_message(&format!("Test interrupted ({why}). Run it again when ready."));
        }
    }

    fn calib_verdict(&mut self, recognized: bool) {
        let Some(cal) = self.calibrator.as_mut() else { return };
        match cal.record(recognized) {
            Outcome::Next(_) => {
                self.calib_phase = CalibPhase::Ready;
                self.sync_calibration();
                self.calib_message(&self.ready_message());
            }
            Outcome::Done { shortest, suggested } => {
                self.calib_phase = CalibPhase::Done;
                self.calib_suggested = suggested;
                self.sync_calibration();
                self.calib_message(&format!(
                    "The shortest recognized hold was {shortest} ms. The suggestion adds a safety margin; recheck it in real use."
                ));
            }
            Outcome::NeverRecognized => {
                self.calib_phase = CalibPhase::Never;
                self.sync_calibration();
                self.calib_message(&format!(
                    "Not recognized even at {} ms. The game may ignore input from this backend; try uinput (Settings → Input).",
                    crate::calibration::MAX_TEST_MS
                ));
            }
        }
    }

    fn calib_apply(&mut self) {
        if self.calib_suggested == 0 {
            return;
        }
        self.config.hold.timing.hold_ms = self.calib_suggested;
        self.config.refresh_preset();
        self.changed();
        self.calib_message(&format!("Hold duration set to {} ms.", self.calib_suggested));
    }

    fn calib_reset(&mut self) {
        if self.calib_phase == CalibPhase::Testing {
            self.engine.send(Command::Stop);
        }
        self.calibrator = None;
        self.calib_phase = CalibPhase::Setup;
        self.calib_suggested = 0;
        self.sync_calibration();
        self.calib_message("");
    }

    // ------------------------------------------------------------------ capture / detect

    fn capture(&mut self, target: &str) {
        if let Some(ui) = self.ui() {
            ui.global::<AppState>().set_capture_target(s(target));
        }
        self.hotkeys.send(HotkeyCommand::Capture { purpose: target.to_string() });
    }

    fn on_captured(&mut self, purpose: &str, result: Result<KeyCombo, String>) {
        if let Some(ui) = self.ui() {
            ui.global::<AppState>().set_capture_target(s(""));
        }
        match (purpose, result) {
            ("start", Ok(combo)) => self.set_hotkey(true, combo),
            ("emergency", Ok(combo)) => self.set_hotkey(false, combo),
            (_, Err(e)) if e != "Capture cancelled" => self.set_hotkey_status(false, &e),
            _ => {}
        }
    }

    fn detect_target(&mut self) {
        if let Some(ui) = self.ui() {
            let st = ui.global::<AppState>();
            st.set_detecting(true);
            st.set_input_message(s("Focus the game window now…"));
        }
        self.hotkeys.send(HotkeyCommand::DetectWindow { delay: Duration::from_secs(3) });
    }

    // ------------------------------------------------------------------ background events

    fn process_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                UiEvent::Engine(e) => self.on_engine(e),
                UiEvent::Hotkey(e) => self.on_hotkey(e),
            }
        }
    }

    fn on_engine(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::State { state, detail } => {
                self.run_state = state;
                self.engine_message = match state {
                    RunState::Idle | RunState::Running => None,
                    RunState::Arming if detail.ends_with(" ms") => Some((format!("Starting in {detail}…"), false)),
                    RunState::Arming | RunState::WaitingForTarget | RunState::Paused => Some((format!("{detail}…"), false)),
                };
                if self.calib_phase == CalibPhase::Testing {
                    let msg = match state {
                        RunState::WaitingForTarget => format!("{detail}. Click into the game window."),
                        RunState::Arming if detail.ends_with(" ms") => "Target locked; starting in a moment…".to_string(),
                        RunState::Running => self
                            .calibrator
                            .as_ref()
                            .map(|c| format!("Testing {} holds of {} ms. Watch the game.", self.calib_holds, c.current()))
                            .unwrap_or_default(),
                        _ => format!("{detail}…"),
                    };
                    self.calib_message(&msg);
                }
                self.sync_run_label();
                self.refresh_status();
            }
            EngineEvent::Finished { reason, cycles, holds, message, calibration, .. } => {
                self.run_state = RunState::Idle;
                if holds.count > 0 {
                    tracing::info!(
                        "{} holds, measured min {} / mean {} / max {} ms",
                        holds.count,
                        fmt_ms(holds.min),
                        fmt_ms(holds.mean()),
                        fmt_ms(holds.max)
                    );
                }
                self.engine_message = match reason {
                    StopReason::Completed if calibration => None,
                    StopReason::Completed => {
                        Some((format!("Finished after {cycles} hold{}", if cycles == 1 { "" } else { "s" }), false))
                    }
                    StopReason::Stopped | StopReason::Shutdown => None,
                    StopReason::Emergency | StopReason::FocusLost => Some((message.clone(), false)),
                    StopReason::Failed => Some((message.clone(), true)),
                };
                if calibration {
                    self.calib_finished(reason, holds, &message);
                }
                self.sync_run_label();
                self.refresh_status();
            }
        }
    }

    fn on_hotkey(&mut self, ev: HotkeyEvent) {
        match ev {
            HotkeyEvent::Registered { ok, message } => {
                self.hotkey_problem = (!ok).then(|| message.clone());
                self.set_hotkey_status(ok, &if ok { format!("Registered: {message}") } else { message });
                self.refresh_status();
            }
            HotkeyEvent::Captured { purpose, result } => self.on_captured(&purpose, result),
            HotkeyEvent::Detected(result) => {
                let msg = match result {
                    Ok(w) => {
                        let class = w.class.split_whitespace().next().unwrap_or_default().to_string();
                        let msg = format!("Detected \"{}\"", w.class);
                        if class != self.config.input.target_class {
                            self.config.input.target_class = class;
                            self.changed();
                        }
                        msg
                    }
                    Err(e) => e,
                };
                if let Some(ui) = self.ui() {
                    let st = ui.global::<AppState>();
                    st.set_detecting(false);
                    st.set_input_message(s(msg));
                }
            }
            HotkeyEvent::Fatal(e) => {
                self.hotkey_problem = Some(e.clone());
                self.set_hotkey_status(false, &e);
                self.refresh_status();
            }
        }
    }
}

/// Build the window, start the worker threads and run the UI until the window closes.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let store = ConfigStore::new(config::default_config_dir()?);
    let loaded = store.load();
    for n in &loaded.notices {
        tracing::warn!("{n}");
    }
    let profiles = ProfileStore::new(store.profiles_dir());

    let ui = MainWindow::new()?;
    let (tx, rx) = mpsc::channel::<UiEvent>();

    let engine_tx = tx.clone();
    let engine = EngineHandle::spawn(X11Platform::default, move |ev| {
        if engine_tx.send(UiEvent::Engine(ev)).is_ok() {
            let _ = slint::invoke_from_event_loop(drain_events);
        }
    })?;
    let hotkey_tx = tx;
    let hotkeys = HotkeyHandle::spawn(engine.sender(), move |ev| {
        if hotkey_tx.send(UiEvent::Hotkey(ev)).is_ok() {
            let _ = slint::invoke_from_event_loop(drain_events);
        }
    })?;

    // SIGINT/SIGTERM/SIGHUP: stop and release the button, then leave the event loop
    // normally so that the configuration is saved and threads are joined.
    let signal_engine = engine.sender();
    crate::safety::spawn_signal_watcher(move |_| {
        let _ = signal_engine.send(Command::Emergency);
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    })?;

    let profiles_model = Rc::new(VecModel::<SharedString>::default());
    let history_model = Rc::new(VecModel::<HistoryRow>::default());
    {
        let st = ui.global::<AppState>();
        st.set_profiles(ModelRc::from(profiles_model.clone()));
        st.set_calib_history(ModelRc::from(history_model.clone()));
        st.set_config_path(s(store.config_path().display().to_string()));
        st.set_profiles_dir(s(store.profiles_dir().display().to_string()));
    }

    let ctrl: Shared = Rc::new(RefCell::new(Controller {
        ui: ui.as_weak(),
        config: loaded.config,
        store,
        profiles,
        engine,
        hotkeys,
        events: rx,
        save_timer: slint::Timer::default(),
        dirty: false,
        run_state: RunState::Idle,
        registered: None,
        hotkey_problem: None,
        engine_message: None,
        notice: loaded.notices.first().cloned(),
        profile_names: Vec::new(),
        main_size: MAIN_SIZE,
        page: 0,
        profiles_model,
        calibrator: None,
        calib_phase: CalibPhase::Setup,
        calib_start_ms: 200,
        calib_holds: 5,
        calib_suggested: 0,
        history_model,
    }));
    CONTROLLER.with(|c| *c.borrow_mut() = Some(ctrl.clone()));
    register_callbacks(&ui, &ctrl);

    {
        let mut c = ctrl.borrow_mut();
        c.changed();
        c.dirty = false; // nothing to save yet; `changed` only pushed state to the threads
        c.sync_all();
    }

    ui.run()?;

    let ctrl = CONTROLLER.with(|c| c.borrow_mut().take()).unwrap_or(ctrl);
    let mut c = ctrl.borrow_mut();
    c.save_timer.stop();
    c.engine.shutdown();
    c.hotkeys.shutdown();
    c.save_now();
    Ok(())
}

fn register_callbacks(ui: &MainWindow, ctrl: &Shared) {
    let st = ui.global::<AppState>();

    macro_rules! on {
        ($setter:ident, |$c:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
            let weak = Rc::downgrade(ctrl);
            st.$setter(move |$($arg: $ty),*| {
                if let Some(rc) = weak.upgrade() {
                    match rc.try_borrow_mut() {
                        Ok(mut guard) => {
                            let $c: &mut Controller = &mut guard;
                            $body;
                        }
                        Err(_) => tracing::warn!(concat!(stringify!($setter), " ignored: controller busy")),
                    }
                }
            });
        }};
    }

    on!(on_navigate, |c, page: i32| c.navigate(page));
    on!(on_toggle_run, |c| c.engine.send(Command::Toggle));
    on!(on_set_int, |c, key: SharedString, v: i32| c.set_int(&key, v));
    on!(on_set_bool, |c, key: SharedString, v: bool| c.set_bool(&key, v));
    on!(on_set_text, |c, key: SharedString, v: SharedString| c.set_text(&key, &v));
    on!(on_select_preset, |c, i: i32| c.select_preset(i));
    on!(on_capture, |c, target: SharedString| c.capture(&target));
    on!(on_cancel_capture, |c| c.hotkeys.send(HotkeyCommand::CancelCapture));
    on!(on_detect_target, |c| c.detect_target());
    on!(on_reset_defaults, |c| c.reset_defaults());

    on!(on_profile_save, |c, name: SharedString| c.profile_save(&name));
    on!(on_profile_load, |c, i: i32| c.profile_load(i));
    on!(on_profile_rename, |c, i: i32, name: SharedString| c.profile_rename(i, &name));
    on!(on_profile_duplicate, |c, i: i32| c.profile_duplicate(i));
    on!(on_profile_delete, |c, i: i32| c.profile_delete(i));
    on!(on_profile_export, |c, i: i32, path: SharedString| c.profile_export(i, &path));
    on!(on_profile_import, |c, path: SharedString| c.profile_import(&path));

    on!(on_calib_begin, |c| c.calib_begin());
    on!(on_calib_run, |c| c.calib_run());
    on!(on_calib_stop, |c| c.engine.send(Command::Stop));
    on!(on_calib_verdict, |c, ok: bool| c.calib_verdict(ok));
    on!(on_calib_apply, |c| c.calib_apply());
    on!(on_calib_reset, |c| c.calib_reset());
}
