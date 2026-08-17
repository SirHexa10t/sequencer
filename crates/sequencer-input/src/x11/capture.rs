//! Hotkeys by X11 key grab — hearing the activation key without reading `/dev/input`.
//!
//! The evdev capture watches every input device, which needs read access the kernel guards
//! (the `input` group, or session mode's sudo). On X11 there is a narrower ask that needs no
//! privilege at all: a **passive key grab** on the root window for exactly the keys the run
//! binds — the activation key and the quit key. The server then delivers those presses to
//! this connection whichever window has focus, which is the whole job.
//!
//! Two ways this deliberately differs from the evdev capture:
//!
//! - **It sees only the grabbed keys.** Evdev capture forwards everything and lets the engine
//!   ignore the rest; a grab that broad would be a keylogger. The caller names the keys.
//! - **The grabbed keys stop reaching other applications** while the run is active. A grab is
//!   exclusive — that is how the server knows who wants the key. For an autoclicker's
//!   hotkeys this is arguably the better behaviour (F9 no longer also types into the focused
//!   window), but it is a real difference from the evdev backend's silent observation.
//!
//! Grabs use [`ModMask::ANY`], so the hotkey works with NumLock on, CapsLock on, or Shift
//! held. If *another* client already holds a grab on the key ([`GrabError::AlreadyGrabbed`]),
//! the run refuses and names the key. Falling back to reading devices would demand access,
//! and possibly a password, for a problem no privilege can fix — the answer is a different
//! `--activate` key, so that is what the error says.
//!
//! Events are stamped on arrival with the shared [`Epoch`] and pushed into the same
//! [`EventQueue`] the evdev backend fills, so the pump above is one code path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask};
use x11rb::rust_connection::RustConnection;

use sequencer_core::input::{InputEvent, Key, Mods};

use crate::capture::{CaptureStream, Epoch, EventQueue};

/// How often the reader thread wakes to check the stop flag while no events arrive.
///
/// Polling instead of blocking in `wait_for_event`, because a blocked thread holding the
/// connection cannot be told to stop — and the X protocol has no portable "unblock my own
/// read" request. Five milliseconds keeps added hotkey latency imperceptible (the event is
/// stamped when *read*, and a human key press does not resolve 5ms) at a wake-up cost of two
/// hundred polls a second, which profiling the evdev path already showed is noise.
const POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(5);

/// What stopped a grab from being taken.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrabError {
    /// No X server to talk to, or the conversation failed.
    #[error("cannot reach the X server: {0}")]
    Connect(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The key has no X keycode — nothing a grab could name.
    #[error("{0} has no X keycode to grab")]
    Unmappable(Key),
    /// A chord names more than one ordinary key; X can grab only one plus modifiers.
    #[error("`{0}` cannot join a chord trigger: X grabs one key plus modifiers")]
    Unchordable(Key),

    /// A chord of nothing but modifiers has no key to grab.
    #[error("a trigger needs one non-modifier key, not modifiers alone")]
    ModifiersOnly,

    /// Another client already owns a grab on this key (a desktop shortcut, usually).
    #[error(
        "{0} is already grabbed by another program (a desktop keyboard shortcut?) — \
         pick a different key with --activate/--quit"
    )]
    AlreadyGrabbed(Key),
}

/// Grabs the named keys and feeds their presses into a [`CaptureStream`].
#[derive(Debug)]
pub struct GrabCapture {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GrabCapture {
    /// Connects, grabs `keys` on the root window, and starts the reader thread.
    ///
    /// # Errors
    ///
    /// [`GrabError`] — no server, an unmappable key, or a key someone else holds. Nothing is
    /// left half-done on failure: the connection drops, and dropping it releases any grabs
    /// this client took.
    pub fn start(epoch: &Epoch, keys: &[Key]) -> Result<(Self, CaptureStream), GrabError> {
        let chords: Vec<Vec<Key>> = keys.iter().map(|key| alloc_one(*key)).collect();
        let (queue, stream) = CaptureStream::channel(256);
        let capture = Self::start_into(epoch, &chords, queue)?;
        Ok((capture, stream))
    }

