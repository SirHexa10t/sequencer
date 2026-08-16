//! Key and button identity: the enums, the one name table, parsing and display.
//!
//! Keys are identified by **physical position** (USB HID usages), never by the char a
//! layout produces — see the module doc in [`super`]. The name table serves both
//! directions: [`FromStr`] scans every spelling, `Display` takes the canonical first
//! hit. Buttons live here too: a mouse key is identity like any other.

use alloc::boxed::Box;
use core::fmt;
use core::str::FromStr;

/// A key, identified by its physical position on a standard keyboard.
///
/// [`Key::Hid`] is the escape hatch for the long tail — media keys, vendor keys and
/// unusual layouts — carrying a raw USB HID usage code.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[non_exhaustive]
#[allow(
    missing_docs,
    reason = "one-letter and F-number variants name themselves"
)]
pub enum Key {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,

    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,

    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,

    Escape,
    Tab,
    CapsLock,
    Space,
    Enter,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,

    LeftCtrl,
    LeftShift,
    LeftAlt,
    LeftMeta,
    RightCtrl,
    RightShift,
    RightAlt,
    RightMeta,

    Minus,
    Equal,
    LeftBracket,
    RightBracket,
    Backslash,
    Semicolon,
    Quote,
    Grave,
    Comma,
    Period,
    Slash,

    PrintScreen,
    ScrollLock,
    Pause,

    VolumeUp,
    VolumeDown,
    Mute,
    PlayPause,
    NextTrack,
    PrevTrack,
    BrightnessUp,
    BrightnessDown,

    NumLock,
    KeypadDivide,
    KeypadMultiply,
    KeypadMinus,
    KeypadPlus,
    KeypadEnter,
    KeypadDot,
    Keypad0,
    Keypad1,
    Keypad2,
    Keypad3,
    Keypad4,
    Keypad5,
    Keypad6,
    Keypad7,
    Keypad8,
    Keypad9,

    /// A raw USB HID usage code, for keys with no named variant.
    Hid(u16),
}

