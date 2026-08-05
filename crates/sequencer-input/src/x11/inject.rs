//! XTEST injection — the same clicks, delivered *above* libinput.
//!
//! The evdev/uinput backend writes to `/dev/uinput`, below the kernel input stack, so its
//! events pass through libinput before any application sees them. On at least one X11
//! machine that path caps out around 20-30 clicks/s: libinput collapses anything faster
//! into a single held button (see the README's rate-ceiling section — the events reach the
//! kernel perfectly, verified by reading the device node back, and are lost above it).
//!
//! XTEST injects into the X server's own event queue through the `XTestFakeInput` request,
//! which is where a tool like `xdotool` — or any pynput-based autoclicker — puts its events.
//! That is past libinput entirely, so the ceiling does not apply. The cost is that it only
//! reaches X11: a Wayland session, or the bare console, still needs the uinput backend. This
//! is a *second* sink, chosen at runtime when `$DISPLAY` is set, never a replacement.
//!
//! Keycodes: on Linux an X keycode is the evdev code plus 8, a fixed convention of the X
//! server's evdev handling. So this reuses the Linux backend's [`crate::linux::keymap`] for
//! the `Key`-to-evdev-code half and adds the offset, rather than querying the server's keymap
//! — it targets Linux X11 specifically, where the offset is exact, and the query would add a
//! round trip and a reverse lookup to learn something already known.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use sequencer_core::emit::{Emit, EmitAction, InputSink, SinkError};
use sequencer_core::input::{Button, Key};

/// The offset from a Linux evdev code to the X keycode for the same physical key.
pub(super) const EVDEV_TO_X_KEYCODE: u16 = 8;

/// Injects synthetic input into the running X server.
#[derive(Debug)]
pub struct XTestSink {
    conn: RustConnection,
    root: Window,
    /// What is currently pressed, newest last, so [`InputSink::release_all`] can undo a
    /// partial click in reverse — the same contract the uinput sink keeps.
    held: Vec<Pressed>,
}

/// One thing held down: an XTEST event kind plus the detail that identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pressed {
    is_key: bool,
    detail: u8,
}

impl XTestSink {
    /// Connects to the X server named by `$DISPLAY` and confirms XTEST is present.
    ///
    /// # Errors
    ///
    /// If there is no reachable X server, or it lacks the XTEST extension (vanishingly rare
    /// — it has shipped with X since the 1990s, but a refusal is honest about what happened).
    pub fn open() -> Result<Self, SinkError> {
        let (conn, screen) =
            RustConnection::connect(None).map_err(|err| SinkError::Backend(Box::new(err)))?;
        // Prove XTEST is really there rather than discovering it mid-run: query its version,
        // which fails cleanly here if the extension is absent.
        conn.xtest_get_version(2, 2)
            .map_err(|err| SinkError::Backend(Box::new(err)))?
            .reply()
            .map_err(|err| SinkError::Backend(Box::new(err)))?;
        let root = conn.setup().roots[screen].root;
        Ok(Self {
            conn,
            root,
            held: Vec::new(),
        })
    }

