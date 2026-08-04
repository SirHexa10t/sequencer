//! The X11 backend: XTEST out, key grabs in — both through the X server, above libinput.
//!
//! Mirrors the [`crate::linux`] layout (`inject` + `capture`) because it plays the same role
//! one layer up. What makes the pair worth having is what each side buys:
//!
//! - [`inject`]: past libinput's per-device processing, whose click-rate ceiling the README
//!   documents. This is the layer `xdotool` and pynput inject at.
//! - [`capture`]: the hotkeys arrive by passive key grab, so nothing here reads
//!   `/dev/input` — which means an X11 session needs **no group membership and no sudo at
//!   all**: the whole run is an ordinary X client.
//!
//! Neither reaches a Wayland client or the console; the [`crate::linux`] backend remains the
//! one that works everywhere, and callers pick per session at runtime.

pub mod capture;
pub mod inject;

pub use capture::{GrabCapture, GrabError};
pub use inject::XTestSink;
