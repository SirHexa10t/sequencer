//! The Linux backend: `/dev/input` in, `/dev/uinput` out.
//!
//! One code path for X11, Wayland and the console, because it sits below all three. That
//! position is the whole point — it is the only layer from which a Wayland compositor and
//! a bare virtual terminal are both reachable — and its cost is that everything written
//! passes through libinput on the way up to an application. What that costs depends on the
//! shape of the virtual device; see [`inject::UinputSink::open`], where the axes exist for
//! no reason other than keeping libinput routing these events at all.
//!
//! [`crate::x11`] sits beside this for X11 sessions, going over libinput rather than
//! under it.
//!
//! [`keymap`] is the exception to all of that: it is pure tables, and the [`crate::x11`]
//! backend shares them, so it stays available whenever either backend is built.

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
