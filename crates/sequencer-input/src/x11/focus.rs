//! Which program is focused, by asking the X server.
//!
//! EWMH: the root window's `_NET_ACTIVE_WINDOW` property names the focused window, and
//! that window's `WM_CLASS` names the program — the *class* half is the stable
//! identifier tools match on (`firefox` stays `firefox` across windows and titles), so
//! it is what [`FocusWatcher::focused_class`] returns. This is the identifier a
//! per-program binds profile will match on later; `detect-key` prints it so users can
//! discover the spelling the same way they discover key names.
//!
//! Polling, not events: the consumers here already wake on their own cadence (a quit
//! poll, a read timeout), and two property reads over a local socket cost microseconds.
//! Subscribing to PropertyNotify would save nothing and add a thread.

use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

/// Reads the focused program's class name on demand.
#[derive(Debug)]
pub struct FocusWatcher {
    conn: RustConnection,
    root: Window,
    active_window: u32,
}

impl FocusWatcher {
    /// Connects and resolves the EWMH atom. `None` when there is no X server to ask or
    /// the window manager does not speak EWMH — focus reporting is garnish, and a
    /// session without it just goes without, which is why this is not a `Result`.
    #[must_use]
    pub fn open() -> Option<Self> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let active_window = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")
            .ok()?
            .reply()
            .ok()?
            .atom;
        Some(Self {
            conn,
            root,
            active_window,
        })
    }

    /// The class of the focused program, or `None` when nothing readable has focus.
    ///
    /// `None` covers every failure alike — no active window, a window that vanished
    /// between the two reads, a window with no `WM_CLASS` — because the caller's answer
    /// is the same for all of them: report nothing, keep the last known name.
    #[must_use]
    pub fn focused_class(&self) -> Option<String> {
        let active = self
            .conn
            .get_property(false, self.root, self.active_window, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let window = active.value32()?.next()?;
        if window == 0 {
            return None;
        }
        let class = self
            .conn
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        class_of(&class.value)
    }
}

/// Parses a `WM_CLASS` value: `instance\0class\0`, class preferred.
///
/// The instance is the fallback for the rare window that sets only one field; a window
/// with neither yields `None` rather than an empty name.
fn class_of(bytes: &[u8]) -> Option<String> {
    let mut parts = bytes.split(|&b| b == 0).filter(|part| !part.is_empty());
    let instance = parts.next()?;
    let class = parts.next().unwrap_or(instance);
    Some(String::from_utf8_lossy(class).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The class half is the identifier; the instance is only a fallback.
    #[test]
    fn wm_class_prefers_the_class_half() {
        assert_eq!(
            class_of(b"Navigator\0firefox\0").as_deref(),
            Some("firefox")
        );
        assert_eq!(
            class_of(b"alacritty\0Alacritty\0").as_deref(),
            Some("Alacritty")
        );
        assert_eq!(class_of(b"lonely\0").as_deref(), Some("lonely"));
        assert_eq!(class_of(b""), None);
        assert_eq!(class_of(b"\0\0"), None);
    }
}
