//! X11 keysyms, modifier sets and key-combination parsing/formatting.
//!
//! Keys are identified by their X11 *keysym* (layout-independent symbol). The keysym is
//! resolved to a physical keycode through the live X keymap right before execution
//! (see `input::Keymap`), so sequences survive keyboard-layout changes between runs.

use serde::{Deserialize, Serialize};
use std::fmt;

pub type Keysym = u32;

pub const XK_SHIFT_L: Keysym = 0xffe1;
pub const XK_SHIFT_R: Keysym = 0xffe2;
pub const XK_CONTROL_L: Keysym = 0xffe3;
pub const XK_CONTROL_R: Keysym = 0xffe4;
pub const XK_META_L: Keysym = 0xffe7;
pub const XK_META_R: Keysym = 0xffe8;
pub const XK_ALT_L: Keysym = 0xffe9;
pub const XK_ALT_R: Keysym = 0xffea;
pub const XK_SUPER_L: Keysym = 0xffeb;
pub const XK_SUPER_R: Keysym = 0xffec;
pub const XK_ISO_LEVEL3_SHIFT: Keysym = 0xfe03;
pub const XK_NUM_LOCK: Keysym = 0xff7f;
pub const XK_SCROLL_LOCK: Keysym = 0xff14;
pub const XK_ESCAPE: Keysym = 0xff1b;
pub const XK_TAB: Keysym = 0xff09;
pub const XK_F1: Keysym = 0xffbe;

/// Bit set of the four modifiers a combination may carry.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const CTRL: Mods = Mods(1);
    pub const ALT: Mods = Mods(2);
    pub const SHIFT: Mods = Mods(4);
    pub const SUPER: Mods = Mods(8);
    /// Canonical press/print order.
    pub const ORDER: [Mods; 4] = [Mods::CTRL, Mods::ALT, Mods::SHIFT, Mods::SUPER];

    pub fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }
    pub fn insert(&mut self, other: Mods) {
        self.0 |= other.0;
    }
    pub fn remove(&mut self, other: Mods) {
        self.0 &= !other.0;
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn bits(self) -> u8 {
        self.0
    }
    /// Iterate the set modifiers in canonical order.
    pub fn iter(self) -> impl DoubleEndedIterator<Item = Mods> {
        Mods::ORDER.into_iter().filter(move |m| self.contains(*m))
    }
    /// Left-hand keysym used when the macro has to press this modifier itself.
    pub fn keysym(self) -> Keysym {
        match self {
            Mods::CTRL => XK_CONTROL_L,
            Mods::ALT => XK_ALT_L,
            Mods::SHIFT => XK_SHIFT_L,
            Mods::SUPER => XK_SUPER_L,
            _ => 0,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Mods::CTRL => "Ctrl",
            Mods::ALT => "Alt",
            Mods::SHIFT => "Shift",
            Mods::SUPER => "Super",
            _ => "",
        }
    }
}

/// Which modifier (if any) a keysym *is*.
pub fn modifier_of(sym: Keysym) -> Option<Mods> {
    match sym {
        XK_SHIFT_L | XK_SHIFT_R => Some(Mods::SHIFT),
        XK_CONTROL_L | XK_CONTROL_R => Some(Mods::CTRL),
        XK_ALT_L | XK_ALT_R | XK_META_L | XK_META_R => Some(Mods::ALT),
        XK_SUPER_L | XK_SUPER_R => Some(Mods::SUPER),
        _ => None,
    }
}

pub fn is_modifier(sym: Keysym) -> bool {
    modifier_of(sym).is_some() || sym == XK_ISO_LEVEL3_SHIFT
}

/// Keypad keysyms are sent as the physical keypad key; their meaning depends on NumLock.
pub fn is_keypad(sym: Keysym) -> bool {
    (0xff80..=0xffbd).contains(&sym)
}

