//! Events flowing *out*: what a run wants done to the outside world.
//!
//! An [`Emit`] is a value — an action and the instant it was meant for — and an
//! [`InputSink`] is the one contract a backend fulfils to perform it. Keeping the output
//! a value rather than a side effect is what makes a behavioural test a plain comparison
//! of two lists: a recording sink is a sink like any other.

use alloc::boxed::Box;

use crate::input::{Button, Key};
use crate::time::Timestamp;

/// A single thing to do to the outside world.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum EmitAction {
    /// Press a key and leave it down.
    KeyDown(Key),
    /// Release a key.
    KeyUp(Key),
    /// Press a mouse button and leave it down.
    ButtonDown(Button),
    /// Release a mouse button.
    ButtonUp(Button),
    /// Turn the wheel.
    Scroll {
        /// Horizontal detents; positive is right.
        dx: i32,
        /// Vertical detents; positive is up.
        dy: i32,
    },
    /// Move the cursor to an absolute screen position.
    CursorTo {
        /// Horizontal position in pixels.
        x: i32,
        /// Vertical position in pixels.
        y: i32,
    },
    /// Move the cursor by an offset.
    CursorBy {
        /// Horizontal offset in pixels.
        dx: i32,
        /// Vertical offset in pixels.
        dy: i32,
    },
}

impl EmitAction {
    /// The thing this action holds down, if it holds anything down.
    #[must_use]
    pub const fn holds(self) -> Option<Holdable> {
        match self {
            Self::KeyDown(k) => Some(Holdable::Key(k)),
            Self::ButtonDown(b) => Some(Holdable::Button(b)),
            _ => None,
        }
    }

    /// The thing this action releases, if it releases anything.
    #[must_use]
    pub const fn releases(self) -> Option<Holdable> {
        match self {
            Self::KeyUp(k) => Some(Holdable::Key(k)),
            Self::ButtonUp(b) => Some(Holdable::Button(b)),
            _ => None,
        }
    }
}

/// Something that can be held down, and therefore something that can be left stuck.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Holdable {
    /// A keyboard key.
    Key(Key),
    /// A mouse button.
    Button(Button),
}

impl Holdable {
    /// The action that presses this.
    #[must_use]
    pub const fn down(self) -> EmitAction {
        match self {
            Self::Key(k) => EmitAction::KeyDown(k),
            Self::Button(b) => EmitAction::ButtonDown(b),
        }
    }

    /// The action that releases this.
    #[must_use]
    pub const fn up(self) -> EmitAction {
        match self {
            Self::Key(k) => EmitAction::KeyUp(k),
            Self::Button(b) => EmitAction::ButtonUp(b),
        }
    }
}

/// One emitted action, with the time the engine intended it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Emit {
    /// When the engine meant this to happen.
    ///
    /// This is the scheduled time, not the wall-clock time it reaches the OS, which is
    /// what makes cadence assertions in tests exact.
    pub at: Timestamp,
    /// What to do.
    pub action: EmitAction,
    /// Send level, propagated onto the synthetic event a backend produces.
    ///
    /// See [`crate::input::EventOrigin`] for why zero is the safe default.
    pub level: u8,
}

/// Something went wrong talking to the OS.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SinkError {
    /// The connection to the display server or device is gone.
    #[error("input backend disconnected")]
    Disconnected,
    /// The backend cannot do this at all.
    #[error("backend does not support {0}")]
    Unsupported(&'static str),
    /// The key exists but has no code on the active layout.
    #[error("key {0} has no mapping on the active layout")]
    UnmappableKey(Key),
    /// Anything platform-specific.
    ///
    /// Displays the inner error rather than a generic label, so a caller printing only
    /// `Display` still shows the user the part they can act on.
    #[error("{0}")]
    Backend(#[source] Box<dyn core::error::Error + Send + Sync>),
}

/// Somewhere to send synthesized input.
///
/// Object-safe, so `Box<dyn InputSink>` works and the backend can be chosen at runtime.
/// The error type is one concrete enum rather than an associated type on purpose: an
/// associated type would propagate into every generic parameter downstream and the runner
/// would still need a unifying enum to combine sink errors with its own, so you would pay
/// for the generic and write the enum anyway. Platform detail rides in
/// [`SinkError::Backend`].
pub trait InputSink {
    /// Performs one action. May buffer; [`InputSink::flush`] is what guarantees delivery.
    fn emit(&mut self, emit: &Emit) -> Result<(), SinkError>;

    /// Pushes any buffered actions to the OS.
    ///
    /// Backends that batch (X11's XTEST, for one) do their round trip here.
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    /// Releases everything this sink believes it is holding down.
    ///
    /// Called from the runner's drop guard, including while unwinding from a panic, so it
    /// cannot fail and cannot panic: a best-effort release beats a stuck modifier key.
    fn release_all(&mut self);
}
