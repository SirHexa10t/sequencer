//! The live suite's rig: a keyboard and a mouse of its own, and watches on what sequencer
//! passes on — its virtual keyboard, and a held mouse's stand-in — so that the device backend
//! can be driven and read back below the display server, with nothing of the user's involved.
//!
//! The devices are made through `/dev/uinput`, which the kernel treats exactly as physical
//! ones, so a manager grabs them and routes them as it would the user's. The keyboard
//! declares keys and nothing else: with a mouse's buttons as well, udev would call it a
//! mouse, and the grab would hand it to the mice ([`crate::linux::grab::take`]). The watches
//! grab what they watch in turn, so what sequencer passes on reaches them and never the
//! session — no key typed into the user's windows, no click or motion on their pointer.
//!
//! Test machinery, behind the `live-tests` feature: only `sequencer-live-rig`, the live
//! suite's helper, uses it, run as root through the user's cached sudo ticket.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AttributeSet, BusType, Device, EventSummary, EventType, InputEvent, InputId, KeyCode,
    RelativeAxisCode, SynchronizationCode,
};
use sequencer_core::input::{Button, Key};

use crate::linux::grab::{KEYBOARD_NAME, STAND_IN_PHYS};
use crate::linux::keymap;

/// How the names of the rig's devices begin — what a test limits a manager's grab to
/// ([`crate::linux::grab::GRAB_ONLY_VARIABLE`]).
pub const TEST_DEVICES: &str = "sequencer test ";

/// The name the rig's keyboard reports.
pub const TEST_KEYBOARD_NAME: &str = "sequencer test keyboard";

/// The name the rig's mouse reports — and its stand-in, which copies it.
pub const TEST_MOUSE_NAME: &str = "sequencer test mouse";

/// The buttons the rig's mouse has: every one a profile names.
pub const TEST_MOUSE_BUTTONS: [Button; 5] = [
    Button::Left,
    Button::Right,
    Button::Middle,
    Button::Back,
    Button::Forward,
];

/// The rig mouse's IDs: on the USB bus, as the stand-in udev rule (`udev/` in the
/// repository) expects of the mice it serves; with vendor and product numbers that no entry
/// of udev's databases or libinput's quirks names (checked 2026-11-04), so nothing meant for
/// a real mouse applies to it; and not a stand-in's defaults, so that a stand-in copying
/// them shows.
fn test_mouse_id() -> InputId {
    InputId::new(BusType::BUS_USB, 0x5345, 0x5154, 1)
}

/// How long the kernel and udev may take to give a new device its node: a manager started
/// before then would never see the keyboard, since devices are opened once, at start.
const NODE_WITHIN: Duration = Duration::from_secs(5);

/// A keyboard to type on: every key sequencer names, and nothing else.
#[derive(Debug)]
pub struct TestKeyboard {
    device: VirtualDevice,
    /// Its node under `/dev/input`.
    node: PathBuf,
}

impl TestKeyboard {
    /// Makes the keyboard, and waits for its device node.
    ///
    /// # Errors
    ///
    /// `/dev/uinput` can't be written — the rig runs as root, so a missing `uinput` module,
    /// most likely — or the node never appeared.
    pub fn open() -> io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        // Keys only: a button among them would make the keyboard a mouse to udev.
        for code in keymap::all_codes().filter(|code| keymap::code_to_key(*code).is_some()) {
            keys.insert(code);
        }
        let mut device = VirtualDevice::builder()
            .and_then(|builder| builder.name(TEST_KEYBOARD_NAME).with_keys(&keys))
            .and_then(VirtualDeviceBuilder::build)?;
        let node = node_of(&mut device)?;
        Ok(Self { device, node })
    }

    /// Sends `key` going down (`value` 1), coming up (0), or repeating as a held key does
    /// (2), as one frame.
    ///
    /// # Errors
    ///
    /// `key` has no evdev code, or the write failed.
    pub fn send(&mut self, key: Key, value: i32) -> io::Result<()> {
        let code = keymap::key_to_code(key).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{key} has no key code"),
            )
        })?;
        // `emit` closes the frame with its own SYN_REPORT.
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code.0, value)])
    }

    /// Its node under `/dev/input`, for opening it as a manager would.
    #[must_use]
    pub fn node(&self) -> &Path {
        &self.node
    }

    /// Whether something holds this keyboard exclusively — a supervisor's grab — asked by
    /// trying to grab it: `false` once the grab is let go.
    ///
    /// # Errors
    ///
    /// The node could not be opened, or the grab failed for another reason than its being
    /// held.
    pub fn is_held(&self) -> io::Result<bool> {
        crate::linux::grab::is_grabbed(&self.node)
    }
}