    /// Sends one `XTestFakeInput`. `detail` is an X keycode for a key, or a button number
    /// (1-5, 8, 9) for a button. Time 0 means "now"; the root window is the one the pointer
    /// is on, which for a click-in-place is wherever it already is.
    fn fake(&self, event_type: u8, detail: u8) -> Result<(), SinkError> {
        self.conn
            .xtest_fake_input(event_type, detail, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map(|_cookie| ())
            .map_err(|err| SinkError::Backend(Box::new(err)))
    }

    fn press(&mut self, is_key: bool, detail: u8) -> Result<(), SinkError> {
        let event_type = if is_key {
            KEY_PRESS_EVENT
        } else {
            BUTTON_PRESS_EVENT
        };
        self.fake(event_type, detail)?;
        self.held.push(Pressed { is_key, detail });
        Ok(())
    }

    fn release(&mut self, is_key: bool, detail: u8) -> Result<(), SinkError> {
        let event_type = if is_key {
            KEY_RELEASE_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(event_type, detail)?;
        if let Some(index) = self
            .held
            .iter()
            .rposition(|p| p.is_key == is_key && p.detail == detail)
        {
            self.held.remove(index);
        }
        Ok(())
    }
}

/// The X keycode for a key: its evdev code plus the fixed offset. `None` for a key with no
/// evdev code (the same keys the uinput backend cannot press either), or one whose X keycode
/// would overflow a `u8` — X keycodes are 8-247, so a real key never does.
pub(super) fn x_keycode(key: Key) -> Option<u8> {
    let evdev = crate::linux::keymap::key_to_code(key)?.0;
    u8::try_from(evdev + EVDEV_TO_X_KEYCODE).ok()
}

/// The X button number for a mouse button. X reserves 4-7 for scroll-wheel detents, so the
/// thumb buttons are 8 and 9 — the numbering `xdotool` and the X server agree on. A button
/// added to the enum later falls back to left rather than failing to compile a match that
/// cannot see it; `Button` is `#[non_exhaustive]`.
fn button_detail(button: Button) -> u8 {
    match button {
        Button::Middle => 2,
        Button::Right => 3,
        Button::Back => 8,
        Button::Forward => 9,
        // Left, and anything added to the `#[non_exhaustive]` enum later — left is the
        // safe default for a click tool, and a new button is better sent than refused.
        Button::Left | _ => 1,
    }
}

impl InputSink for XTestSink {
    fn emit(&mut self, emit: &Emit) -> Result<(), SinkError> {
        match emit.action {
            EmitAction::KeyDown(key) => {
                let detail = x_keycode(key).ok_or(SinkError::UnmappableKey(key))?;
                self.press(true, detail)
            }
            EmitAction::KeyUp(key) => {
                let detail = x_keycode(key).ok_or(SinkError::UnmappableKey(key))?;
                self.release(true, detail)
            }
            EmitAction::ButtonDown(button) => self.press(false, button_detail(button)),
            EmitAction::ButtonUp(button) => self.release(false, button_detail(button)),
            EmitAction::Scroll { .. } => Err(SinkError::Unsupported("scrolling")),
            EmitAction::CursorTo { .. } | EmitAction::CursorBy { .. } => {
                Err(SinkError::Unsupported("moving the cursor"))
            }
            _ => Err(SinkError::Unsupported(
                "an action this backend does not handle",
            )),
        }
    }

    /// XTEST batches on the connection's write buffer; this is the round trip that actually
    /// puts the queued events on the wire, which is why the trait has a `flush` at all.
    fn flush(&mut self) -> Result<(), SinkError> {
        self.conn
            .flush()
            .map_err(|err| SinkError::Backend(Box::new(err)))
    }

    fn release_all(&mut self) {
        // Best effort, in reverse, swallowing errors: this runs from the drop guard, and a
        // stuck key is worse than a lost error message. Drain so a second call is a no-op.
        for pressed in std::mem::take(&mut self.held).into_iter().rev() {
            let event_type = if pressed.is_key {
                KEY_RELEASE_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            };
            let _ = self.fake(event_type, pressed.detail);
        }
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The XTEST button numbering, including the gap X leaves for scroll detents.
    #[test]
    fn buttons_map_to_the_x_numbering_scroll_leaves_a_gap() {
        assert_eq!(button_detail(Button::Left), 1);
        assert_eq!(button_detail(Button::Middle), 2);
        assert_eq!(button_detail(Button::Right), 3);
        // 4-7 are wheel detents in X, so the thumb buttons resume at 8.
        assert_eq!(button_detail(Button::Back), 8);
        assert_eq!(button_detail(Button::Forward), 9);
    }

    /// A key's X keycode is its evdev code plus 8 — the property the whole keycode path
    /// rests on. `f` is evdev 33 (`KEY_F`), so X keycode 41.
    #[test]
    fn a_keys_x_keycode_is_its_evdev_code_plus_eight() {
        let f: Key = "f".parse().expect("f is a key");
        assert_eq!(x_keycode(f), Some(33 + 8));
        let f9: Key = "f9".parse().expect("f9 is a key");
        assert_eq!(x_keycode(f9), Some(67 + 8), "KEY_F9 is evdev 67");
    }
}