/// (X11 keysym name, keysym, display label)
const NAMED: &[(&str, Keysym, &str)] = &[
    ("BackSpace", 0xff08, "Backspace"),
    ("Tab", 0xff09, "Tab"),
    ("Return", 0xff0d, "Enter"),
    ("Pause", 0xff13, "Pause"),
    ("Scroll_Lock", 0xff14, "ScrollLock"),
    ("Escape", 0xff1b, "Esc"),
    ("Delete", 0xffff, "Delete"),
    ("Home", 0xff50, "Home"),
    ("Left", 0xff51, "Left"),
    ("Up", 0xff52, "Up"),
    ("Right", 0xff53, "Right"),
    ("Down", 0xff54, "Down"),
    ("Prior", 0xff55, "PageUp"),
    ("Next", 0xff56, "PageDown"),
    ("End", 0xff57, "End"),
    ("Print", 0xff61, "PrintScreen"),
    ("Insert", 0xff63, "Insert"),
    ("Menu", 0xff67, "Menu"),
    ("Num_Lock", 0xff7f, "NumLock"),
    ("KP_Enter", 0xff8d, "NumEnter"),
    ("KP_Home", 0xff95, "NumHome"),
    ("KP_Left", 0xff96, "NumLeft"),
    ("KP_Up", 0xff97, "NumUp"),
    ("KP_Right", 0xff98, "NumRight"),
    ("KP_Down", 0xff99, "NumDown"),
    ("KP_Prior", 0xff9a, "NumPageUp"),
    ("KP_Next", 0xff9b, "NumPageDown"),
    ("KP_End", 0xff9c, "NumEnd"),
    ("KP_Begin", 0xff9d, "NumBegin"),
    ("KP_Insert", 0xff9e, "NumInsert"),
    ("KP_Delete", 0xff9f, "NumDelete"),
    ("KP_Multiply", 0xffaa, "Num*"),
    ("KP_Add", 0xffab, "Num+"),
    ("KP_Separator", 0xffac, "NumSeparator"),
    ("KP_Subtract", 0xffad, "Num-"),
    ("KP_Decimal", 0xffae, "Num."),
    ("KP_Divide", 0xffaf, "Num/"),
    ("KP_0", 0xffb0, "Num0"),
    ("KP_1", 0xffb1, "Num1"),
    ("KP_2", 0xffb2, "Num2"),
    ("KP_3", 0xffb3, "Num3"),
    ("KP_4", 0xffb4, "Num4"),
    ("KP_5", 0xffb5, "Num5"),
    ("KP_6", 0xffb6, "Num6"),
    ("KP_7", 0xffb7, "Num7"),
    ("KP_8", 0xffb8, "Num8"),
    ("KP_9", 0xffb9, "Num9"),
    ("KP_Equal", 0xffbd, "Num="),
    ("Shift_L", XK_SHIFT_L, "Shift"),
    ("Shift_R", XK_SHIFT_R, "RShift"),
    ("Control_L", XK_CONTROL_L, "Ctrl"),
    ("Control_R", XK_CONTROL_R, "RCtrl"),
    ("Caps_Lock", 0xffe5, "CapsLock"),
    ("Meta_L", XK_META_L, "Meta"),
    ("Meta_R", XK_META_R, "RMeta"),
    ("Alt_L", XK_ALT_L, "Alt"),
    ("Alt_R", XK_ALT_R, "RAlt"),
    ("Super_L", XK_SUPER_L, "Super"),
    ("Super_R", XK_SUPER_R, "RSuper"),
    ("ISO_Level3_Shift", XK_ISO_LEVEL3_SHIFT, "AltGr"),
    ("space", 0x20, "Space"),
    ("exclam", 0x21, "!"),
    ("quotedbl", 0x22, "\""),
    ("numbersign", 0x23, "#"),
    ("dollar", 0x24, "$"),
    ("percent", 0x25, "%"),
    ("ampersand", 0x26, "&"),
    ("apostrophe", 0x27, "'"),
    ("parenleft", 0x28, "("),
    ("parenright", 0x29, ")"),
    ("asterisk", 0x2a, "*"),
    ("plus", 0x2b, "+"),
    ("comma", 0x2c, ","),
    ("minus", 0x2d, "-"),
    ("period", 0x2e, "."),
    ("slash", 0x2f, "/"),
    ("colon", 0x3a, ":"),
    ("semicolon", 0x3b, ";"),
    ("less", 0x3c, "<"),
    ("equal", 0x3d, "="),
    ("greater", 0x3e, ">"),
    ("question", 0x3f, "?"),
    ("at", 0x40, "@"),
    ("bracketleft", 0x5b, "["),
    ("backslash", 0x5c, "\\"),
    ("bracketright", 0x5d, "]"),
    ("asciicircum", 0x5e, "^"),
    ("underscore", 0x5f, "_"),
    ("grave", 0x60, "`"),
    ("braceleft", 0x7b, "{"),
    ("bar", 0x7c, "|"),
    ("braceright", 0x7d, "}"),
    ("asciitilde", 0x7e, "~"),
];

