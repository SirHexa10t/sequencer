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

use crate::emit::Holdable;

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

    /// Which [`Category`] this key belongs to.
    ///
    /// An exhaustive `match` on purpose: adding a `Key` variant refuses to compile until
    /// it picks a category, which is what keeps every generated listing complete.
    #[must_use]
    #[rustfmt::skip]
    pub const fn category(self) -> Category {
        match self {
            Self::A | Self::B | Self::C | Self::D | Self::E | Self::F | Self::G | Self::H
            | Self::I | Self::J | Self::K | Self::L | Self::M | Self::N | Self::O | Self::P
            | Self::Q | Self::R | Self::S | Self::T | Self::U | Self::V | Self::W | Self::X
            | Self::Y | Self::Z
            | Self::Num0 | Self::Num1 | Self::Num2 | Self::Num3 | Self::Num4 | Self::Num5
            | Self::Num6 | Self::Num7 | Self::Num8 | Self::Num9
            | Self::F1 | Self::F2 | Self::F3 | Self::F4 | Self::F5 | Self::F6 | Self::F7
            | Self::F8 | Self::F9 | Self::F10 | Self::F11 | Self::F12 | Self::F13 | Self::F14
            | Self::F15 | Self::F16 | Self::F17 | Self::F18 | Self::F19 | Self::F20
            | Self::F21 | Self::F22 | Self::F23 | Self::F24
            | Self::Escape | Self::Tab | Self::CapsLock | Self::Space | Self::Enter
            | Self::Backspace
            | Self::Minus | Self::Equal | Self::LeftBracket | Self::RightBracket
            | Self::Backslash | Self::Semicolon | Self::Quote | Self::Grave | Self::Comma
            | Self::Period | Self::Slash
            // Raw HID escapes have no place in a listing; Main is a harmless home.
            | Self::Hid(_) => Category::Main,
            Self::Insert | Self::Home | Self::End | Self::PageUp | Self::PageDown
            | Self::Delete | Self::PrintScreen | Self::ScrollLock | Self::Pause => Category::Nav,
            Self::Up | Self::Down | Self::Left | Self::Right => Category::Arrow,
            Self::NumLock | Self::KeypadDivide | Self::KeypadMultiply | Self::KeypadMinus
            | Self::KeypadPlus | Self::KeypadEnter | Self::KeypadDot
            | Self::Keypad0 | Self::Keypad1 | Self::Keypad2 | Self::Keypad3 | Self::Keypad4
            | Self::Keypad5 | Self::Keypad6 | Self::Keypad7 | Self::Keypad8
            | Self::Keypad9 => Category::Numpad,
            Self::LeftCtrl | Self::LeftShift | Self::LeftAlt | Self::LeftMeta
            | Self::RightCtrl | Self::RightShift | Self::RightAlt
            | Self::RightMeta => Category::Modifier,
            Self::VolumeUp | Self::VolumeDown | Self::Mute | Self::PlayPause
            | Self::NextTrack | Self::PrevTrack | Self::BrightnessUp
            | Self::BrightnessDown => Category::Media,
        }
    }
}

/// A family of inputs, for listing them separately: media keys are not modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Category {
    /// The keyboard's main block — letters, digits, symbols, F-keys and their
    /// neighbours. Drawn rather than listed.
    Main,
    /// The navigation cluster.
    Nav,
    /// The arrow keys.
    Arrow,
    /// The numeric keypad.
    Numpad,
    /// Ctrl, shift, alt and meta, both hands.
    Modifier,
    /// Volume, playback and brightness keys.
    Media,
    /// Mouse buttons and wheel notches.
    Mouse,
    /// Controller buttons. Reserved: they need the device backend.
    Pad,
}

impl Category {
    /// The label a listing prints for this family.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Nav => "nav",
            Self::Arrow => "arrows",
            Self::Numpad => "numpad",
            Self::Modifier => "modifiers",
            Self::Media => "media",
            Self::Mouse => "mouse",
            Self::Pad => "controller",
        }
    }
}

/// One row of [`InputMap::entries`]: a bindable name, its family, what it resolves to,
/// and a display-only aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEntry {
    /// The canonical binds-file spelling.
    pub name: &'static str,
    /// Which family it belongs to.
    pub category: Category,
    /// What the name resolves to. `None` for reserved spellings — names the format owns
    /// for signals nothing can *send* yet (wheel notches are already heard, as
    /// [`EventKind::Scroll`]).
    pub input: Option<Holdable>,
    /// A parenthetical for listings ("left", "A on Xbox"); empty when none is needed.
    pub gloss: &'static str,
}

