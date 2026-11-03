//! What KDE knows that Xwayland does not: which program has focus, which keyboard layout
//! is active, and which windows there are.
//!
//! In a Wayland session the X server is Xwayland, which sees only X11 windows and names
//! only the first configured layout — so [`crate::x11`]'s readers go blind there. KDE's
//! compositor, KWin, knows both, and tells over the session bus:
//!
//! - [`KdeLayout`]: KWin's `org.kde.keyboard` service names the active layout and
//!   announces every switch.
//! - [`KWinFocus`]: KWin has no call that names the active window, so a small script
//!   loaded into it reports every focus change back.
//! - [`windows`] and [`close`]: KWin's window list, every kind of window included, and
//!   asking windows to close — through a script too, one that runs once.
//! - [`invoke_kwin_shortcut`]: one of KWin's own global shortcuts, run as its keys would
//!   run it — the window actions KWin offers no call for.
//! - [`xwayland_sharing`]: what KDE lets X11 apps hear of input meant for other apps —
//!   the setting that decides which triggers still fire while a Wayland app has focus.
//!
//! Both readers keep a thread listening on a D-Bus connection of their own and answer
//! from what it last heard, so asking them costs a lock, never a round trip. Shaped like
//! the X11 readers: `open() -> Option<Self>`, then queries that return `None` for
//! anything unknown. The window list and the shortcuts are asked afresh each time, and say
//! why they could not be ([`KWinError`]), since their callers report that to the user.

mod focus;
mod layout;
mod script;
mod shortcut;
mod windows;
mod xwayland;

pub use focus::KWinFocus;
pub use layout::KdeLayout;
pub use shortcut::invoke_kwin_shortcut;
pub use windows::{KWinWindow, close, windows};
pub use xwayland::{KeySharing, XwaylandSharing, xwayland_sharing};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Why KWin could not be asked.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KWinError {
    /// There is no session bus to reach KWin on.
    #[error("no session bus: {0}")]
    Bus(String),
    /// KWin is not running in this session.
    #[error("KWin is not running in this session")]
    NotRunning,
    /// KWin would not load or run sequencer's script, or it never reported back.
    #[error("KWin did not run sequencer's script: {0}")]
    Script(&'static str),
    /// KWin answered a call with an error: the D-Bus error's name and message.
    #[error("KWin refused: {0}")]
    Refused(String),
}

impl From<crate::dbus::Error> for KWinError {
    fn from(err: crate::dbus::Error) -> Self {
        Self::Bus(err.to_string())
    }
}

/// Locks state a listener thread shares. A panic elsewhere while holding the lock leaves
/// plain data behind, still worth reading.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
