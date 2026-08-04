//! The Linux backend: `/dev/input` in, `/dev/uinput` out.
//!
//! One code path for X11, Wayland and the console, because it sits below all three. An X11
//! backend ([`crate::x11`]) sits *beside* it for X11 sessions where libinput throttles the
//! device path (see that module) — the two share this [`keymap`], since an X keycode on Linux
//! is just the evdev code plus a fixed offset.

pub mod keymap;

#[cfg(feature = "evdev")]
pub mod bench;
#[cfg(feature = "evdev")]
pub mod capture;
#[cfg(feature = "evdev")]
pub mod inject;

#[cfg(feature = "evdev")]
pub use bench::{BenchError, BenchObserver, BenchResult, BenchSample, Unobserved};
#[cfg(feature = "evdev")]
pub use capture::{CaptureError, EvdevCapture};
#[cfg(feature = "evdev")]
pub use inject::{DEVICE_NAME, UinputSink};
