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
//! lands. It is also the user-facing promise: a bind fires whatever keyboard language is
//! active — Hebrew `א` and `t` are one key — because a "save" hotkey that fails when the
//! system is on the wrong language is worse than useless. This program binds keys, not
//! the characters they happen to type; per-language name aliases are a non-goal.

use crate::time::Timestamp;

mod key;
mod map;

pub use key::{Button, Key, KeyParseError};
pub use map::{Category, INPUT_MAP, InputEntry, InputMap};

/// Where an event came from.
///
/// [`EventOrigin::Synthetic`] carries AutoHotkey's send-level idea: with a binding's input
/// level and an emitted event's level both defaulting to zero, and a binding only firing
/// on synthetic events *strictly above* its level, a binding cannot retrigger itself.
/// The safe behaviour falls out of the arithmetic instead of needing a special case, and
/// deliberate remap cascades stay possible later by raising the level. That rule itself
/// retired with the core engine that applied it (2026-09-29): the binds format guards
/// against self-retriggering instead, by refusing any profile whose output would keep
/// re-pressing its own triggers. What reads the level now is the profile manager, as a
/// chain's depth: a trigger fired by a bind's output runs its own bind one deeper, and the
/// device backend stops chaining at a fixed depth.
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

/// The modifier classes held when an event fired, folded the way X folds them:
/// left and right shift/ctrl/meta are one class each, right alt (AltGr) is its own.
///
/// This is the half of a chord that rides along as *state* rather than arriving as
/// its own event — a grab fires a chord as one key-down of its ordinary key, and
/// without this a listener cannot tell `ctrl w` from `ctrl shift w`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Mods(u8);

impl Mods {
    /// No modifiers held.
    pub const NONE: Self = Self(0);
    /// Either shift.
    pub const SHIFT: Self = Self(1);
    /// Either ctrl.
    pub const CTRL: Self = Self(1 << 1);
    /// Left alt.
    pub const ALT: Self = Self(1 << 2);
    /// Right alt — AltGr, its own class on nearly every layout.
    pub const RALT: Self = Self(1 << 3);
    /// Either meta/super.
    pub const META: Self = Self(1 << 4);

    /// Every class, in the order a chord spells them — for walking a set class by
    /// class, which is what any "which of these is held?" question needs.
    pub const CLASSES: [Self; 5] = [Self::SHIFT, Self::CTRL, Self::ALT, Self::RALT, Self::META];

    /// The class `key` contributes, or `None` if it is not a modifier.
    #[must_use]
    pub const fn of_key(key: Key) -> Option<Self> {
        Some(match key {
            Key::LeftShift | Key::RightShift => Self::SHIFT,
            Key::LeftCtrl | Key::RightCtrl => Self::CTRL,
            Key::LeftAlt => Self::ALT,
            Key::RightAlt => Self::RALT,
            Key::LeftMeta | Key::RightMeta => Self::META,
            _ => return None,
        })
    }

    /// Every modifier class named in `chord`, folded together.
    #[must_use]
    pub fn of_chord(chord: &[Key]) -> Self {
        chord
            .iter()
            .filter_map(|&key| Self::of_key(key))
            .fold(Self::NONE, Self::and)
    }

