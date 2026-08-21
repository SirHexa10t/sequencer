//! Which keyboard layout is active, by asking the X server.
//!
//! XKB keeps every configured layout loaded at once as numbered *groups* and one of them
//! current; switching language is switching group. Two reads answer it: the extension
//! says which group is current, and the root window's `_XKB_RULES_NAMES` property lists
//! the layouts in group order — the same list `setxkbmap -query` prints.
//!
//! What comes back is the layout's own name (`us`, `il`, `ru`), never an invented
//! language label. That is the spelling the system already uses, it is what `detect-key`
//! prints, and it is therefore what a binds file's `kb_lang` matches — the same bargain
//! [`super::focus`] strikes with `WM_CLASS`: report the identifier the machine has rather
//! than a friendlier one that would need a table nobody can keep complete.
//!
//! Polling, like focus: the consumers already wake on their own cadence, and two reads
//! over a local socket cost microseconds.

use x11rb::connection::Connection as _;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

/// Reads the active layout's name on demand.
#[derive(Debug)]
pub struct LayoutWatcher {
    conn: RustConnection,
    root: Window,
    rules_names: u32,
}

impl LayoutWatcher {
    /// Connects and enables XKB. `None` when there is no server to ask or it has no
    /// XKB — layout reporting is garnish, and a session without it goes without, which
    /// is why this is not a `Result`.
    #[must_use]
    pub fn open() -> Option<Self> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        // Required before any other XKB request; the version is the one every server
        // implementing the extension at all supports.
        conn.xkb_use_extension(1, 0).ok()?.reply().ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let rules_names = conn
            .intern_atom(false, b"_XKB_RULES_NAMES")
            .ok()?
            .reply()
            .ok()?
            .atom;
        Some(Self {
            conn,
            root,
            rules_names,
        })
    }

    /// The current layout's name, or `None` when it cannot be named.
    ///
    /// `None` covers a server that will not answer and a session whose rules property
    /// is missing or too short for the group. Callers treat it as "unknown", never as a
    /// name of its own: a profile gated on the layout must not fire on a guess.
    #[must_use]
    pub fn current(&self) -> Option<String> {
        let state = self
            .conn
            .xkb_get_state(xkb::ID::USE_CORE_KBD.into())
            .ok()?
            .reply()
            .ok()?;
        let names = self
            .conn
            .get_property(
                false,
                self.root,
                self.rules_names,
                AtomEnum::STRING,
                0,
                1024,
            )
            .ok()?
            .reply()
            .ok()?;
        layout_for(&names.value, u8::from(state.group))
    }
}

/// The `group`th layout named in a `_XKB_RULES_NAMES` value.
///
/// The property is NUL-separated `rules`, `model`, `layout`, `variant`, `options`, where
/// the layout field is comma-separated in group order (`us,il,us`). A group past the end
/// of that list is not an error to guess at — some setups load fewer names than groups —
/// so it reads as unknown.
fn layout_for(property: &[u8], group: u8) -> Option<String> {
    let layouts = property.split(|&byte| byte == 0).nth(2)?;
    let name = layouts
        .split(|&byte| byte == b',')
        .nth(usize::from(group))
        .filter(|name| !name.is_empty())?;
    Some(String::from_utf8_lossy(name).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property's third field, indexed by group — the whole rule.
    #[test]
    fn the_group_indexes_the_layout_list() {
        let property = b"evdev\0pc105\0us,il,ru\0,,\0grp:alt_shift_toggle\0";
        assert_eq!(layout_for(property, 0).as_deref(), Some("us"));
        assert_eq!(layout_for(property, 1).as_deref(), Some("il"));
        assert_eq!(layout_for(property, 2).as_deref(), Some("ru"));
        // A group with no name of its own is unknown, not a wrong answer.
        assert_eq!(layout_for(property, 3), None);
        assert_eq!(layout_for(b"evdev\0pc105\0us\0", 1), None);
        assert_eq!(layout_for(b"", 0), None);
        assert_eq!(layout_for(b"evdev\0pc105\0", 0), None);
        // A trailing empty name (`us,`) is a name of nothing.
        assert_eq!(layout_for(b"evdev\0pc105\0us,\0,\0\0", 1), None);
    }
}