/// Extra spellings accepted when parsing (normalized: lowercase, no spaces/underscores).
const ALIASES: &[(&str, Keysym)] = &[
    ("esc", XK_ESCAPE),
    ("enter", 0xff0d),
    ("return", 0xff0d),
    ("backspace", 0xff08),
    ("del", 0xffff),
    ("ins", 0xff63),
    ("pgup", 0xff55),
    ("pageup", 0xff55),
    ("pgdn", 0xff56),
    ("pagedown", 0xff56),
    ("printscreen", 0xff61),
    ("prtsc", 0xff61),
    ("capslock", 0xffe5),
    ("numlock", XK_NUM_LOCK),
    ("scrolllock", XK_SCROLL_LOCK),
    ("shift", XK_SHIFT_L),
    ("lshift", XK_SHIFT_L),
    ("rshift", XK_SHIFT_R),
    ("ctrl", XK_CONTROL_L),
    ("control", XK_CONTROL_L),
    ("lctrl", XK_CONTROL_L),
    ("rctrl", XK_CONTROL_R),
    ("alt", XK_ALT_L),
    ("lalt", XK_ALT_L),
    ("ralt", XK_ALT_R),
    ("altgr", XK_ISO_LEVEL3_SHIFT),
    ("super", XK_SUPER_L),
    ("win", XK_SUPER_L),
    ("lsuper", XK_SUPER_L),
    ("rsuper", XK_SUPER_R),
    ("space", 0x20),
    ("spacebar", 0x20),
    ("numenter", 0xff8d),
    ("kpenter", 0xff8d),
    ("numplus", 0xffab),
    ("numminus", 0xffad),
    ("numstar", 0xffaa),
    ("numslash", 0xffaf),
    ("numdot", 0xffae),
];

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn function_key(n: u32) -> Option<Keysym> {
    (1..=24).contains(&n).then(|| XK_F1 + n - 1)
}

/// Parse a single key name (no modifiers) into a keysym.
pub fn parse_key_name(raw: &str) -> Option<Keysym> {
    let token = raw.trim();
    if token.is_empty() {
        return None;
    }
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_lowercase() as Keysym);
        }
        if c.is_ascii_graphic() {
            return Some(c as Keysym);
        }
        return None;
    }
    if let Some(hex) = token.strip_prefix("0x").or_else(|| token.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok().filter(|v| *v > 0 && *v <= 0x1fff_ffff);
    }
    let norm = normalize(token);
    if let Some(rest) = norm.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            return function_key(n);
        }
    }
    if let Some(rest) = norm.strip_prefix("num").or_else(|| norm.strip_prefix("kp")) {
        if let Ok(n) = rest.parse::<u32>() {
            if n <= 9 {
                return Some(0xffb0 + n);
            }
        }
    }
    for (name, sym, label) in NAMED {
        if normalize(name) == norm || normalize(label) == norm {
            return Some(*sym);
        }
    }
    ALIASES.iter().find(|(alias, _)| *alias == norm).map(|(_, sym)| *sym)
}

