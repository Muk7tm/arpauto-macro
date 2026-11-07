//! ARPAuto Macro configuration: `$XDG_CONFIG_HOME/arpauto-macro/config.toml`.
//!
//! * every field has a default (`#[serde(default)]`), so older and partially edited files load;
//! * values are sanitized (clamped) after loading;
//! * writes are atomic (temp file + fsync + rename);
//! * an unparsable file is moved aside as `config.toml.corrupt-<unix time>` and defaults
//!   are used, so a bad edit never prevents the application from starting.

use crate::input::BackendKind;
use crate::keys::{self, KeyCombo, MouseButton};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const APP_DIR_NAME: &str = "arpauto-macro";
pub const CONFIG_FILE: &str = "config.toml";
pub const CONFIG_VERSION: u32 = 1;
pub const DURATION_RANGE: (u32, u32) = (1, 10_000);
pub const MAX_START_DELAY_MS: u32 = 30_000;
pub const MAX_REPEAT_COUNT: u32 = 1_000_000;
pub const MAX_TARGET_CLASS_LEN: usize = 128;
/// A repeating cycle shorter than this would flood the X server.
pub const MIN_CYCLE_MS: u32 = 5;

/// Hold and release durations in milliseconds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HoldTiming {
    /// How long the button stays down.
    pub hold_ms: u32,
    /// How long the button stays up before the next hold.
    pub release_ms: u32,
}

impl Default for HoldTiming {
    fn default() -> Self {
        Preset::Balanced.timing()
    }
}