/// A mouse to click and move: the buttons a profile names, two axes and a wheel — what udev
/// and the grab call a mouse.
#[derive(Debug)]
pub struct TestMouse {
    device: VirtualDevice,
    /// Its node under `/dev/input`.
    node: PathBuf,
}

impl TestMouse {
    /// Makes the mouse, and waits for its device node.
    ///
    /// # Errors
    ///
    /// `/dev/uinput` can't be written, or the node never appeared.
    pub fn open() -> io::Result<Self> {
        let mut buttons = AttributeSet::<KeyCode>::new();
        for code in TEST_MOUSE_BUTTONS
            .into_iter()
            .filter_map(keymap::button_to_code)
        {
            buttons.insert(code);
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        for axis in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
        ] {
            axes.insert(axis);
        }
        let mut device = VirtualDevice::builder()?
            .name(TEST_MOUSE_NAME)
            .input_id(test_mouse_id())
            .with_keys(&buttons)?
            .with_relative_axes(&axes)?
            .build()?;
        let node = node_of(&mut device)?;
        Ok(Self { device, node })
    }

    /// Sends `button` going down (`value` 1) or coming up (0), as one frame.
    ///
    /// # Errors
    ///
    /// `button` has no evdev code, or the write failed.
    pub fn send(&mut self, button: Button, value: i32) -> io::Result<()> {
        let code = keymap::button_to_code(button).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the {button} button has no code"),
            )
        })?;
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code.0, value)])
    }

    /// Moves `across` to the right and `down`, as one frame.
    ///
    /// # Errors
    ///
    /// The write failed.
    pub fn move_by(&mut self, across: i32, down: i32) -> io::Result<()> {
        self.device.emit(&[
            InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, across),
            InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, down),
        ])
    }

    /// Whether something holds this mouse exclusively — a supervisor's grab — asked by
    /// trying to grab it: `false` once the grab is let go.
    ///
    /// # Errors
    ///
    /// The node could not be opened, or the grab failed for another reason than its being
    /// held.
    pub fn is_held(&self) -> io::Result<bool> {
        crate::linux::grab::is_grabbed(&self.node)
    }
}

/// `device`'s node under `/dev/input`, once it is there to open.
fn node_of(device: &mut VirtualDevice) -> io::Result<PathBuf> {
    let deadline = Instant::now() + NODE_WITHIN;
    loop {
        if let Some(node) = device
            .enumerate_dev_nodes_blocking()?
            .find_map(Result::ok)
            .filter(|node| node.exists())
        {
            return Ok(node);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the rig's device got no device node",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What sequencer's virtual keyboard sends, held from the session: grabbed, so that nothing
/// it sends reaches anything but the watch.
#[derive(Debug)]
pub struct Watch {
    /// Taken only as the watch drops, to be let go quietly: the virtual keyboard is gone by
    /// then, as often as not.
    device: Option<Device>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            let_go(device);
        }
    }
}

/// Ends the grab `device` holds, and closes it — quietly when the device has gone: evdev,
/// dropping a handle whose grab it couldn't end, would print that it failed, though an
/// unplugged device holds no grab. Such a handle is left open instead; it is a closing
/// process's, and goes with it.
fn let_go(mut device: Device) {
    if device.ungrab().is_err() {
        std::mem::forget(device);
    }
}

impl Watch {
    /// Grabs the virtual keyboard of the manager on the devices; `Ok(None)` while there is
    /// none to grab.
    ///
    /// # Errors
    ///
    /// It is there but could not be opened or grabbed.
    pub fn open() -> io::Result<Option<Self>> {
        let mut found: Vec<_> = crate::devices::list()
            .into_iter()
            .filter(|device| device.name == KEYBOARD_NAME)
            .collect();
        // With two, the one watched might not be the test's — and then the test's output
        // would go on to the session unheard.
        if found.len() > 1 {
            return Err(io::Error::other(format!(
                "{} virtual keyboards, so which is the test's? A manager from an earlier run is still alive",
                found.len()
            )));
        }
        let Some(found) = found.pop() else {
            return Ok(None);
        };
        let mut device = Device::open(found.path())?;
        device.grab()?;
        Ok(Some(Self {
            device: Some(device),
        }))
    }

    /// The key events sent since the last read, as each key and its value — waiting for at
    /// least one. A code sequencer has no name for is passed over.
    ///
    /// # Errors
    ///
    /// The read failed: the virtual keyboard went away with its manager.
    pub fn next_keys(&mut self) -> io::Result<Vec<(Key, i32)>> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| io::Error::other("the watch is over"))?;
        Ok(device
            .fetch_events()?
            .filter_map(|event| match event.destructure() {
                EventSummary::Key(_, code, value) => {
                    keymap::code_to_key(code).map(|key| (key, value))
                }
                _ => None,
            })
            .collect())
    }
}

