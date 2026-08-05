//! Events flowing *in*: what the user physically did.
//!
//! Keys are identified by **physical position**, following the USB HID keyboard usage
//! table, not by the character they happen to produce. Backends own the mapping to
//! whatever their platform names keys by — evdev codes on Linux, and X keycodes (which are
//! evdev codes plus a fixed offset) for the X11 backend.
//!
//! This matters more than it looks. A keysym-based identity cannot survive a layout
//! change and does not correspond to any position on evdev's keyboard, so building on one
//! would mean rewriting every backend's mapping table the first time a second platform
//! lands.

use alloc::boxed::Box;
use core::fmt;
use core::str::FromStr;

use crate::time::Timestamp;

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
static KEY_NAMES: &[(&str, Key)] = &[
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

    ("leftctrl", Key::LeftCtrl), ("lctrl", Key::LeftCtrl), ("ctrl", Key::LeftCtrl),
    ("leftshift", Key::LeftShift), ("lshift", Key::LeftShift), ("shift", Key::LeftShift),
    ("leftalt", Key::LeftAlt), ("lalt", Key::LeftAlt), ("alt", Key::LeftAlt),
    ("leftmeta", Key::LeftMeta), ("lmeta", Key::LeftMeta), ("meta", Key::LeftMeta),
    ("super", Key::LeftMeta), ("win", Key::LeftMeta),
    ("rightctrl", Key::RightCtrl), ("rctrl", Key::RightCtrl),
    ("rightshift", Key::RightShift), ("rshift", Key::RightShift),
    ("rightalt", Key::RightAlt), ("ralt", Key::RightAlt), ("altgr", Key::RightAlt),
    ("rightmeta", Key::RightMeta), ("rmeta", Key::RightMeta),

    ("minus", Key::Minus), ("-", Key::Minus),
    ("equal", Key::Equal), ("=", Key::Equal),
    ("leftbracket", Key::LeftBracket), ("[", Key::LeftBracket),
    ("rightbracket", Key::RightBracket), ("]", Key::RightBracket),
    ("backslash", Key::Backslash), ("\\", Key::Backslash),
    ("semicolon", Key::Semicolon), (";", Key::Semicolon),
    ("quote", Key::Quote), ("'", Key::Quote),
    ("grave", Key::Grave), ("`", Key::Grave),
    ("comma", Key::Comma), (",", Key::Comma),
    ("period", Key::Period), (".", Key::Period),
    ("slash", Key::Slash), ("/", Key::Slash),

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
    /// Accepts canonical names (`f9`), aliases (`esc`, `ctrl`, `pgdn`), bare punctuation
    /// (`,`), and `hid:<number>` in decimal or `0x` hex for anything unnamed.
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

        KEY_NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(trimmed))
            .map(|(_, key)| *key)
            .ok_or_else(err)
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

/// Where an event came from.
///
/// [`EventOrigin::Synthetic`] carries AutoHotkey's send-level idea: with a binding's input
/// level and an emitted event's level both defaulting to zero, and a binding only firing
/// on synthetic events *strictly above* its level, the engine cannot retrigger itself.
/// The safe behaviour falls out of the arithmetic instead of needing a special case, and
/// deliberate remap cascades stay possible later by raising the level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventOrigin {
    /// Real hardware — or an event the backend could not attribute, since failing safe
    /// means treating an unknown event as a real user action.
    Physical,
    /// Injected by software, at the given send level.
    Synthetic {
        /// The emitting binding's send level.
        level: u8,
    },
}

/// What happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum EventKind {
    /// A key went down.
    KeyDown(Key),
    /// A key came up.
    KeyUp(Key),
    /// A mouse button went down.
    ButtonDown(Button),
    /// A mouse button came up.
    ButtonUp(Button),
    /// The wheel moved.
    Scroll {
        /// Horizontal detents; positive is right.
        dx: i32,
        /// Vertical detents; positive is up.
        dy: i32,
    },
    /// The cursor moved to an absolute position.
    Motion {
        /// Horizontal position in pixels.
        x: i32,
        /// Vertical position in pixels.
        y: i32,
    },
}

/// One thing the user did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InputEvent {
    /// The backend's hardware timestamp if it has one, otherwise arrival time.
    ///
    /// Repeat cadence phase-locks to this rather than to when the runner got around to
    /// processing it, so an event delivered late does not shift the whole click train.
    pub at: Timestamp,
    /// Whether this came from hardware or from an injector.
    pub origin: EventOrigin,
    /// What happened.
    pub kind: EventKind,
}

impl InputEvent {
    /// A physical event at `at`.
    #[must_use]
    pub const fn physical(at: Timestamp, kind: EventKind) -> Self {
        Self {
            at,
            origin: EventOrigin::Physical,
            kind,
        }
    }

    /// A synthetic event at `at`, tagged with the emitting binding's send `level`.
    #[must_use]
    pub const fn synthetic(at: Timestamp, level: u8, kind: EventKind) -> Self {
        Self {
            at,
            origin: EventOrigin::Synthetic { level },
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