impl HoldTiming {
    pub fn sanitize(&mut self) -> bool {
        let before = *self;
        self.hold_ms = self.hold_ms.clamp(DURATION_RANGE.0, DURATION_RANGE.1);
        self.release_ms = self.release_ms.clamp(DURATION_RANGE.0, DURATION_RANGE.1);
        before != *self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Fast,
    #[default]
    Balanced,
    Stable,
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Fast, Preset::Balanced, Preset::Stable, Preset::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Fast => "Fast",
            Preset::Balanced => "Balanced",
            Preset::Stable => "Stable",
            Preset::Custom => "Custom",
        }
    }

    pub fn timing(self) -> HoldTiming {
        let (hold_ms, release_ms) = match self {
            Preset::Fast => (60, 30),
            Preset::Balanced | Preset::Custom => (120, 60),
            Preset::Stable => (200, 100),
        };
        HoldTiming { hold_ms, release_ms }
    }

    pub fn detect(t: &HoldTiming) -> Preset {
        [Preset::Fast, Preset::Balanced, Preset::Stable].into_iter().find(|p| p.timing() == *t).unwrap_or(Preset::Custom)
    }

    pub fn index(self) -> usize {
        Preset::ALL.iter().position(|p| *p == self).unwrap_or(1)
    }
    pub fn from_index(i: usize) -> Self {
        Preset::ALL.get(i).copied().unwrap_or_default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationMode {
    /// Press the hotkey once to start, again to stop.
    #[default]
    Toggle,
    /// Runs only while the hotkey is held down.
    Hold,
}

impl ActivationMode {
    pub fn index(self) -> usize {
        match self {
            ActivationMode::Toggle => 0,
            ActivationMode::Hold => 1,
        }
    }
    pub fn from_index(i: usize) -> Self {
        if i == 1 {
            ActivationMode::Hold
        } else {
            ActivationMode::Toggle
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FocusLoss {
    #[default]
    Stop,
    Pause,
}

impl FocusLoss {
    pub fn index(self) -> usize {
        match self {
            FocusLoss::Stop => 0,
            FocusLoss::Pause => 1,
        }
    }
    pub fn from_index(i: usize) -> Self {
        if i == 1 {
            FocusLoss::Pause
        } else {
            FocusLoss::Stop
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RepeatConfig {
    pub enabled: bool,
    /// Number of holds when repeating; 0 means until stopped.
    pub count: u32,
}

impl Default for RepeatConfig {
    fn default() -> Self {
        RepeatConfig { enabled: true, count: 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub start_stop: KeyCombo,
    pub emergency: KeyCombo,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys { start_stop: KeyCombo::new(keys::XK_F1 + 6), emergency: KeyCombo::new(keys::XK_F1 + 9) }
    }
}

impl Hotkeys {
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, k) in [("Start/stop", self.start_stop), ("Emergency stop", self.emergency)] {
            if keys::is_modifier(k.key) {
                out.push(format!("{name} hotkey needs a non-modifier key"));
            }
        }
        if self.start_stop.key == self.emergency.key {
            out.push("Start/stop and emergency stop must use different keys".into());
        }
        out
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    pub backend: BackendKind,
    /// Only click while the window that was focused at start is focused and under the pointer.
    pub focus_protection: bool,
    pub focus_loss: FocusLoss,
    /// Optional WM_CLASS substring the target window must match (e.g. "sober").
    pub target_class: String,
}

impl Default for InputConfig {
    fn default() -> Self {
        InputConfig { backend: BackendKind::Xtest, focus_protection: true, focus_loss: FocusLoss::Stop, target_class: String::new() }
    }
}

/// Everything that defines one hold cycle; also what a profile stores.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HoldSettings {
    pub button: MouseButton,
    pub timing: HoldTiming,
    pub repeat: RepeatConfig,
    pub start_delay_ms: u32,
}

impl Default for HoldSettings {
    fn default() -> Self {
        HoldSettings { button: MouseButton::Left, timing: HoldTiming::default(), repeat: RepeatConfig::default(), start_delay_ms: 0 }
    }
}

impl HoldSettings {
    /// Problems that prevent running.
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let (lo, hi) = DURATION_RANGE;
        if !(lo..=hi).contains(&self.timing.hold_ms) {
            out.push(format!("Hold duration must be {lo}–{hi} ms"));
        }
        if !(lo..=hi).contains(&self.timing.release_ms) {
            out.push(format!("Release duration must be {lo}–{hi} ms"));
        }
        if self.repeat.enabled && self.timing.hold_ms + self.timing.release_ms < MIN_CYCLE_MS {
            out.push(format!("Hold + release must be at least {MIN_CYCLE_MS} ms when repeating"));
        }
        if self.start_delay_ms > MAX_START_DELAY_MS {
            out.push(format!("Start delay must be at most {MAX_START_DELAY_MS} ms"));
        }
        out
    }

    pub fn sanitize(&mut self) -> bool {
        let mut changed = self.timing.sanitize();
        if self.start_delay_ms > MAX_START_DELAY_MS {
            self.start_delay_ms = MAX_START_DELAY_MS;
            changed = true;
        }
        if self.repeat.count > MAX_REPEAT_COUNT {
            self.repeat.count = MAX_REPEAT_COUNT;
            changed = true;
        }
        changed
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub preset: Preset,
    pub activation: ActivationMode,
    /// Profile the current settings were last loaded from or saved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    pub hold: HoldSettings,
    pub hotkeys: Hotkeys,
    pub input: InputConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            preset: Preset::Balanced,
            activation: ActivationMode::Toggle,
            active_profile: None,
            hold: HoldSettings::default(),
            hotkeys: Hotkeys::default(),
            input: InputConfig::default(),
        }
    }
}

impl Config {
    pub fn apply_preset(&mut self, preset: Preset) {
        if preset != Preset::Custom {
            self.hold.timing = preset.timing();
        }
        self.preset = preset;
    }

    pub fn refresh_preset(&mut self) {
        self.preset = Preset::detect(&self.hold.timing);
    }

    /// Problems that prevent running (settings and hotkeys).
    pub fn validate(&self) -> Vec<String> {
        let mut out = self.hold.validate();
        out.extend(self.hotkeys.validate());
        out
    }

    /// Clamp values into range and repair invalid entries; returns notes for the user.
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.hold.sanitize() {
            notes.push("Some timing values were out of range and have been adjusted".to_string());
        }
        if self.preset != Preset::Custom && Preset::detect(&self.hold.timing) != self.preset {
            self.preset = Preset::Custom;
        }
        let trimmed: String = self.input.target_class.trim().chars().take(MAX_TARGET_CLASS_LEN).collect();
        self.input.target_class = trimmed;
        let hotkey_issues = self.hotkeys.validate();
        if !hotkey_issues.is_empty() {
            notes.push(format!("Hotkeys reset to F7/F10: {}", hotkey_issues.join("; ")));
            self.hotkeys = Hotkeys::default();
        }
        notes
    }
}

/// `$XDG_CONFIG_HOME/arpauto-macro`, falling back to `~/.config/arpauto-macro`.
pub fn default_config_dir() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return Ok(p.join(APP_DIR_NAME));
        }
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join(APP_DIR_NAME))
}

/// Write `bytes` to `path` atomically: temp file in the same directory, fsync, rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn unix_time() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub struct LoadOutcome {
    pub config: Config,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    dir: PathBuf,
}