/// What the rig mouse's stand-in passes on, held from the session as [`Watch`] holds the
/// virtual keyboard: grabbed, so that nothing the rig's mouse does moves the user's pointer
/// or clicks anywhere.
#[derive(Debug)]
pub struct StandInWatch {
    /// Taken only as the watch drops, to be let go quietly: the stand-in is gone by then, as
    /// often as not.
    device: Option<Device>,
}

impl Drop for StandInWatch {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            let_go(device);
        }
    }
}

/// One thing a stand-in passed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassedOn {
    /// A button going down (1) or coming up (0).
    Button(Button, i32),
    /// One frame's motion: its steps to the right and down.
    Moved(i32, i32),
}

impl StandInWatch {
    /// Grabs the stand-in for `mouse` once it is there — within the time a new device takes
    /// to get its node — and is `mouse` to the life: its name, its IDs, its buttons and axes.
    /// `Ok(None)` when none came.
    ///
    /// # Errors
    ///
    /// It came but differs from `mouse`, or could not be opened or grabbed.
    pub fn open(mouse: &TestMouse) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + NODE_WITHIN;
        let found = loop {
            let mut found: Vec<_> = crate::devices::list()
                .into_iter()
                .filter(|device| {
                    device.name == TEST_MOUSE_NAME
                        && STAND_IN_PHYS.to_str().is_ok_and(|phys| device.phys == phys)
                })
                .collect();
            if found.len() > 1 {
                return Err(io::Error::other(format!(
                    "{} stand-ins for the rig's mouse, so which is the test's? A manager from an earlier run is still alive",
                    found.len()
                )));
            }
            if let Some(found) = found.pop() {
                break found;
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut device = Device::open(found.path())?;
        let original = Device::open(&mouse.node)?;
        let differs = |what: &str| io::Error::other(format!("the stand-in's {what} differs"));
        if device.name() != original.name() {
            return Err(differs("name"));
        }
        if device.input_id() != original.input_id() {
            return Err(differs("IDs"));
        }
        let codes = |device: &Device| {
            device
                .supported_keys()
                .map(|keys| keys.iter().collect::<Vec<_>>())
        };
        if codes(&device) != codes(&original) {
            return Err(differs("buttons"));
        }
        let axes = |device: &Device| {
            device
                .supported_relative_axes()
                .map(|axes| axes.iter().collect::<Vec<_>>())
        };
        if axes(&device) != axes(&original) {
            return Err(differs("axes"));
        }
        device.grab()?;
        Ok(Some(Self {
            device: Some(device),
        }))
    }

    /// What the stand-in passed on since the last read — waiting for at least one event —
    /// each frame's motion summed.
    ///
    /// # Errors
    ///
    /// The read failed: the stand-in went away with the mouse's grab.
    pub fn next_passed(&mut self) -> io::Result<Vec<PassedOn>> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| io::Error::other("the watch is over"))?;
        let mut passed = Vec::new();
        let (mut across, mut down) = (0, 0);
        for event in device.fetch_events()? {
            match event.destructure() {
                EventSummary::Key(_, code, value) => {
                    if let Some(button) = keymap::code_to_button(code) {
                        passed.push(PassedOn::Button(button, value));
                    }
                }
                EventSummary::RelativeAxis(_, RelativeAxisCode::REL_X, value) => across += value,
                EventSummary::RelativeAxis(_, RelativeAxisCode::REL_Y, value) => down += value,
                EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _)
                    if (across, down) != (0, 0) =>
                {
                    passed.push(PassedOn::Moved(across, down));
                    (across, down) = (0, 0);
                }
                _ => {}
            }
        }
        Ok(passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test manager admits the rig's devices by how their names begin: every one of them
    /// must begin so, or a test's manager would leave it alone.
    #[test]
    fn every_rig_device_is_admitted_by_the_tests_limit() {
        assert!(TEST_KEYBOARD_NAME.starts_with(TEST_DEVICES));
        assert!(TEST_MOUSE_NAME.starts_with(TEST_DEVICES));
    }
}