/// Canonical name first, then aliases. This table is the single place key names live:
/// [`Key::FromStr`] scans it forwards, [`Key`]'s [`fmt::Display`] takes the first hit.
#[rustfmt::skip]
pub(super) static KEY_NAMES: &[(&str, Key)] = &[
    ("a", Key::A), ("b", Key::B), ("c", Key::C), ("d", Key::D), ("e", Key::E),
    ("f", Key::F), ("g", Key::G), ("h", Key::H), ("i", Key::I), ("j", Key::J),
    ("k", Key::K), ("l", Key::L), ("m", Key::M), ("n", Key::N), ("o", Key::O),
    ("p", Key::P), ("q", Key::Q), ("r", Key::R), ("s", Key::S), ("t", Key::T),
    ("u", Key::U), ("v", Key::V), ("w", Key::W), ("x", Key::X), ("y", Key::Y),
    ("z", Key::Z),

    ("0", Key::Num0), ("1", Key::Num1), ("2", Key::Num2), ("3", Key::Num3),
    ("4", Key::Num4), ("5", Key::Num5), ("6", Key::Num6), ("7", Key::Num7),
    ("8", Key::Num8), ("9", Key::Num9),

    ("f1", Key::F1), ("f2", Key::F2), ("f3", Key::F3), ("f4", Key::F4),
    ("f5", Key::F5), ("f6", Key::F6), ("f7", Key::F7), ("f8", Key::F8),
    ("f9", Key::F9), ("f10", Key::F10), ("f11", Key::F11), ("f12", Key::F12),
    ("f13", Key::F13), ("f14", Key::F14), ("f15", Key::F15), ("f16", Key::F16),
    ("f17", Key::F17), ("f18", Key::F18), ("f19", Key::F19), ("f20", Key::F20),
    ("f21", Key::F21), ("f22", Key::F22), ("f23", Key::F23), ("f24", Key::F24),

    ("escape", Key::Escape), ("esc", Key::Escape),
    ("tab", Key::Tab),
    ("capslock", Key::CapsLock), ("caps", Key::CapsLock),
    ("space", Key::Space), ("spacebar", Key::Space),
    ("enter", Key::Enter), ("return", Key::Enter),
    ("backspace", Key::Backspace),
    ("delete", Key::Delete), ("del", Key::Delete),
    ("insert", Key::Insert), ("ins", Key::Insert),
    ("home", Key::Home), ("end", Key::End),
    ("pageup", Key::PageUp), ("pgup", Key::PageUp),
    ("pagedown", Key::PageDown), ("pgdn", Key::PageDown),
    ("up", Key::Up), ("down", Key::Down), ("left", Key::Left), ("right", Key::Right),

    // Canonical first: the short forms are what a binds file writes and what the
    // keyboard drawing shows, so they are also what Display prints.
    ("ctrl", Key::LeftCtrl), ("lctrl", Key::LeftCtrl), ("leftctrl", Key::LeftCtrl),
    ("shift", Key::LeftShift), ("lshift", Key::LeftShift), ("leftshift", Key::LeftShift),
    ("alt", Key::LeftAlt), ("lalt", Key::LeftAlt), ("leftalt", Key::LeftAlt),
    ("meta", Key::LeftMeta), ("lmeta", Key::LeftMeta), ("leftmeta", Key::LeftMeta),
    ("super", Key::LeftMeta), ("win", Key::LeftMeta),
    ("rctrl", Key::RightCtrl), ("rightctrl", Key::RightCtrl),
    ("rshift", Key::RightShift), ("rightshift", Key::RightShift),
    ("ralt", Key::RightAlt), ("rightalt", Key::RightAlt), ("altgr", Key::RightAlt),
    ("rmeta", Key::RightMeta), ("rightmeta", Key::RightMeta),

    // Char keys go by their char — that is what a user types in a binds file — with the
    // spelled-out word as a parse alias for contexts where punctuation is awkward.
    // The SHIFTED char names the same physical key: writing `{` means the `[` key, the
    // way writing `A` means the `a` key. Shift is never implied; chord it explicitly.
    ("-", Key::Minus), ("_", Key::Minus), ("minus", Key::Minus),
    ("=", Key::Equal), ("+", Key::Equal), ("equal", Key::Equal),
    ("[", Key::LeftBracket), ("{", Key::LeftBracket), ("leftbracket", Key::LeftBracket),
    ("]", Key::RightBracket), ("}", Key::RightBracket), ("rightbracket", Key::RightBracket),
    ("\\", Key::Backslash), ("|", Key::Backslash), ("backslash", Key::Backslash),
    (";", Key::Semicolon), (":", Key::Semicolon), ("semicolon", Key::Semicolon),
    ("'", Key::Quote), ("\"", Key::Quote), ("quote", Key::Quote),
    ("`", Key::Grave), ("~", Key::Grave), ("grave", Key::Grave),
    (",", Key::Comma), ("<", Key::Comma), ("comma", Key::Comma),
    (".", Key::Period), (">", Key::Period), ("period", Key::Period),
    ("/", Key::Slash), ("?", Key::Slash), ("slash", Key::Slash),
    // The digit row's shifted chars, same rule: `!` is the `1` key.
    ("!", Key::Num1), ("@", Key::Num2), ("#", Key::Num3), ("$", Key::Num4),
    ("%", Key::Num5), ("^", Key::Num6), ("&", Key::Num7), ("*", Key::Num8),
    ("(", Key::Num9), (")", Key::Num0),

    ("volume-up", Key::VolumeUp), ("volume-down", Key::VolumeDown), ("mute", Key::Mute),
    ("play-pause", Key::PlayPause), ("next-track", Key::NextTrack),
    ("prev-track", Key::PrevTrack),
    ("brightness-up", Key::BrightnessUp), ("brightness-down", Key::BrightnessDown),

    ("printscreen", Key::PrintScreen), ("prtsc", Key::PrintScreen),
    ("scrolllock", Key::ScrollLock),
    ("pause", Key::Pause),

    ("numlock", Key::NumLock),
    ("kpdivide", Key::KeypadDivide), ("kpmultiply", Key::KeypadMultiply),
    ("kpminus", Key::KeypadMinus), ("kpplus", Key::KeypadPlus),
    ("kpenter", Key::KeypadEnter), ("kpdot", Key::KeypadDot),
    ("kp0", Key::Keypad0), ("kp1", Key::Keypad1), ("kp2", Key::Keypad2),
    ("kp3", Key::Keypad3), ("kp4", Key::Keypad4), ("kp5", Key::Keypad5),
    ("kp6", Key::Keypad6), ("kp7", Key::Keypad7), ("kp8", Key::Keypad8),
    ("kp9", Key::Keypad9),
];

/// Returned when a string does not name a key.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("unknown key {input:?} (try a name like `f9`, `a`, `lctrl`, or `hid:0x00E7`)")]
pub struct KeyParseError {
    /// The text that could not be parsed.
    pub input: Box<str>,
}

