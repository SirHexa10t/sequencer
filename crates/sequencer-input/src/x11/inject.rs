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
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use sequencer_core::emit::{Emit, EmitAction, Holdable, InputSink, SinkError};
use sequencer_core::input::{Button, Key, Mods};
use sequencer_core::time::Duration;
use std::time::Duration as StdDuration;

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
    ///
    /// **Flushed immediately, on purpose.** x11rb buffers requests until something forces
    /// a write, so a caller that emits without flushing produces *nothing* — and then the
    /// backlog lands all at once whenever the buffer happens to fill, which can strand a
    /// key-down in the server with its release still queued behind it. That is a stuck
    /// key on the user's real keyboard, so the flush is not the caller's to remember.
    /// One `write(2)` per event is the price; injection is already a syscall-per-event
    /// path, and the device backend measures rate for anyone who needs it faster.
    fn fake(&self, event_type: u8, detail: u8) -> Result<(), SinkError> {
        self.conn
            .xtest_fake_input(event_type, detail, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map(|_cookie| ())
            .map_err(|err| SinkError::Backend(Box::new(err)))?;
        self.conn
            .flush()
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
        // Visible with `-v`: the exact X keycode leaving this process. When a key seems
        // to do nothing, this is the line that separates "we never sent it" from "we
        // sent it and the desktop ignored it" — two very different bugs.
        tracing::debug!(action = ?emit.action, "XTEST inject");
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

/// A tap fired while the hand still holds modifiers the target does not name: the
/// standing keys are lifted, the target tapped clean, and the hand's keys pressed
/// back down — all real XTEST input on one connection, in one call.
///
/// This is the mechanism `xdotool key --clearmodifiers` has field-proven for years.
/// An earlier version orchestrated the same steps from the executor, interleaved
/// with other traffic, and recoloured intermittently; keeping the whole lift-tap-
/// restore on one connection with a sync barrier after the lifts is the difference.
///
/// Two windows, each about one tap wide, are the price of firing immediately: a hand
/// that releases inside one gets its modifier pressed back logically (one tap of the
/// key clears it), and a trigger press landing inside arrives unmodified.
#[derive(Debug)]
pub struct LiftedTap {
    conn: RustConnection,
    root: Window,
}

impl LiftedTap {
    /// Connects and confirms XTEST, like [`XTestSink::open`]; `None` when there is
    /// no server. Without one, contaminated taps fall back to recolourable injection.
    #[must_use]
    pub fn open() -> Option<Self> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        conn.xtest_get_version(2, 2).ok()?.reply().ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        Some(Self { conn, root })
    }

    /// Taps `targets` (down in order, up in reverse, `duration` between) with the
    /// way cleared first: keys of `held` classes the target does not name are lifted,
    /// and so is `lift_primary` when given — the trigger's own key, when the target
    /// re-presses it, whose fresh edge is otherwise impossible. Lifted modifiers are
    /// pressed back afterwards; a lifted ordinary key is not — its logical release
    /// self-heals on the hand's real release, and pressing it back would fire a
    /// second edge. Keys of classes the target names stay the hand's to supply.
    ///
    /// `false` when the server is unreachable or a key has no keycode — the caller
    /// then falls back to plain injection.
    #[must_use]
    pub fn tap(
        &self,
        targets: &[Holdable],
        held: Mods,
        lift_primary: Option<Key>,
        duration: Duration,
    ) -> bool {
        let wanted = targets
            .iter()
            .filter_map(|target| match target {
                Holdable::Key(key) => Mods::of_key(*key),
                Holdable::Button(_) => None,
            })
            .fold(Mods::NONE, Mods::and);
        // One keymap read answers every "is it physically down" question at once.
        let Some(keymap) = self.keymap() else {
            return false;
        };
        let is_down = |key: Key| {
            x_keycode(key)
                .is_some_and(|code| keymap[usize::from(code / 8)] & (1 << (code % 8)) != 0)
        };

        let mut restored: Vec<u8> = Vec::new();
        for class in Mods::CLASSES {
            if held.covers(class) && !wanted.covers(class) {
                for key in class.watch_keys() {
                    if is_down(key)
                        && let Some(code) = x_keycode(key)
                    {
                        restored.push(code);
                    }
                }
            }
        }
        let mut lifts = restored.clone();
        if let Some(primary) = lift_primary
            && is_down(primary)
        {
            let Some(code) = x_keycode(primary) else {
                return false;
            };
            lifts.push(code);
        }

        for &code in &lifts {
            if !self.fake(KEY_RELEASE_EVENT, code) {
                return false;
            }
        }
        // The sync barrier: a round trip proves the server has processed every lift
        // before a single tap event is generated behind it.
        if !lifts.is_empty()
            && self
                .conn
                .get_input_focus()
                .ok()
                .and_then(|c| c.reply().ok())
                .is_none()
        {
            return false;
        }

        // The tap itself, skipping keys of classes the hand supplies (held AND named
        // by the target — those were not lifted).
        let injected: Vec<(u8, bool)> = match collect_details(targets, held, wanted) {
            Some(details) => details,
            None => return false,
        };
        for &(detail, is_key) in &injected {
            let event_type = if is_key {
                KEY_PRESS_EVENT
            } else {
                BUTTON_PRESS_EVENT
            };
            if !self.fake(event_type, detail) {
                return false;
            }
        }
        spin_sleep::sleep(StdDuration::from_nanos(
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
        ));
        for &(detail, is_key) in injected.iter().rev() {
            let event_type = if is_key {
                KEY_RELEASE_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            };
            if !self.fake(event_type, detail) {
                return false;
            }
        }

        for &code in restored.iter().rev() {
            if !self.fake(KEY_PRESS_EVENT, code) {
                return false;
            }
        }
        self.conn.flush().is_ok()
    }

    /// The server's 256-bit key-state bitmap, or `None` when it cannot be asked.
    fn keymap(&self) -> Option<[u8; 32]> {
        Some(self.conn.query_keymap().ok()?.reply().ok()?.keys)
    }

    /// One `XTestFakeInput`, flushed immediately — the same non-negotiable flush as
    /// [`XTestSink::fake`], for the same stuck-key reason. Logged with the same
    /// "XTEST inject" marker too, so `-v` (and the live suite's injection counter)
    /// sees every synthetic event regardless of which path fired it.
    fn fake(&self, event_type: u8, detail: u8) -> bool {
        tracing::debug!(event_type, detail, "XTEST inject (lifted tap)");
        self.conn
            .xtest_fake_input(event_type, detail, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .is_ok()
            && self.conn.flush().is_ok()
    }
}

/// The XTEST details for a tap's injected members: every target except keys of
/// classes the hand supplies (`held` and `wanted` both cover them). `None` when a
/// key has no keycode.
fn collect_details(targets: &[Holdable], held: Mods, wanted: Mods) -> Option<Vec<(u8, bool)>> {
    let mut details = Vec::with_capacity(targets.len());
    for target in targets {
        match target {
            Holdable::Key(key) => {
                let hand_supplies = Mods::of_key(*key)
                    .is_some_and(|class| held.covers(class) && wanted.covers(class));
                if !hand_supplies {
                    details.push((x_keycode(*key)?, true));
                }
            }
            Holdable::Button(button) => details.push((button_detail(*button), false)),
        }
    }
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The media keys land where a standard Linux X keymap puts them — the numbers a
    /// user can check against `xmodmap -pke`. Off-by-one here is invisible at runtime:
    /// the server accepts any keycode and simply does nothing useful with a wrong one.
    #[test]
    fn volume_keys_land_on_the_standard_x_keycodes() {
        assert_eq!(x_keycode(Key::VolumeUp), Some(123));
        assert_eq!(x_keycode(Key::VolumeDown), Some(122));
        assert_eq!(x_keycode(Key::Mute), Some(121));
    }

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
