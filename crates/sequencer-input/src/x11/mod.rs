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
//! - [`layout`]: which keyboard layout is active — the same pair of consumers, for
//!   `kb_lang` gating.
//!
//! Neither reaches a Wayland client or the console; the [`crate::linux`] backend remains the
//! one that works everywhere, and callers pick per session at runtime.

pub mod capture;
pub mod focus;
pub mod inject;
pub mod layout;

pub use capture::{GrabCapture, GrabError, KeyProbe, KeyboardState};
pub use focus::FocusWatcher;
pub use inject::{LiftedTap, XTestSink};
pub use layout::LayoutWatcher;

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

/// Whether the X server is Xwayland: an X11 server inside a Wayland session, which sees
/// only its own windows and its first layout.
///
/// Asks the server, which announces itself with an `XWAYLAND` extension, rather than
/// the environment: sudo keeps `$DISPLAY` but drops `$WAYLAND_DISPLAY`, so from the
/// variables alone a Wayland session run through sudo looks like plain X11.
#[must_use]
pub fn is_xwayland() -> bool {
    use x11rb::connection::RequestConnection as _;
    x11rb::connect(None).is_ok_and(|(connection, _)| {
        connection
            .extension_information("XWAYLAND")
            .is_ok_and(|extension| extension.is_some())
    })
}