    /// [`GrabCapture::start`], feeding an existing queue instead of creating one.
    ///
    /// This is what lets several grabs — one per profile, plus an emergency grab —
    /// share a single stream, so a run loop can *block* on all of them at once rather
    /// than take turns polling each.
    ///
    /// # Errors
    ///
    /// As [`GrabCapture::start`].
    pub fn start_into(
        epoch: &Epoch,
        chords: &[Vec<Key>],
        queue: EventQueue,
    ) -> Result<Self, GrabError> {
        let (conn, screen) =
            RustConnection::connect(None).map_err(|err| GrabError::Connect(Box::new(err)))?;
        let root = conn.setup().roots[screen].root;

        for chord in chords {
            let (mods, key) = split_chord(chord)?;
            let keycode = super::inject::x_keycode(key).ok_or(GrabError::Unmappable(key))?;
            // A bare key is grabbed with ModMask::ANY — F9 is F9 with NumLock on. A chord
            // names its modifiers exactly, so the plain key stays usable on its own; the
            // lock variants below are why NumLock does not break it either.
            for mask in mask_variants(mods) {
                conn.grab_key(false, root, mask, keycode, GrabMode::ASYNC, GrabMode::ASYNC)
                    .map_err(|err| GrabError::Connect(Box::new(err)))?
                    .check()
                    .map_err(|_| GrabError::AlreadyGrabbed(key))?;
            }
        }
        conn.flush()
            .map_err(|err| GrabError::Connect(Box::new(err)))?;

        let running = Arc::new(AtomicBool::new(true));
        let thread = {
            let running = Arc::clone(&running);
            let epoch = epoch.clone();
            std::thread::Builder::new()
                .name("x11-grab-capture".into())
                .spawn(move || pump_events(&conn, &queue, &epoch, &running))
                .map_err(|err| GrabError::Connect(Box::new(err)))?
        };
        Ok(Self {
            running,
            thread: Some(thread),
        })
    }

    /// Stops the reader thread and, with it, the grabs (dropping the connection ungrabs).
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GrabCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Reads grabbed-key events until told to stop, translating each to the engine's shape.
fn pump_events(conn: &RustConnection, queue: &EventQueue, epoch: &Epoch, running: &AtomicBool) {
    while running.load(Ordering::Relaxed) {
        match conn.poll_for_event() {
            Ok(Some(event)) => {
                // A passive grab becomes an ACTIVE keyboard grab the instant its key
                // goes down, and while that is in effect the server routes keyboard
                // events to this client rather than activating anyone else's passive
                // grab. Injected keys would therefore land back on us — the desktop's
                // own binding for, say, XF86AudioRaiseVolume would never fire. Ending
                // the active grab immediately (the passive registration survives, so
                // the next press still arrives) is what lets injection reach the rest
                // of the session.
                if matches!(event, Event::KeyPress(_)) {
                    let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
                    let _ = conn.flush();
                }
                let Some((kind, mods)) = translate(&event) else {
                    continue;
                };
                // Stamped on arrival, on the runner's timeline — same rule as the evdev
                // reader, so cadence phase-locks to the press either way.
                if !queue.offer(InputEvent::physical(epoch.now(), kind).with_mods(mods)) {
                    tracing::trace!("capture queue full, event dropped");
                }
            }
            Ok(None) => std::thread::sleep(POLL_EVERY),
            Err(err) => {
                // The server hung up (session ended, most likely). The queue closing on
                // drop is what tells the pump, which tells the engine to shut down.
                tracing::debug!(%err, "X connection lost; capture stopping");
                return;
            }
        }
    }
}

/// One key as a one-element chord.
fn alloc_one(key: Key) -> Vec<Key> {
    vec![key]
}

/// Splits a chord into its modifier mask and the single key it decorates.
///
/// X grabs one keycode plus a modifier state, which is exactly a chord's shape — but it
/// means a chord may carry only *one* non-modifier key: "ctrl i" is grabbable, "a b" is
/// not, because no modifier state describes "a is also down".
fn split_chord(chord: &[Key]) -> Result<(ModMask, Key), GrabError> {
    let mut mods = ModMask::default();
    let mut primary = None;
    for &key in chord {
        match modifier_mask(key) {
            Some(mask) => mods |= mask,
            None => {
                if primary.replace(key).is_some() {
                    return Err(GrabError::Unchordable(key));
                }
            }
        }
    }
    // An all-modifier chord ("ctrl shift") has nothing to hang the grab on: X delivers
    // modifier presses to a grab only as the state of some other key.
    primary
        .map(|key| (mods, key))
        .ok_or(GrabError::ModifiersOnly)
}

/// The X modifier bit a key contributes, or `None` if it is not a modifier.
///
/// Left and right map to the same bit, as X itself does: `Shift` is `Shift` whichever
/// one is held. Alt is Mod1 and Meta/Super is Mod4 by near-universal convention; AltGr
/// (right alt) is Mod5 where a layout defines it.
fn modifier_mask(key: Key) -> Option<ModMask> {
    Some(match key {
        Key::LeftShift | Key::RightShift => ModMask::SHIFT,
        Key::LeftCtrl | Key::RightCtrl => ModMask::CONTROL,
        Key::LeftAlt => ModMask::M1,
        Key::LeftMeta | Key::RightMeta => ModMask::M4,
        Key::RightAlt => ModMask::M5,
        _ => return None,
    })
}

/// The same chord with every combination of the "who cares" lock bits.
///
/// CapsLock (Lock) and NumLock (Mod2) sit in the same state field as real modifiers, so
/// a grab that names neither simply never fires while either is on. Every hotkey tool
/// grabs all four permutations for this reason.
fn mask_variants(mods: ModMask) -> Vec<ModMask> {
    if mods == ModMask::default() {
        // A bare key wants ANY, which already covers the locks.
        return vec![ModMask::ANY];
    }
    vec![
        mods,
        mods | ModMask::LOCK,
        mods | ModMask::M2,
        mods | ModMask::LOCK | ModMask::M2,
    ]
}

/// A grabbed-key X event as the engine's event kind plus the modifier classes that
/// were held; anything else is `None`.
///
/// The modifiers matter because this is the only place they exist: a grab fires a
/// chord as one key-down of its ordinary key, and the chord's other half lives in the
/// event's `state` field. Dropping it here once made `ctrl w` indistinguishable from
/// `ctrl shift w` downstream.
///
/// X's auto-repeat arrives as release/press pairs at the same timestamp; unlike the evdev
/// path there is no repeat flag to filter on. Harmless here: the engine derives its own
/// edges from state transitions, so a re-press of a held key is a no-op — the same reason
/// the evdev reader could afford to drop kernel repeats rather than needing to.
fn translate(event: &Event) -> Option<(sequencer_core::input::EventKind, Mods)> {
    use sequencer_core::input::EventKind;
    match event {
        Event::KeyPress(press) => Some((
            EventKind::KeyDown(key_from_x(press.detail)?),
            mods_of(press.state),
        )),
        Event::KeyRelease(release) => Some((
            EventKind::KeyUp(key_from_x(release.detail)?),
            mods_of(release.state),
        )),
        // MappingNotify and friends: nothing the engine binds to.
        _ => None,
    }
}

/// The engine's modifier classes for an X state mask. Locks (Lock, Mod2) are not
/// modifiers a chord can name, so they are not classes either.
fn mods_of(state: x11rb::protocol::xproto::KeyButMask) -> Mods {
    use x11rb::protocol::xproto::KeyButMask;
    [
        (KeyButMask::SHIFT, Mods::SHIFT),
        (KeyButMask::CONTROL, Mods::CTRL),
        (KeyButMask::MOD1, Mods::ALT),
        (KeyButMask::MOD5, Mods::RALT),
        (KeyButMask::MOD4, Mods::META),
    ]
    .into_iter()
    .filter(|(bit, _)| state.contains(*bit))
    .fold(Mods::NONE, |mods, (_, class)| mods.and(class))
}

/// Reads the server's live key state, for the one question grabs cannot answer:
/// is a key still physically down *right now*?
///
/// A deferred tap needs it — after ungrabbing (see [`pump_events`]) the trigger's
/// releases are routed elsewhere, so release events never arrive here; polling the
/// server's keymap is the honest way to see the hand leave the keys.
#[derive(Debug)]
pub struct KeyProbe {
    conn: RustConnection,
}

impl KeyProbe {
    /// Connects; `None` when there is no server to ask.
    #[must_use]
    pub fn open() -> Option<Self> {
        let (conn, _) = RustConnection::connect(None).ok()?;
        Some(Self { conn })
    }

