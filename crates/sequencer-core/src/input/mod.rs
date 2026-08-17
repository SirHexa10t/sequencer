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

use crate::time::Timestamp;

mod key;
mod map;

pub use key::{Button, Key, KeyParseError};
pub use map::{Category, INPUT_MAP, InputEntry, InputMap};

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
