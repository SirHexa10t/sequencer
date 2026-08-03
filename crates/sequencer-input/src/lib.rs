//! Platform input: watching real devices, and synthesising events into them.
//!
//! Everything in this workspace that talks to an operating system lives here. The engine
//! in `sequencer-core` has no path to any of it in its dependency graph, which is what
//! makes "the engine is testable without a display server" a fact about the build rather
//! than a promise in a comment.
//!
//! One real backend: [`linux`], reading `/dev/input` and writing `/dev/uinput`. It sits
//! below the display server, so a single code path covers X11, Wayland and the console —
//! which is also why there is no X11-protocol backend alongside it. Other platforms would
//! be new modules behind their own target-gated features; nothing here anticipates them
//! beyond that.

// Tests unwrap freely: an `unwrap()` in a test reports a failure rather than
// hiding one. Library code keeps the workspace-level `warn`.
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod capture;
pub mod clock;
#[cfg(all(feature = "evdev", target_os = "linux"))]
pub mod linux;
pub mod mock;
pub mod probe;

pub use crate::capture::{CaptureStream, Epoch, EventQueue};
pub use crate::clock::SystemClock;
#[cfg(all(feature = "evdev", target_os = "linux"))]
pub use crate::linux::{EvdevCapture, UinputSink};
pub use crate::mock::MockInjector;
pub use crate::probe::{CheckResult, Remediation, Requirement, Session, SessionInfo};
