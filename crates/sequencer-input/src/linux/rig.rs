//! The live suite's rig: a keyboard of its own to type on, and a watch on what sequencer's
//! virtual keyboard sends — so that the device backend can be driven and read back below
//! the display server, with nothing of the user's involved.
//!
//! The keyboard is made through `/dev/uinput`, which the kernel treats exactly as a physical
//! one, so a manager grabs it and routes its keys as it would the user's. It declares keys
//! and nothing else: with a mouse's buttons as well, udev would call it a mouse, and the
//! grab would leave it alone ([`crate::linux::grab::take`]). The watch grabs sequencer's
//! virtual keyboard in turn, so what that sends reaches the watch and never the session.
//!
//! Test machinery, behind the `live-tests` feature: only `sequencer-live-rig`, the live
//! suite's helper, uses it, run as root through the user's cached sudo ticket.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, Device, EventSummary, EventType, InputEvent, KeyCode};
use sequencer_core::input::Key;

use crate::linux::grab::KEYBOARD_NAME;
use crate::linux::keymap;

/// The name the rig's keyboard reports — what a test limits a manager's grab to
/// ([`crate::linux::grab::GRAB_ONLY_VARIABLE`]).
pub const TEST_KEYBOARD_NAME: &str = "sequencer test keyboard";

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
                "the test keyboard got no device node",
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
            crate::linux::grab::let_go(device);
        }
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