/// Human-readable label for a keysym.
pub fn key_label(sym: Keysym) -> String {
    if (0x61..=0x7a).contains(&sym) {
        return ((sym as u8) as char).to_ascii_uppercase().to_string();
    }
    if (0x30..=0x39).contains(&sym) {
        return ((sym as u8) as char).to_string();
    }
    if (XK_F1..XK_F1 + 24).contains(&sym) {
        return format!("F{}", sym - XK_F1 + 1);
    }
    if let Some((_, _, label)) = NAMED.iter().find(|(_, s, _)| *s == sym) {
        return (*label).to_string();
    }
    if (0x41..=0x5a).contains(&sym) {
        return ((sym as u8) as char).to_string();
    }
    format!("0x{sym:x}")
}

/// Parse a modifier token used as a combination prefix ("Ctrl", "shift", ...).
fn parse_modifier_prefix(token: &str) -> Option<Mods> {
    match normalize(token).as_str() {
        "ctrl" | "control" | "ctl" => Some(Mods::CTRL),
        "alt" | "mod1" | "meta" => Some(Mods::ALT),
        "shift" => Some(Mods::SHIFT),
        "super" | "win" | "windows" | "mod4" | "cmd" | "logo" => Some(Mods::SUPER),
        _ => None,
    }
}

/// A key plus optional modifiers, e.g. `Shift+E`, `Ctrl+Alt+F5`, `Num+`, or a lone `Shift`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct KeyCombo {
    pub mods: Mods,
    pub key: Keysym,
}

impl KeyCombo {
    pub const fn new(key: Keysym) -> Self {
        KeyCombo { mods: Mods::NONE, key }
    }
    pub fn with(mods: Mods, key: Keysym) -> Self {
        KeyCombo { mods, key }
    }

    /// Parse text such as `Ctrl+Shift+E`. Modifier prefixes are stripped greedily from the
    /// left so that keys whose label contains `+` (`Num+`, `+`) still parse.
    pub fn parse(text: &str) -> Result<KeyCombo, String> {
        let mut rest = text.trim();
        if rest.is_empty() {
            return Err("No key given".into());
        }
        let mut mods = Mods::NONE;
        while let Some(idx) = rest.find('+') {
            let (head, tail) = (&rest[..idx], &rest[idx + 1..]);
            if tail.trim().is_empty() {
                break;
            }
            match parse_modifier_prefix(head) {
                Some(m) => {
                    mods.insert(m);
                    rest = tail.trim_start();
                }
                None => break,
            }
        }
        let key = parse_key_name(rest).ok_or_else(|| format!("Unknown key \"{}\"", rest.trim()))?;
        // `Shift+Shift` is meaningless; a lone modifier key is fine.
        if let Some(m) = modifier_of(key) {
            if mods.contains(m) {
                return Err(format!("{} is listed twice", m.label()));
            }
        }
        Ok(KeyCombo { mods, key })
    }

    /// All keysyms this combination presses, modifiers first.
    pub fn keysyms(&self) -> Vec<Keysym> {
        let mut out: Vec<Keysym> = self.mods.iter().map(Mods::keysym).collect();
        out.push(self.key);
        out
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in self.mods.iter() {
            write!(f, "{}+", m.label())?;
        }
        write!(f, "{}", key_label(self.key))
    }
}

impl Serialize for KeyCombo {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeyCombo {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        KeyCombo::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Mouse buttons usable in custom sequences (X11 core button numbers).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    #[default]
    Left,
    Middle,
    Right,
}

impl MouseButton {
    pub const ALL: [MouseButton; 3] = [MouseButton::Left, MouseButton::Middle, MouseButton::Right];