/// The two-sided mapping between binds-file names and inputs.
///
/// One direction reads profiles (**name → input**, [`InputMap::input_of`]); the other
/// reports live events (**input → name**, [`InputMap::name_of`]). Keyboard keys are not
/// duplicated here — their names come from the key table's canonical entries (via
/// [`Key::named`]) and their families from [`Key::category`] — so there is exactly one
/// table per fact. Aliases are a *view* over the same table ([`InputMap::aliases_of`],
/// [`InputMap::canonical_of`]) rather than stored again, so the spellings a listing
/// shows and the spellings the parser takes cannot disagree.
#[derive(Debug)]
pub struct InputMap {
    buttons: &'static [(&'static str, Button, &'static str)],
    reserved: &'static [(&'static str, Category, &'static str)],
    key_glosses: &'static [(Key, &'static str)],
}

/// The map itself. See [`InputMap`].
pub static INPUT_MAP: InputMap = InputMap {
    // `mouse1` rather than `left`, which would collide with the arrow key.
    buttons: &[
        ("mouse1", Button::Left, "left"),
        ("mouse2", Button::Right, "right"),
        ("mouse3", Button::Middle, "middle"),
        ("mouse4", Button::Back, "back"),
        ("mouse5", Button::Forward, "forward"),
    ],
    reserved: &[
        ("wheel-up", Category::Mouse, ""),
        ("wheel-down", Category::Mouse, ""),
        ("wheel-left", Category::Mouse, ""),
        ("wheel-right", Category::Mouse, "one step = one notch"),
        // Position-named (evdev's convention): Xbox and Nintendo disagree on letters.
        ("pad-south", Category::Pad, "A on Xbox"),
        ("pad-east", Category::Pad, "B"),
        ("pad-west", Category::Pad, "X"),
        ("pad-north", Category::Pad, "Y"),
        ("pad-l1", Category::Pad, "bumper"),
        ("pad-r1", Category::Pad, "bumper"),
        ("pad-l2", Category::Pad, "trigger"),
        ("pad-r2", Category::Pad, "trigger"),
        ("pad-up", Category::Pad, "d-pad"),
        ("pad-down", Category::Pad, "d-pad"),
        ("pad-left", Category::Pad, "d-pad"),
        ("pad-right", Category::Pad, "d-pad"),
        ("pad-select", Category::Pad, ""),
        ("pad-start", Category::Pad, ""),
        ("pad-guide", Category::Pad, ""),
        ("pad-l3", Category::Pad, "stick click"),
        ("pad-r3", Category::Pad, "stick click"),
    ],
    key_glosses: &[(Key::LeftMeta, "the super / win key")],
};

impl InputMap {
    /// Every entry: named keys first (from the key table), then buttons, then reserved
    /// spellings.
    pub fn entries(&self) -> impl Iterator<Item = InputEntry> + '_ {
        let keys = Key::named().map(|(key, name)| InputEntry {
            name,
            category: key.category(),
            input: Some(Holdable::Key(key)),
            gloss: self
                .key_glosses
                .iter()
                .find(|(glossed, _)| *glossed == key)
                .map_or("", |(_, gloss)| gloss),
        });
        let buttons = self.buttons.iter().map(|(name, button, gloss)| InputEntry {
            name,
            category: Category::Mouse,
            input: Some(Holdable::Button(*button)),
            gloss,
        });
        let reserved = self
            .reserved
            .iter()
            .map(|(name, category, gloss)| InputEntry {
                name,
                category: *category,
                input: None,
                gloss,
            });
        keys.chain(buttons).chain(reserved)
    }

    /// The entries of one family, in listing order.
    pub fn in_category(&self, category: Category) -> impl Iterator<Item = InputEntry> + '_ {
        self.entries()
            .filter(move |entry| entry.category == category)
    }

    /// Name → input: what a binds file's `<key>` token resolves to.
    ///
    /// Buttons match their `mouseN` names; everything else goes through [`Key`]'s
    /// parser, aliases and `hid:` escapes included. `None` means unknown *or* reserved —
    /// [`InputMap::is_reserved`] tells those apart for a better error.
    #[must_use]
    pub fn input_of(&self, name: &str) -> Option<Holdable> {
        if let Some((_, button, _)) = self
            .buttons
            .iter()
            .find(|(candidate, _, _)| candidate.eq_ignore_ascii_case(name))
        {
            return Some(Holdable::Button(*button));
        }
        name.parse::<Key>().ok().map(Holdable::Key)
    }

    /// Input → name: what a live event should be called.
    ///
    /// `None` only for inputs with no canonical spelling (a [`Key::Hid`], a
    /// [`Button::Other`]); callers fall back to the input's own `Display`.
    #[must_use]
    pub fn name_of(&self, input: Holdable) -> Option<&'static str> {
        match input {
            Holdable::Key(key) => key.canonical_name(),
            Holdable::Button(button) => self
                .buttons
                .iter()
                .find(|(_, candidate, _)| *candidate == button)
                .map(|(name, _, _)| *name),
        }
    }

    /// Canonical name → the other accepted spellings, worst habits included.
    ///
    /// Single-character entries are excluded on purpose: those are the shifted twins
    /// (`{` for `[`), a rule worth one sentence, not thirty list items.
    pub fn aliases_of(&self, key: Key) -> impl Iterator<Item = &'static str> {
        KEY_NAMES
            .iter()
            .filter(move |(_, candidate)| *candidate == key)
            .skip(1) // the first entry is the canonical spelling, not an alias
            .map(|(name, _)| *name)
            .filter(|name| name.chars().count() > 1)
    }

    /// Alias → the canonical spelling: the runtime reverse of [`InputMap::aliases_of`].
    ///
    /// Any accepted spelling comes back canonical — `canonical_of("esc")` is `escape`,
    /// `canonical_of("leftctrl")` is `ctrl` — which is what a tool prints after reading
    /// whatever the user wrote.
    #[must_use]
    pub fn canonical_of(&self, name: &str) -> Option<&'static str> {
        name.parse::<Key>().ok().and_then(Key::canonical_name)
    }

    /// Whether `name` is a reserved spelling: owned by the format, produced by nothing
    /// yet. Separator- and case-insensitive, like key names.
    #[must_use]
    pub fn is_reserved(&self, name: &str) -> bool {
        let wanted = fold_separators(name);
        self.reserved
            .iter()
            .any(|(candidate, _, _)| fold_separators(candidate) == wanted)
    }
}

