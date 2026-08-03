//! Watching `/dev/input` for the trigger key.
//!
//! Reading the device nodes directly is what makes this work the same on X11, on Wayland,
//! and on a bare console — it sits below the display server rather than asking one for
//! permission. The cost is that it needs read access to `/dev/input`, which
//! [`crate::Requirement::EvdevReadable`] explains.
//!
//! **Observe only.** The devices are not grabbed exclusively, so a trigger key still does
//! its normal thing — pressing F9 both starts the clicker and sends F9 to whatever has
//! focus. That matches the Python prototype's behaviour. Hiding the trigger would mean an
//! exclusive `EVIOCGRAB` and re-injecting every other keystroke, which puts the process
//! between the user and their keyboard: if it hangs while holding the grab, the keyboard
//! stops responding until it is killed from another virtual terminal. Not a trade worth
//! making for an autoclicker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use evdev::{Device, EventSummary};
use sequencer_core::input::{EventKind, InputEvent};

use crate::capture::{CaptureStream, Epoch, EventQueue};
use crate::linux::inject::DEVICE_NAME;
use crate::linux::keymap;

/// Bound on the queue between the reader threads and the runner.
const QUEUE_CAPACITY: usize = 1024;

/// Reads key and button events from every input device on the machine.
#[derive(Debug)]
pub struct EvdevCapture {
    epoch: Epoch,
    running: Arc<AtomicBool>,
    watching: usize,
}

impl EvdevCapture {
    /// Prepares a capture backend sharing `epoch` with the runner's clock.
    #[must_use]
    pub fn new(epoch: Epoch) -> Self {
        Self {
            epoch,
            running: Arc::new(AtomicBool::new(false)),
            watching: 0,
        }
    }

    /// How many devices are being read.
    #[must_use]
    pub const fn watching(&self) -> usize {
        self.watching
    }

    /// Every device that reports keys or buttons, excluding our own virtual one.
    #[must_use]
    pub fn openable_devices() -> Vec<(std::path::PathBuf, Device)> {
        let mut found = Vec::new();
        for (path, device) in evdev::enumerate() {
            // Our own uinput device reports keys too, so without this filter the clicker
            // would read its own clicks back and, with a mouse-button trigger, retrigger
            // itself. Excluding by name is exact and needs no timing heuristics.
            if device.name() == Some(DEVICE_NAME) {
                continue;
            }
            if device.supported_keys().is_some() {
                found.push((path, device));
            }
        }
        found
    }

    /// Starts listening, returning the consuming half of the event queue.
    ///
    /// # Errors
    ///
    /// If already started, or if no input device can be opened — which on a fresh
    /// machine means the `input` group setup is missing, and the error says so.
    pub fn start(&mut self) -> Result<CaptureStream, CaptureError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(CaptureError::AlreadyStarted);
        }

        let devices = Self::openable_devices();
        if devices.is_empty() {
            return Err(CaptureError::NoReadableDevices);
        }

        let (queue, stream) = CaptureStream::channel(QUEUE_CAPACITY);
        self.running.store(true, Ordering::SeqCst);
        self.watching = devices.len();

        for (path, device) in devices {
            let queue = queue.clone();
            let epoch = self.epoch.clone();
            let running = Arc::clone(&self.running);
            let name = device.name().unwrap_or("unnamed").to_owned();

            // One thread per device, each parked in a blocking read. Detached on purpose:
            // there is no portable way to interrupt a blocking read, and a thread waiting
            // on a device the user is not touching has nothing to clean up. The process
            // exits without joining them, and the kernel closes the descriptors.
            let spawned = thread::Builder::new()
                .name(format!("sequencer-capture-{name}"))
                .spawn(move || pump_device(device, &queue, &epoch, &running));

            match spawned {
                Ok(_handle) => tracing::debug!(device = %name, path = %path.display(), "watching"),
                Err(err) => tracing::warn!(device = %name, %err, "could not watch device"),
            }
        }

        Ok(stream)
    }

    /// Stops listening.
    ///
    /// The reader threads notice on their next event and return. They may sit blocked
    /// until then, which is harmless: they hold nothing and the process can exit.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self.watching = 0;
    }
}

