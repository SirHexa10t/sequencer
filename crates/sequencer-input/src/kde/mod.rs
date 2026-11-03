//! What KDE knows that Xwayland does not: which program has focus, and which keyboard
//! layout is active.
//!
//! In a Wayland session the X server is Xwayland, which sees only X11 windows and names
//! only the first configured layout — so [`crate::x11`]'s readers go blind there. KDE's
//! compositor, KWin, knows both, and tells over the session bus:
//!
//! - [`KdeLayout`]: KWin's `org.kde.keyboard` service names the active layout and
//!   announces every switch.
//! - [`KWinFocus`]: KWin has no call that names the active window, so a small script
//!   loaded into it reports every focus change back.
//! - [`xwayland_sharing`]: what KDE lets X11 apps hear of input meant for other apps —
//!   the setting that decides which triggers still fire while a Wayland app has focus.
//!
//! Both readers keep a thread listening on a D-Bus connection of their own and answer
//! from what it last heard, so asking them costs a lock, never a round trip. Shaped like
//! the X11 readers: `open() -> Option<Self>`, then queries that return `None` for
//! anything unknown.

mod focus;
mod layout;
mod xwayland;

pub use focus::KWinFocus;
pub use layout::KdeLayout;
pub use xwayland::{KeySharing, XwaylandSharing, xwayland_sharing};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks state a listener thread shares. A panic elsewhere while holding the lock leaves
/// plain data behind, still worth reading.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