    pub fn x11_button(self) -> u8 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
        }
    }
    /// Linux evdev code (BTN_LEFT, BTN_MIDDLE, BTN_RIGHT).
    pub fn evdev_code(self) -> u16 {
        match self {
            MouseButton::Left => 0x110,
            MouseButton::Middle => 0x112,
            MouseButton::Right => 0x111,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            MouseButton::Left => "Left",
            MouseButton::Middle => "Middle",
            MouseButton::Right => "Right",
        }
    }
    pub fn index(self) -> usize {
        MouseButton::ALL.iter().position(|b| *b == self).unwrap_or(0)
    }
    pub fn from_index(i: usize) -> MouseButton {
        MouseButton::ALL.get(i).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_keys() {
        assert_eq!(parse_key_name("5"), Some(0x35));
        assert_eq!(parse_key_name("e"), Some(0x65));
        assert_eq!(parse_key_name("E"), Some(0x65));
        assert_eq!(parse_key_name("Tab"), Some(XK_TAB));
        assert_eq!(parse_key_name("tab"), Some(XK_TAB));
        assert_eq!(parse_key_name("F6"), Some(XK_F1 + 5));
        assert_eq!(parse_key_name("f24"), Some(XK_F1 + 23));
        assert_eq!(parse_key_name("F25"), None);
        assert_eq!(parse_key_name("KP_5"), Some(0xffb5));
        assert_eq!(parse_key_name("Num 5"), Some(0xffb5));
        assert_eq!(parse_key_name("PgUp"), Some(0xff55));
        assert_eq!(parse_key_name("Prior"), Some(0xff55));
        assert_eq!(parse_key_name("0xffbe"), Some(0xffbe));
        assert_eq!(parse_key_name("+"), Some(0x2b));
        assert_eq!(parse_key_name("Shif"), None);
    }

    #[test]
    fn parses_combinations() {
        let c = KeyCombo::parse("Shift+E").unwrap();
        assert_eq!(c, KeyCombo::with(Mods::SHIFT, 0x65));
        let c = KeyCombo::parse("ctrl + alt + F5").unwrap();
        assert!(c.mods.contains(Mods::CTRL) && c.mods.contains(Mods::ALT));
        assert_eq!(c.key, XK_F1 + 4);
        assert_eq!(KeyCombo::parse("Num+").unwrap().key, 0xffab);
        assert_eq!(KeyCombo::parse("Ctrl+Num+").unwrap(), KeyCombo::with(Mods::CTRL, 0xffab));
        assert_eq!(KeyCombo::parse("Shift++").unwrap(), KeyCombo::with(Mods::SHIFT, 0x2b));
        assert_eq!(KeyCombo::parse("Shift").unwrap(), KeyCombo::new(XK_SHIFT_L));
        assert_eq!(KeyCombo::parse("Ctrl+Shift").unwrap(), KeyCombo::with(Mods::CTRL, XK_SHIFT_L));
        assert!(KeyCombo::parse("Shift+Shift").is_err());
        assert!(KeyCombo::parse("").is_err());
        assert!(KeyCombo::parse("Ctrl+Nope").is_err());
    }

    #[test]
    fn display_round_trips() {
        for text in ["5", "Tab", "Shift+E", "Ctrl+Alt+Shift+Super+F12", "Num+", "Ctrl+Num-", "Space", "AltGr", "\\", "'"] {
            let combo = KeyCombo::parse(text).unwrap();
            assert_eq!(KeyCombo::parse(&combo.to_string()).unwrap(), combo, "{text}");
        }
        assert_eq!(KeyCombo::parse("shift+e").unwrap().to_string(), "Shift+E");
    }

    #[test]
    fn every_named_key_round_trips() {
        for (_, sym, _) in NAMED {
            let label = key_label(*sym);
            assert_eq!(parse_key_name(&label), Some(*sym), "label {label}");
        }
    }

    #[test]
    fn modifier_classification() {
        assert_eq!(modifier_of(XK_SHIFT_R), Some(Mods::SHIFT));
        assert!(is_modifier(XK_ISO_LEVEL3_SHIFT));
        assert!(!is_modifier(XK_TAB));
        assert!(is_keypad(0xffb5));
        assert!(!is_keypad(XK_F1));
    }
}
