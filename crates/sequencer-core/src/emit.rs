//! Events flowing *out*: what the engine wants done to the outside world.
//!
//! The engine never calls a sink. It appends to an [`EmitBuf`] and returns; the runner
//! drains that buffer into the real [`InputSink`]. Keeping the engine's output a value
//! rather than a side effect is what makes every behavioural test a plain comparison of
//! two lists.

use alloc::boxed::Box;
use alloc::vec::Vec;

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

/// The engine's output buffer for one tick.
///
/// Owned by the runner and reused across ticks, so the steady state allocates nothing.
/// Only the engine can push, so an `EmitBuf` handed to a test is necessarily a faithful
/// record of what the engine decided.
#[derive(Debug, Default, Clone)]
pub struct EmitBuf {
    actions: Vec<Emit>,
}

impl EmitBuf {
    /// An empty buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            actions: Vec::new(),
        }
    }

    /// The actions emitted since the last [`EmitBuf::clear`].
    #[must_use]
    pub fn as_slice(&self) -> &[Emit] {
        &self.actions
    }

    /// Whether anything was emitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// How many actions are buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Drops the buffered actions, keeping the allocation for the next tick.
    pub fn clear(&mut self) {
        self.actions.clear();
    }

    pub(crate) fn push(&mut self, emit: Emit) {
        self.actions.push(emit);
    }
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