/// Lowercases and drops `-`/`_`, so separator spelling never decides a match.
fn fold_separators(name: &str) -> alloc::string::String {
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

    /// The map's two directions agree: a name resolves to an input, and that input
    /// resolves back to the same canonical name — for keys and buttons alike. This is
    /// the round trip a profile-write followed by a live report goes through.
    #[test]
    fn the_input_map_round_trips_names_and_inputs() {
        use alloc::string::ToString as _;
        for entry in INPUT_MAP.entries() {
            let Some(input) = entry.input else {
                assert!(
                    INPUT_MAP.is_reserved(entry.name),
                    "{} has no input, so it must be reserved",
                    entry.name
                );
                assert!(
                    INPUT_MAP.input_of(entry.name).is_none(),
                    "{} is reserved and must not resolve",
                    entry.name
                );
                continue;
            };
            assert_eq!(
                INPUT_MAP.input_of(entry.name),
                Some(input),
                "name -> input for {}",
                entry.name
            );
            assert_eq!(
                INPUT_MAP.name_of(input),
                Some(entry.name),
                "input -> name for {}",
                entry.name
            );
        }
        // The nameless tail stays nameless rather than borrowing someone's name.
        assert_eq!(INPUT_MAP.name_of(Holdable::Key(Key::Hid(0x1234))), None);
        assert_eq!(INPUT_MAP.name_of(Holdable::Button(Button::Other(9))), None);
        let _ = Key::Hid(0).to_string();
    }

    /// Categories keep the families apart — the whole reason they exist: a listing must
    /// be able to print media keys without dragging the modifiers along.
    #[test]
    fn categories_separate_the_families() {
        assert_eq!(Key::VolumeUp.category(), Category::Media);
        assert_eq!(Key::LeftCtrl.category(), Category::Modifier);
        assert_eq!(Key::PageUp.category(), Category::Nav);
        assert_eq!(Key::Keypad8.category(), Category::Numpad);
        assert_eq!(Key::A.category(), Category::Main);
        assert!(
            INPUT_MAP
                .in_category(Category::Media)
                .all(|entry| entry.category == Category::Media)
        );
        assert!(INPUT_MAP.in_category(Category::Pad).count() > 0);
    }

    /// The alias view groups per canonical name and the reverse lookup lands back on
    /// it, whatever spelling went in — the two directions the user-facing tools need.
    #[test]
    fn aliases_group_by_canonical_and_reverse_to_it() {
        let aliases: alloc::vec::Vec<_> = INPUT_MAP.aliases_of(Key::Escape).collect();
        assert_eq!(aliases, ["esc"]);
        let ctrl: alloc::vec::Vec<_> = INPUT_MAP.aliases_of(Key::LeftCtrl).collect();
        assert_eq!(ctrl, ["lctrl", "leftctrl"]);
        // The shifted twin is a rule, not an alias: `{` must not show up in the list.
        let bracket: alloc::vec::Vec<_> = INPUT_MAP.aliases_of(Key::LeftBracket).collect();
        assert_eq!(bracket, ["leftbracket"]);

        assert_eq!(INPUT_MAP.canonical_of("esc"), Some("escape"));
        assert_eq!(INPUT_MAP.canonical_of("leftctrl"), Some("ctrl"));
        assert_eq!(INPUT_MAP.canonical_of("{"), Some("["));
        assert_eq!(INPUT_MAP.canonical_of("nosuchkey"), None);
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