    /// Both sets together.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The classes in both sets.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Whether every class in `other` is also in `self`.
    #[must_use]
    pub const fn covers(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no modifier is held.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The keys to watch to see this set released, both sides of each folded class.
    #[must_use]
    pub fn watch_keys(self) -> alloc::vec::Vec<Key> {
        let mut keys = alloc::vec::Vec::new();
        if self.covers(Self::SHIFT) {
            keys.extend([Key::LeftShift, Key::RightShift]);
        }
        if self.covers(Self::CTRL) {
            keys.extend([Key::LeftCtrl, Key::RightCtrl]);
        }
        if self.covers(Self::ALT) {
            keys.push(Key::LeftAlt);
        }
        if self.covers(Self::RALT) {
            keys.push(Key::RightAlt);
        }
        if self.covers(Self::META) {
            keys.extend([Key::LeftMeta, Key::RightMeta]);
        }
        keys
    }
}

impl core::fmt::Display for Mods {
    /// Canonical short names, space-joined — the way a chord spells them.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        for (class, name) in [
            (Self::SHIFT, "shift"),
            (Self::CTRL, "ctrl"),
            (Self::ALT, "alt"),
            (Self::RALT, "ralt"),
            (Self::META, "meta"),
        ] {
            if self.covers(class) {
                if !first {
                    f.write_str(" ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
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

impl EventKind {
    /// The key or mouse button that went down (`true`) or came up (`false`), for what a
    /// trigger can be — key and button alike; `None` for the wheel and pointer motion.
    #[must_use]
    pub const fn edge(self) -> Option<(crate::emit::Holdable, bool)> {
        use crate::emit::Holdable;
        match self {
            Self::KeyDown(key) => Some((Holdable::Key(key), true)),
            Self::KeyUp(key) => Some((Holdable::Key(key), false)),
            Self::ButtonDown(button) => Some((Holdable::Button(button), true)),
            Self::ButtonUp(button) => Some((Holdable::Button(button), false)),
            Self::Scroll { .. } | Self::Motion { .. } => None,
        }
    }
}

/// One thing the user did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InputEvent {
    /// The backend's hardware timestamp if it has one, otherwise arrival time, on the
    /// run's shared timeline.
    ///
    /// Nothing reads it today. The retired core engine phase-locked repeat cadence to it;
    /// the binds executor paces from when it handles a press instead, so an event
    /// delivered late starts its repetitions late rather than catching up.
    pub at: Timestamp,
    /// Whether this came from hardware or from an injector.
    pub origin: EventOrigin,
    /// What happened.
    pub kind: EventKind,
    /// The modifier classes held at that moment, where the backend can tell
    /// ([`Mods::NONE`] where it cannot — a bare-key world is the safe default).
    pub mods: Mods,
}

impl InputEvent {
    /// A physical event at `at`.
    #[must_use]
    pub const fn physical(at: Timestamp, kind: EventKind) -> Self {
        Self {
            at,
            origin: EventOrigin::Physical,
            kind,
            mods: Mods::NONE,
        }
    }

    /// A synthetic event at `at`, tagged with the emitting binding's send `level`.
    #[must_use]
    pub const fn synthetic(at: Timestamp, level: u8, kind: EventKind) -> Self {
        Self {
            at,
            origin: EventOrigin::Synthetic { level },
            kind,
            mods: Mods::NONE,
        }
    }

    /// The same event with the modifier classes that were held when it fired.
    #[must_use]
    pub const fn with_mods(mut self, mods: Mods) -> Self {
        self.mods = mods;
        self
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString as _;

    use super::*;

    /// Keys and buttons are edges alike — a trigger can be either — and the wheel and the
    /// pointer are none.
    #[test]
    fn keys_and_buttons_are_edges_and_motion_is_not() {
        use crate::emit::Holdable;
        assert_eq!(
            EventKind::KeyDown(Key::A).edge(),
            Some((Holdable::Key(Key::A), true))
        );
        assert_eq!(
            EventKind::ButtonUp(Button::Back).edge(),
            Some((Holdable::Button(Button::Back), false))
        );
        assert_eq!(EventKind::Scroll { dx: 0, dy: 1 }.edge(), None);
        assert_eq!(EventKind::Motion { x: 1, y: 1 }.edge(), None);
    }

    /// The folding rules chords and events must agree on: left/right shift, ctrl and
    /// meta are one class each; right alt is AltGr, its own; non-modifiers are none.
    #[test]
    fn modifier_classes_fold_the_way_x_folds_them() {
        assert_eq!(Mods::of_key(Key::LeftShift), Mods::of_key(Key::RightShift));
        assert_eq!(Mods::of_key(Key::LeftCtrl), Mods::of_key(Key::RightCtrl));
        assert_eq!(Mods::of_key(Key::LeftMeta), Mods::of_key(Key::RightMeta));
        assert_ne!(Mods::of_key(Key::LeftAlt), Mods::of_key(Key::RightAlt));
        assert_eq!(Mods::of_key(Key::A), None);

        let chord = Mods::of_chord(&[Key::LeftCtrl, Key::LeftShift, Key::W]);
        assert_eq!(chord, Mods::CTRL.and(Mods::SHIFT));
        assert!(chord.covers(Mods::of_chord(&[Key::RightCtrl])));
        assert!(!Mods::of_chord(&[Key::RightCtrl]).covers(chord));
        assert!(Mods::of_chord(&[Key::W]).is_none());
        assert_eq!(chord.intersect(Mods::CTRL), Mods::CTRL);
        assert!(Mods::SHIFT.intersect(Mods::CTRL).is_none());
    }

    /// [`Mods::CLASSES`] and [`Display`](core::fmt::Display) must agree, in content and
    /// in order: a class missing from the list would be invisible to every "which of
    /// these is held?" walk built on it, and silently so.
    #[test]
    fn the_class_list_names_every_class_display_knows() {
        let all = Mods::CLASSES.into_iter().fold(Mods::NONE, Mods::and);
        assert_eq!(all.to_string(), "shift ctrl alt ralt meta");
        for class in Mods::CLASSES {
            assert!(!class.is_none(), "a class must name at least one modifier");
            assert!(all.covers(class));
        }
    }

    /// Display speaks the same canonical names a chord is written in.
    #[test]
    fn modifier_classes_name_themselves_like_a_chord() {
        let text = Mods::of_chord(&[Key::LeftShift, Key::RightCtrl]).to_string();
        assert_eq!(text, "shift ctrl");
        assert_eq!(Mods::NONE.to_string(), "");
    }

    /// The watch list covers both physical sides of each folded class — the grab was
    /// side-blind, so the release watch must be too.
    #[test]
    fn watching_a_class_watches_both_its_keys() {
        let keys = Mods::of_chord(&[Key::RightShift]).watch_keys();
        assert!(keys.contains(&Key::LeftShift) && keys.contains(&Key::RightShift));
    }
}
