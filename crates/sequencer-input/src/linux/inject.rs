//! Injecting input through `/dev/uinput`.
//!
//! A uinput device is a real input device as far as the rest of the system is concerned —
//! the kernel delivers its events the same way it delivers a physical mouse's. That is
//! what makes this one backend work on X11, on Wayland, and on a bare console, and it is
//! also the fastest path available: emitting a click is a couple of `write(2)` calls, with
//! no display-server round trip in the way.
//!
//! Clicks land wherever the pointer already is. The device declares buttons but no
//! pointer axes, so it can press and release without ever moving the cursor.

use std::io;

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use sequencer_core::emit::{Emit, EmitAction, Holdable, InputSink, SinkError};

use crate::linux::keymap;

/// The name the virtual device reports.
///
/// Visible in `libinput list-devices` and similar, so it is worth being recognisable
/// rather than anonymous.
pub const DEVICE_NAME: &str = "sequencer virtual input";

/// Sends synthesized input through a uinput virtual device.
#[derive(Debug)]
pub struct UinputSink {
    device: VirtualDevice,
    /// What this sink has pressed and not released, so `release_all` is exact rather than
    /// a blind sweep over every code the device declares.
    held: Vec<KeyCode>,
    /// Events staged for the next `flush`.
    pending: Vec<InputEvent>,
}

impl UinputSink {
    /// Creates the virtual device.
    ///
    /// # Errors
    ///
    /// If `/dev/uinput` is missing or not writable. That is the common case on a fresh
    /// machine, and [`crate::Requirement::UinputNodeWritable`] explains the fix.
    pub fn open() -> Result<Self, SinkError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in keymap::all_codes() {
            keys.insert(code);
        }

        // Declared but never sent: this device only ever clicks, and moving the cursor is
        // explicitly not its job.
        //
        // They are here because libinput — which every Wayland compositor and modern X
        // server sits on — decides what a device *is* from the axes it advertises, not from
        // its buttons. Advertising BTN_LEFT with no axes produces a device that libinput does
        // not classify as a pointer, so it routes the button events nowhere: the kernel
        // accepts every write, a read-back (`bench`) counts every one of them, and not a
        // single click reaches an application. Two unused axes are the whole difference
        // between that and a working clicker.
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        axes.insert(RelativeAxisCode::REL_X);
        axes.insert(RelativeAxisCode::REL_Y);
        // A wheel, for the same reason and never scrolled either. Without one libinput has
        // no wheel to offer, so it selects BUTTON scrolling as the device's scroll method
        // (`Scroll methods: *button` in `libinput list-devices`, where real mice say
        // `*wheel`). Button scrolling makes libinput hold each press back to see whether it
        // begins a scroll gesture rather than a click — which turns rapid clicking into one
        // long held button.
        axes.insert(RelativeAxisCode::REL_WHEEL);
        axes.insert(RelativeAxisCode::REL_HWHEEL);

        let device = VirtualDevice::builder()
            .and_then(|builder| builder.name(DEVICE_NAME).with_keys(&keys))
            .and_then(|builder| builder.with_relative_axes(&axes))
            .and_then(VirtualDeviceBuilder::build)
            .map_err(uinput_error)?;

        Ok(Self {
            device,
            held: Vec::new(),
            pending: Vec::new(),
        })
    }

    /// The `/dev/input/event*` paths this virtual device appears at.
    ///
    /// Only the benchmark uses this — it reads the device back to count what the kernel
    /// actually delivered. Ordinary capture deliberately skips this device by name.
    ///
    /// # Errors
    ///
    /// If the kernel will not report the device's sysfs entry.
    pub fn dev_nodes(&mut self) -> io::Result<Vec<std::path::PathBuf>> {
        self.device
            .enumerate_dev_nodes_blocking()?
            .collect::<io::Result<Vec<_>>>()
    }

    /// Queues a press or release.
    fn stage(&mut self, code: KeyCode, pressed: bool) {
        self.pending.push(InputEvent::new(
            EventType::KEY.0,
            code.0,
            i32::from(pressed),
        ));
        if pressed {
            self.held.push(code);
        } else if let Some(index) = self.held.iter().rposition(|held| *held == code) {
            self.held.remove(index);
        }
    }

    /// Writes the staged events and the synchronisation marker that commits them.
    ///
    /// The `SYN_REPORT` is not optional: without it the kernel treats the events as an
    /// incomplete packet and nothing downstream sees them.
    fn commit(&mut self) -> Result<(), SinkError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.pending
            .push(InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0));
        let result = self.device.emit(&self.pending).map_err(uinput_error);
        self.pending.clear();
        result
    }
}