impl ConfigStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ConfigStore { dir: dir.into() }
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.dir.join("profiles")
    }

    pub fn load(&self) -> LoadOutcome {
        let path = self.config_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return LoadOutcome { config: Config::default(), notices: Vec::new() };
            }
            Err(e) => {
                return LoadOutcome {
                    config: Config::default(),
                    notices: vec![format!("Could not read {}: {e}. Using defaults.", path.display())],
                };
            }
        };
        match toml::from_str::<Config>(&text) {
            Ok(mut config) => {
                let mut notices = Vec::new();
                if config.version > CONFIG_VERSION {
                    notices.push(format!(
                        "config.toml was written by a newer version (v{}); unknown settings are ignored",
                        config.version
                    ));
                }
                config.version = CONFIG_VERSION;
                notices.extend(config.sanitize());
                LoadOutcome { config, notices }
            }
            Err(e) => {
                let backup = self.dir.join(format!("{CONFIG_FILE}.corrupt-{}", unix_time()));
                let moved = fs::rename(&path, &backup).is_ok();
                tracing::warn!("config parse error: {e}");
                let msg = if moved {
                    format!("config.toml was invalid and was moved to {}. Defaults restored.", backup.display())
                } else {
                    "config.toml is invalid; defaults are used for this session.".to_string()
                };
                LoadOutcome { config: Config::default(), notices: vec![msg] }
            }
        }
    }

    pub fn save(&self, config: &Config) -> io::Result<()> {
        let mut c = config.clone();
        c.version = CONFIG_VERSION;
        let body = toml::to_string_pretty(&c).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let text = format!("# ARPAuto Macro configuration (written automatically; safe to edit while the app is closed)\n{body}");
        atomic_write(&self.config_path(), text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_specification() {
        let c = Config::default();
        assert_eq!(c.hold.button, MouseButton::Left);
        assert_eq!((c.hold.timing.hold_ms, c.hold.timing.release_ms), (120, 60));
        assert!(c.hold.repeat.enabled);
        assert_eq!(c.hotkeys.start_stop.to_string(), "F7");
        assert_eq!(c.hotkeys.emergency.to_string(), "F10");
        assert_eq!(c.input.backend, BackendKind::Xtest);
        assert!(c.input.focus_protection);
        assert_eq!(c.preset, Preset::Balanced);
        assert!(c.validate().is_empty());
    }

    #[test]
    fn round_trip_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join(APP_DIR_NAME));
        let mut c = Config::default();
        c.hold.button = MouseButton::Right;
        c.hold.timing.hold_ms = 77;
        c.refresh_preset();
        c.input.target_class = "sober".into();
        store.save(&c).unwrap();
        let loaded = store.load();
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        assert_eq!(loaded.config, c);
        assert_eq!(loaded.config.preset, Preset::Custom);
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        fs::write(store.config_path(), "[[[ nope").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.notices.len(), 1);
        assert!(!store.config_path().exists());
        assert!(fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("config.toml.corrupt-")));
    }

    #[test]
    fn out_of_range_values_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        fs::write(store.config_path(), "[hold]\nbutton = \"middle\"\nstart_delay_ms = 99999999\n[hold.timing]\nhold_ms = 0\n[hotkeys]\nemergency = \"Shift\"\n").unwrap();
        let loaded = store.load();
        let c = loaded.config;
        assert_eq!(c.hold.button, MouseButton::Middle);
        assert_eq!(c.hold.timing.hold_ms, 1);
        assert_eq!(c.hold.timing.release_ms, 60, "unspecified fields keep defaults");
        assert_eq!(c.hold.start_delay_ms, MAX_START_DELAY_MS);
        assert_eq!(c.hotkeys, Hotkeys::default());
        assert!(loaded.notices.len() >= 2, "{:?}", loaded.notices);
    }

    #[test]
    fn validation_rejects_bad_settings() {
        let mut h = HoldSettings::default();
        h.timing.hold_ms = 0;
        assert_eq!(h.validate().len(), 1);
        h.timing = HoldTiming { hold_ms: 2, release_ms: 2 };
        assert!(h.validate()[0].contains("at least"));
        h.repeat.enabled = false;
        assert!(h.validate().is_empty(), "one-shot may be short");
    }

    #[test]
    fn presets() {
        assert_eq!(Preset::detect(&Preset::Fast.timing()), Preset::Fast);
        assert_eq!(Preset::detect(&Preset::Stable.timing()), Preset::Stable);
        assert_eq!(Preset::detect(&HoldTiming { hold_ms: 121, release_ms: 60 }), Preset::Custom);
        let mut c = Config::default();
        c.apply_preset(Preset::Fast);
        assert_eq!(c.hold.timing, HoldTiming { hold_ms: 60, release_ms: 30 });
    }

    #[test]
    fn atomic_write_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.toml");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
