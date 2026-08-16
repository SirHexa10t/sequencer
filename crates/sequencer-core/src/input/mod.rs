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
