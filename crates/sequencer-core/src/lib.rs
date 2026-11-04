//! The sequencer domain, with no I/O in it: what a key is, what an input event is, what
//! an action to perform is, how time is measured, and a replayable random source.
//!
//! This crate never reads a clock, never sleeps, never touches a device and never spawns a
//! thread — `#![no_std]` is the mechanical guarantee, so reaching for `Instant::now()` or
//! `thread::sleep()` from here is a compile error rather than a code-review catch. The
//! backends (`sequencer-input`) own every call into the input stack; the products
//! (`sequencer-cli`) own the loops. What lives here is the vocabulary both of them speak:
//!
//! - [`input`]: keys as *positions* (the USB HID usage table), buttons, and the events a
//!   backend reports, with the modifier classes folded the way X folds them.
//! - [`emit`]: the actions a run performs and the [`emit::InputSink`] contract a backend
//!   fulfils to perform them.
//! - [`time`]: integer-nanosecond timestamps and durations, the [`time::Clock`] seam a
//!   virtual clock can stand in for, and rates as periods.
//! - [`rng`]: deterministic xorshift, so a run with chance in it can be replayed exactly.
//! - [`testutil`]: the virtual clock, exported so every crate above tests against the same
//!   one.
#![no_std]
// Tests unwrap freely: an `unwrap()` in a test reports a failure rather than
// hiding one. Library code keeps the workspace-level `warn`.
#![cfg_attr(test, allow(clippy::unwrap_used))]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod emit;
pub mod input;
pub mod rng;
pub mod time;

pub mod testutil;

pub use crate::emit::{Emit, EmitAction, Holdable, InputSink, SinkError};
pub use crate::input::{Button, EventKind, EventOrigin, InputEvent, Key};
pub use crate::time::{Clock, ConfigError, Duration, Period, Timestamp};