    /// Whether any of `keys` is down right now. Unaskable states read as "none down"
    /// so a lost connection degrades to firing immediately, never to waiting forever.
    #[must_use]
    pub fn any_down(&self, keys: &[Key]) -> bool {
        let Ok(cookie) = self.conn.query_keymap() else {
            return false;
        };
        let Ok(reply) = cookie.reply() else {
            return false;
        };
        keys.iter()
            .filter_map(|&key| super::inject::x_keycode(key))
            .any(|code| reply.keys[usize::from(code / 8)] & (1 << (code % 8)) != 0)
    }
}

/// The inverse of [`super::inject::x_keycode`]: X keycode back to the engine's key.
fn key_from_x(keycode: u8) -> Option<Key> {
    let code = u16::from(keycode).checked_sub(super::inject::EVDEV_TO_X_KEYCODE)?;
    crate::linux::keymap::code_to_key(evdev::KeyCode(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two directions must agree, or a grab would name one key and the event another.
    #[test]
    fn x_keycodes_round_trip_through_inject_and_capture() {
        for name in ["f8", "f9", "a", "space"] {
            let key: Key = name.parse().expect("a real key");
            let code = super::super::inject::x_keycode(key).expect("mappable");
            assert_eq!(key_from_x(code), Some(key), "{name} did not round-trip");
        }
    }

    /// A keycode below the offset is not a key (X reserves 0-7); it must not underflow.
    #[test]
    fn impossible_keycodes_translate_to_nothing() {
        assert_eq!(key_from_x(0), None);
        assert_eq!(key_from_x(7), None);
    }

    /// A key another program owns names itself and points at the fix. The caller turns
    /// this into a usage error rather than falling back to reading devices, so the message
    /// is the only thing the user gets — it has to be the actionable one.
    #[test]
    fn an_already_grabbed_key_names_itself_and_the_flags_that_change_it() {
        let message = GrabError::AlreadyGrabbed(Key::F9).to_string();
        assert!(
            message.contains("f9") || message.contains("F9"),
            "{message}"
        );
        assert!(
            message.contains("--activate") && message.contains("--quit"),
            "the fix is a different key, and the message must say which flags: {message}"
        );
    }
}