impl FromStr for Key {
    type Err = KeyParseError;

    /// Parses a key name, case-insensitively.
    ///
    /// Accepts canonical names (`f9`, `,`), aliases (`esc`, `ctrl`, `pgdn`), the shifted
    /// char for the same key (`{` is the `[` key), and `hid:<number>` in decimal or `0x`
    /// hex for anything unnamed. Within a multi-character name, `-` and `_` are
    /// interchangeable and optional: `volume-up` == `volume_up` == `volumeup`. Single
    /// characters are exempt from that rule — `-` and `_` are themselves the minus key.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || KeyParseError { input: s.into() };
        let trimmed = s.trim();

        if let Some(rest) = strip_prefix_ignore_case(trimmed, "hid:") {
            let usage = match rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
                Some(hex) => u16::from_str_radix(hex, 16).map_err(|_| err())?,
                None => rest.parse::<u16>().map_err(|_| err())?,
            };
            return Ok(Self::Hid(usage));
        }

        if let Some(key) = KEY_NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(trimmed))
            .map(|(_, key)| *key)
        {
            return Ok(key);
        }
        // Second pass, separators ignored: `volume_up` and `volumeup` land on
        // `volume-up`. Only for multi-character names — `-` alone IS the minus key.
        if trimmed.chars().count() > 1 {
            let wanted = fold_separators(trimmed);
            if let Some(key) = KEY_NAMES
                .iter()
                .filter(|(name, _)| name.chars().count() > 1)
                .find(|(name, _)| fold_separators(name) == wanted)
                .map(|(_, key)| *key)
            {
                return Ok(key);
            }
        }
        Err(err())
    }
}

impl fmt::Display for Key {
    /// The name a user would write, cased the way the key is *labelled*: `F9`, not `f9`. The
    /// parse table is lowercase because input is matched case-insensitively; output is read by
    /// someone looking at a keyboard.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match KEY_NAMES.iter().find(|(_, key)| key == self) {
            Some((name, _)) if is_function_key(name) => {
                write!(f, "F{}", &name[1..])
            }
            Some((name, _)) => f.write_str(name),
            // Only reachable for `Hid`, which by construction has no table entry.
            None => match self {
                Self::Hid(usage) => write!(f, "hid:{usage:#06x}"),
                other => write!(f, "{other:?}"),
            },
        }
    }
}

impl Key {
    /// Every named key, each once, with its canonical spelling — the one [`fmt::Display`]
    /// prints and documentation shows. Tools that list "everything bindable" iterate
    /// this instead of copying names, so a key added to the table appears in their
    /// output without anyone remembering to update it.
    pub fn named() -> impl Iterator<Item = (Self, &'static str)> {
        KEY_NAMES
            .iter()
            .enumerate()
            .filter(|(index, (_, key))| !KEY_NAMES[..*index].iter().any(|(_, prior)| prior == key))
            .map(|(_, (name, key))| (*key, *name))
    }

    /// The canonical spelling, if this key has one. `None` only for [`Key::Hid`], whose
    /// name is computed (`hid:0x…`) rather than tabled.
    #[must_use]
    pub fn canonical_name(self) -> Option<&'static str> {
        KEY_NAMES
            .iter()
            .find(|(_, key)| *key == self)
            .map(|(name, _)| *name)
    }
}

