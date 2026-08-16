//! Platform input: watching real devices, and synthesising events into them.
//!
//! Everything in this workspace that talks to an operating system lives here. The engine
//! in `sequencer-core` has no path to any of it in its dependency graph, which is what
//! makes "the engine is testable without a display server" a fact about the build rather
//! than a promise in a comment.
//!
//! Two Linux backends, chosen per session by the caller:
//!
//! - [`linux`] — reads `/dev/input`, writes `/dev/uinput`. Below the display server, so
//!   one code path covers X11, Wayland and the bare console, and it is the only way to
//!   reach the last two. Needs access to the device nodes, which is what a user has to
//!   grant.
//! - [`x11`] — XTEST out, passive key grabs in. X11 only, and **needs no device access at
//!   all**: an X11 run is an ordinary X client, so it asks for no permissions and no
//!   password. It also sits above libinput, which historically mattered for the click rate.
//!
//! They share [`linux::keymap`] — an X keycode on Linux is the evdev code plus a fixed
//! offset — which is why `xtest` pulls `evdev` in for the tables even though it opens no
//! device.
//!
//! Other platforms would be new modules behind their own target-gated features; nothing
//! here anticipates them beyond that.

// Tests unwrap freely: an `unwrap()` in a test reports a failure rather than
// hiding one. Library code keeps the workspace-level `warn`.
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod capture;
pub mod clock;
#[cfg(all(any(feature = "evdev", feature = "xtest"), target_os = "linux"))]
pub mod linux;
pub mod mock;
pub mod probe;
#[cfg(all(feature = "xtest", target_os = "linux"))]
pub mod x11;

pub use crate::capture::{CaptureStream, Epoch, EventQueue};
pub use crate::clock::SystemClock;
#[cfg(all(feature = "evdev", target_os = "linux"))]
pub use crate::linux::{EvdevCapture, UinputSink};
pub use crate::mock::MockInjector;
pub use crate::probe::{CheckResult, Remediation, Requirement, Session, SessionInfo};
#[cfg(all(feature = "xtest", target_os = "linux"))]
pub use crate::x11::{FocusWatcher, GrabCapture, XTestSink};