/// Reads one device until it goes away or capture stops.
fn pump_device(mut device: Device, queue: &EventQueue, epoch: &Epoch, running: &AtomicBool) {
    while running.load(Ordering::Relaxed) {
        let events = match device.fetch_events() {
            Ok(events) => events,
            // A device being unplugged is ordinary, not an error worth shouting about.
            Err(err) => {
                tracing::debug!(%err, "stopped reading device");
                return;
            }
        };
        for event in events {
            let Some(kind) = translate(event) else {
                continue;
            };
            // Stamped on arrival, on the runner's timeline, so repeat cadence
            // phase-locks to the press rather than to when the loop got around to it.
            if !queue.offer(InputEvent::physical(epoch.now(), kind)) {
                // The queue is bounded and the counter is surfaced by the runner; there
                // is nothing useful to do here but keep reading.
                tracing::trace!("capture queue full, event dropped");
            }
        }
    }
}

/// Turns an evdev event into one this crate understands, or `None` to ignore it.
fn translate(event: evdev::InputEvent) -> Option<EventKind> {
    let EventSummary::Key(_, code, value) = event.destructure() else {
        return None;
    };
    // 0 is release, 1 is press, 2 is the kernel's auto-repeat. The engine derives its own
    // edges from transitions, so forwarding repeats would be noise.
    let pressed = match value {
        0 => false,
        1 => true,
        _ => return None,
    };

    if let Some(button) = keymap::code_to_button(code) {
        return Some(if pressed {
            EventKind::ButtonDown(button)
        } else {
            EventKind::ButtonUp(button)
        });
    }
    let key = keymap::code_to_key(code)?;
    Some(if pressed {
        EventKind::KeyDown(key)
    } else {
        EventKind::KeyUp(key)
    })
}

/// Capture could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureError {
    /// [`EvdevCapture::start`] was called twice.
    #[error("capture is already running")]
    AlreadyStarted,
    /// Nothing in `/dev/input` could be opened.
    #[error(
        "no readable input devices in /dev/input; run `sequencer doctor` for the group \
         membership this needs"
    )]
    NoReadableDevices,
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::{EventType, KeyCode};
    use sequencer_core::input::{Button, Key};

    fn key_event(code: KeyCode, value: i32) -> evdev::InputEvent {
        evdev::InputEvent::new(EventType::KEY.0, code.0, value)
    }

    #[test]
    fn presses_and_releases_translate() {
        assert_eq!(
            translate(key_event(KeyCode::KEY_F9, 1)),
            Some(EventKind::KeyDown(Key::F9))
        );
        assert_eq!(
            translate(key_event(KeyCode::KEY_F9, 0)),
            Some(EventKind::KeyUp(Key::F9))
        );
    }

    #[test]
    fn buttons_translate_as_buttons_not_keys() {
        // At the evdev layer a button is just another key code, so the button table has
        // to be consulted first or BTN_LEFT would come through as a nonexistent key.
        assert_eq!(
            translate(key_event(KeyCode::BTN_LEFT, 1)),
            Some(EventKind::ButtonDown(Button::Left))
        );
        assert_eq!(
            translate(key_event(KeyCode::BTN_SIDE, 0)),
            Some(EventKind::ButtonUp(Button::Back))
        );
    }

    #[test]
    fn kernel_auto_repeat_is_ignored() {
        // Value 2 is the kernel repeating a held key. The engine derives edges from
        // transitions, so passing these along would be pure noise.
        assert_eq!(translate(key_event(KeyCode::KEY_F9, 2)), None);
    }

    #[test]
    fn unmapped_codes_and_other_event_types_are_ignored() {
        assert_eq!(translate(key_event(KeyCode(0xFFF), 1)), None);
        assert_eq!(
            translate(evdev::InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0)),
            None
        );
    }

    #[test]
    fn starting_with_no_readable_devices_says_so() {
        // On a machine with devices this starts successfully instead; either way it must
        // not panic, and the no-device path must name the fix.
        let mut capture = EvdevCapture::new(Epoch::start());
        match capture.start() {
            Ok(_stream) => {
                assert!(capture.watching() > 0);
                capture.stop();
            }
            Err(err) => {
                assert!(err.to_string().contains("doctor") || err.to_string().contains("input"));
            }
        }
    }
}