/// Lowercases and drops `-`/`_`, so separator spelling never decides a match.
pub(super) fn fold_separators(name: &str) -> alloc::string::String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a table name is a function key (`f` followed by digits) — `f9` yes, `find` no.
fn is_function_key(name: &str) -> bool {
    name.strip_prefix('f')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// `str::strip_prefix`, but comparing ASCII case-insensitively.
fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let (head, rest) = s.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

/// A mouse button.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[non_exhaustive]
pub enum Button {
    /// Primary button.
    #[default]
    Left,
    /// Scroll-wheel button.
    Middle,
    /// Secondary button.
    Right,
    /// Thumb button, conventionally "back".
    Back,
    /// Thumb button, conventionally "forward".
    Forward,
    /// Any further button, by platform-reported index.
    Other(u8),
}

impl fmt::Display for Button {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Left => f.write_str("left"),
            Self::Middle => f.write_str("middle"),
            Self::Right => f.write_str("right"),
            Self::Back => f.write_str("back"),
            Self::Forward => f.write_str("forward"),
            Self::Other(n) => write!(f, "button{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule a binds file leans on: a shifted char names the same physical key, so
    /// `{` is the `[` key and `!` is the `1` key — shift is never implied.
    #[test]
    fn a_shifted_char_names_its_base_key() {
        assert_eq!("{".parse::<Key>().unwrap(), Key::LeftBracket);
        assert_eq!("}".parse::<Key>().unwrap(), Key::RightBracket);
        assert_eq!("!".parse::<Key>().unwrap(), Key::Num1);
        assert_eq!(")".parse::<Key>().unwrap(), Key::Num0);
        assert_eq!("+".parse::<Key>().unwrap(), Key::Equal);
        assert_eq!("\"".parse::<Key>().unwrap(), Key::Quote);
        assert_eq!("?".parse::<Key>().unwrap(), Key::Slash);
        assert_eq!("~".parse::<Key>().unwrap(), Key::Grave);
        assert_eq!("|".parse::<Key>().unwrap(), Key::Backslash);
    }

    /// Char keys display as their char — that is what a user writes in a binds file —
    /// and word-named keys keep their words.
    #[test]
    fn char_keys_display_as_their_char() {
        use alloc::string::ToString as _;
        assert_eq!(Key::Comma.to_string(), ",");
        assert_eq!(Key::LeftBracket.to_string(), "[");
        assert_eq!(Key::Minus.to_string(), "-");
        assert_eq!(Key::VolumeUp.to_string(), "volume-up");
        assert_eq!(Key::Space.to_string(), "space");
    }

    /// `-` and `_` inside a multi-character name are interchangeable and optional;
    /// alone, each IS the minus key and the rule must not eat them.
    #[test]
    fn separators_are_optional_inside_names_but_are_keys_alone() {
        for name in [
            "volume-up",
            "volume_up",
            "volumeup",
            "VOLUME_UP",
            "Volume-Up",
        ] {
            assert_eq!(name.parse::<Key>().unwrap(), Key::VolumeUp, "{name}");
        }
        assert_eq!("page-up".parse::<Key>().unwrap(), Key::PageUp);
        assert_eq!("-".parse::<Key>().unwrap(), Key::Minus);
        assert_eq!("_".parse::<Key>().unwrap(), Key::Minus);
    }

    use alloc::format;

    #[test]
    fn key_names_round_trip_through_display_and_parse() {
        for (_, key) in KEY_NAMES {
            let shown = format!("{key}");
            assert_eq!(
                shown.parse::<Key>().as_ref(),
                Ok(key),
                "{key:?} displayed as {shown:?} but did not parse back"
            );
        }
    }

    #[test]
    fn parsing_is_case_insensitive_and_trims() {
        assert_eq!("F9".parse(), Ok(Key::F9));
        assert_eq!("  f9  ".parse(), Ok(Key::F9));
        assert_eq!("PgDn".parse(), Ok(Key::PageDown));
    }

    #[test]
    fn aliases_resolve_to_the_canonical_key() {
        assert_eq!("esc".parse(), Ok(Key::Escape));
        assert_eq!("ctrl".parse(), Ok(Key::LeftCtrl));
        assert_eq!("lctrl".parse(), Ok(Key::LeftCtrl));
        assert_eq!("altgr".parse(), Ok(Key::RightAlt));
    }

    #[test]
    fn hid_escape_hatch_parses_decimal_and_hex() {
        assert_eq!("hid:231".parse(), Ok(Key::Hid(231)));
        assert_eq!("hid:0x00E7".parse(), Ok(Key::Hid(0x00E7)));
        assert_eq!("HID:0XE7".parse(), Ok(Key::Hid(0xE7)));
        assert_eq!(format!("{}", Key::Hid(0xE7)), "hid:0x00e7");
    }

    #[test]
    fn unknown_names_report_the_input() {
        let err = "nosuchkey".parse::<Key>().unwrap_err();
        assert_eq!(&*err.input, "nosuchkey");
        // An empty `hid:` payload is a parse failure, not a zero usage code.
        assert!("hid:".parse::<Key>().is_err());
        assert!("hid:notanumber".parse::<Key>().is_err());
        assert!("hid:99999999".parse::<Key>().is_err());
    }

    #[test]
    fn strip_prefix_ignore_case_handles_short_and_multibyte_input() {
        assert_eq!(strip_prefix_ignore_case("HID:9", "hid:"), Some("9"));
        assert_eq!(strip_prefix_ignore_case("hi", "hid:"), None);
        // Must not panic when the prefix length lands inside a multi-byte character.
        assert_eq!(strip_prefix_ignore_case("é", "hid:"), None);
        assert_eq!(strip_prefix_ignore_case("héd:", "hid:"), None);
    }
}
