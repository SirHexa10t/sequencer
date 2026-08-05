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

use sequencer_core::input::{InputEvent, Key};

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
        let (conn, screen) =
            RustConnection::connect(None).map_err(|err| GrabError::Connect(Box::new(err)))?;
        let root = conn.setup().roots[screen].root;

        for &key in keys {
            let keycode = super::inject::x_keycode(key).ok_or(GrabError::Unmappable(key))?;
            // ANY modifier state: F9 is F9 with NumLock on too. Checked immediately — an
            // `Access` error here means another client got there first, and starting a run
            // whose hotkey silently never fires would be far worse than refusing.
            conn.grab_key(
                false,
                root,
                ModMask::ANY,
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|err| GrabError::Connect(Box::new(err)))?
            .check()
            .map_err(|_| GrabError::AlreadyGrabbed(key))?;
        }
        conn.flush()
            .map_err(|err| GrabError::Connect(Box::new(err)))?;

        let (queue, stream) = CaptureStream::channel(256);
        let running = Arc::new(AtomicBool::new(true));
        let thread = {
            let running = Arc::clone(&running);
            let epoch = epoch.clone();
            std::thread::Builder::new()
                .name("x11-grab-capture".into())
                .spawn(move || pump_events(&conn, &queue, &epoch, &running))
                .map_err(|err| GrabError::Connect(Box::new(err)))?
        };
        Ok((
            Self {
                running,
                thread: Some(thread),
            },
            stream,
        ))
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
                let Some(kind) = translate(&event) else {
                    continue;
                };
                // Stamped on arrival, on the runner's timeline — same rule as the evdev
                // reader, so cadence phase-locks to the press either way.
                if !queue.offer(InputEvent::physical(epoch.now(), kind)) {
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

/// A grabbed-key X event as the engine's event kind; anything else is `None`.
///
/// X's auto-repeat arrives as release/press pairs at the same timestamp; unlike the evdev
/// path there is no repeat flag to filter on. Harmless here: the engine derives its own
/// edges from state transitions, so a re-press of a held key is a no-op — the same reason
/// the evdev reader could afford to drop kernel repeats rather than needing to.
fn translate(event: &Event) -> Option<sequencer_core::input::EventKind> {
    use sequencer_core::input::EventKind;
    match event {
        Event::KeyPress(press) => Some(EventKind::KeyDown(key_from_x(press.detail)?)),
        Event::KeyRelease(release) => Some(EventKind::KeyUp(key_from_x(release.detail)?)),
        // MappingNotify and friends: nothing the engine binds to.
        _ => None,
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
