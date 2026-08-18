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
//! - [`focus`]: which program is focused — the manager's per-program gating asks it,
//!   and `detect-key` reports it.
//!
//! Neither reaches a Wayland client or the console; the [`crate::linux`] backend remains the
//! one that works everywhere, and callers pick per session at runtime.

pub mod capture;
pub mod focus;
pub mod inject;

pub use capture::{GrabCapture, GrabError, KeyProbe};
pub use focus::FocusWatcher;
pub use inject::XTestSink;

/// Whether this really is an X11 session that both halves can use.
///
/// Connects and checks for XTEST, rather than trusting `$DISPLAY` to be set — a stale
/// variable, or a server without the extension, would otherwise route a run down the X11
/// path and strand it there. Callers use this to pick the backend *pair* up front, so the
/// privilege the run will need is known before it asks for any.
///
/// The connection is opened and dropped; a run that proceeds opens its own.
#[must_use]
pub fn is_usable() -> bool {
    if std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    XTestSink::open().is_ok()
}
