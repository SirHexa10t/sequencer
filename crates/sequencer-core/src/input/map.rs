//! The categorized name↔input registry: what can be bound, by family.
//!
//! Two directions, one source of truth: **name → input** reads profiles, **input →
//! name** reports live events. Keyboard names and spellings come from the key table in
//! [`super::key`] — nothing is stored twice — while buttons, reserved spellings and
//! glosses live in [`INPUT_MAP`]'s own rows. [`Key::category`] is an exhaustive match,
//! so a new key variant refuses to compile until it picks a family.

use crate::emit::Holdable;

use super::key::{KEY_NAMES, fold_separators};
use super::{Button, Key};

impl Key {
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
    /// [`super::EventKind::Scroll`]).
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

    /// Input → name as owned text, with the nameless tail covered: where
    /// [`InputMap::name_of`] gives up (a `hid:` key, an extra mouse button), the input's
    /// own `Display` answers instead. The form banners, errors and live reporting print.
    #[must_use]
    pub fn display_name(&self, input: Holdable) -> alloc::string::String {
        use alloc::string::ToString as _;
        self.name_of(input).map_or_else(
            || match input {
                Holdable::Key(key) => key.to_string(),
                Holdable::Button(button) => button.to_string(),
            },
            alloc::string::ToString::to_string,
        )
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

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::emit::Holdable;

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
}
