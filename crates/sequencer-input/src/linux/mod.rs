//! The Linux backend: `/dev/input` in, `/dev/uinput` out.
//!
//! One code path for X11, Wayland and the console, because it sits below all three. There
//! is deliberately no X11-protocol backend alongside it: XTEST and XInput2 would only
//! cover X11 sessions, which this already handles, and would add a second keymap and a
//! second self-echo problem for no coverage gained.

pub mod bench;
pub mod capture;
pub mod inject;
pub mod keymap;

pub use bench::{BenchError, BenchResult};
pub use capture::{CaptureError, EvdevCapture};
pub use inject::{DEVICE_NAME, UinputSink};
