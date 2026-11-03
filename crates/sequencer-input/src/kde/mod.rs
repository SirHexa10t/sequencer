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
//! - [`xwayland_sharing`]: what KDE lets X11 apps hear of input meant for other apps —
//!   the setting that decides which triggers still fire while a Wayland app has focus.
//!
//! Both readers keep a thread listening on a D-Bus connection of their own and answer
//! from what it last heard, so asking them costs a lock, never a round trip. Shaped like
//! the X11 readers: `open() -> Option<Self>`, then queries that return `None` for
//! anything unknown. The window list is asked afresh each time, and says why it could not
//! be ([`KWinError`]), since its callers report that to the user.

mod focus;
mod layout;
mod script;
mod windows;
mod xwayland;

pub use focus::KWinFocus;
pub use layout::KdeLayout;
pub use windows::{KWinError, KWinWindow, close, windows};
pub use xwayland::{KeySharing, XwaylandSharing, xwayland_sharing};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks state a listener thread shares. A panic elsewhere while holding the lock leaves
/// plain data behind, still worth reading.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