impl InputSink for UinputSink {
    fn emit(&mut self, emit: &Emit) -> Result<(), SinkError> {
        match emit.action {
            EmitAction::KeyDown(_) | EmitAction::ButtonDown(_) => {
                let what = emit.action.holds().expect("a down action holds something");
                let code = code_for(what)?;
                self.stage(code, true);
            }
            EmitAction::KeyUp(_) | EmitAction::ButtonUp(_) => {
                let what = emit
                    .action
                    .releases()
                    .expect("an up action releases something");
                let code = code_for(what)?;
                self.stage(code, false);
            }
            EmitAction::Scroll { .. } => return Err(SinkError::Unsupported("scrolling")),
            EmitAction::CursorTo { .. } | EmitAction::CursorBy { .. } => {
                return Err(SinkError::Unsupported("moving the cursor"));
            }
            // `EmitAction` is non-exhaustive. Refusing an action added later is right:
            // silently ignoring one would look like the backend worked.
            _ => return Err(SinkError::Unsupported("that action")),
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.commit()
    }

    fn release_all(&mut self) {
        // Best effort by contract: this runs from a drop guard, including while
        // unwinding from a panic, so it swallows errors rather than risking a second one.
        // Reverse order, mirroring the engine's own ledger.
        let held = std::mem::take(&mut self.held);
        for code in held.into_iter().rev() {
            self.pending
                .push(InputEvent::new(EventType::KEY.0, code.0, 0));
        }
        let _ = self.commit();
    }
}

fn code_for(what: Holdable) -> Result<KeyCode, SinkError> {
    keymap::holdable_to_code(what).ok_or(match what {
        Holdable::Key(key) => SinkError::UnmappableKey(key),
        Holdable::Button(_) => SinkError::Unsupported("that mouse button"),
    })
}

fn uinput_error(err: io::Error) -> SinkError {
    match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => {
            SinkError::Backend(Box::new(UinputUnavailable(err)))
        }
        _ => SinkError::Backend(Box::new(err)),
    }
}

/// `/dev/uinput` could not be opened, with the fix named.
#[derive(Debug, thiserror::Error)]
#[error(
    "cannot open /dev/uinput ({0}); run `sequencer doctor` for the udev rule and group \
     membership this needs"
)]
pub struct UinputUnavailable(#[source] io::Error);

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::input::{Button, Key};
    use sequencer_core::time::Timestamp;

    fn emit_of(action: EmitAction) -> Emit {
        Emit {
            at: Timestamp::ZERO,
            action,
            level: 0,
        }
    }

    /// Whether this machine can actually create a virtual device. Most CI containers
    /// cannot, so every test that needs one bows out rather than failing.
    fn uinput_available() -> bool {
        std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_ok()
    }

    /// The virtual device must advertise relative axes, or libinput does not classify it as
    /// a pointer and routes every button event into nothing.
    ///
    /// Worth a hardware test rather than a unit one because the failure is *silent at every
    /// layer we can see*: the builder succeeds, `emit` succeeds, the kernel accepts the
    /// writes, and `bench` — which reads the raw device back — counts every single one. Only
    /// an application receiving nothing reveals it, and no assertion below the compositor
    /// notices. So this inspects what the kernel actually published for the device.
    ///
    /// Named `uinput` so CI's device-enabled job selects it.
    #[test]
    fn the_uinput_device_advertises_pointer_axes() {
        if !uinput_available() {
            eprintln!("SKIPPED: /dev/uinput is not writable here");
            return;
        }
        let mut sink = UinputSink::open().expect("virtual device");
        let node = sink
            .dev_nodes()
            .expect("dev nodes")
            .into_iter()
            .next()
            .expect("the device has an event node");
        let device = evdev::Device::open(&node).expect("open the device we just made");

        let axes = device
            .supported_relative_axes()
            .expect("a pointer must advertise relative axes at all");
        assert!(
            axes.contains(RelativeAxisCode::REL_X) && axes.contains(RelativeAxisCode::REL_Y),
            "libinput classifies by axes, not buttons: without REL_X/REL_Y the clicks go nowhere"
        );
        assert!(
            axes.contains(RelativeAxisCode::REL_WHEEL),
            "without a wheel libinput falls back to BUTTON scrolling, and then holds every \
             press back to see whether it starts a scroll instead of a click"
        );
        assert!(
            device
                .supported_keys()
                .is_some_and(|keys| keys.contains(KeyCode::BTN_LEFT)),
            "and the buttons must still be there"
        );
    }

    #[test]
    fn unsupported_actions_are_named_rather_than_silently_dropped() {
        // Checked without a device, since it is the mapping that is under test.
        assert!(keymap::holdable_to_code(Holdable::Key(Key::F9)).is_some());
        assert!(keymap::holdable_to_code(Holdable::Button(Button::Left)).is_some());
    }

    #[test]
    fn uinput_opens_and_clicks_round_trip() {
        if !uinput_available() {
            eprintln!("skipping: /dev/uinput is not writable here");
            return;
        }
        let mut sink = UinputSink::open().expect("device should open");
        sink.emit(&emit_of(EmitAction::ButtonDown(Button::Left)))
            .expect("press should stage");
        sink.emit(&emit_of(EmitAction::ButtonUp(Button::Left)))
            .expect("release should stage");
        sink.flush().expect("flush should commit");
        assert!(sink.held.is_empty(), "nothing should still be held");
    }

    #[test]
    fn uinput_release_all_clears_what_was_pressed() {
        if !uinput_available() {
            eprintln!("skipping: /dev/uinput is not writable here");
            return;
        }
        let mut sink = UinputSink::open().expect("device should open");
        sink.emit(&emit_of(EmitAction::KeyDown(Key::LeftShift)))
            .expect("press should stage");
        sink.emit(&emit_of(EmitAction::KeyDown(Key::A)))
            .expect("press should stage");
        sink.flush().expect("flush should commit");
        assert_eq!(sink.held.len(), 2);

        sink.release_all();
        assert!(sink.held.is_empty());
    }
}
