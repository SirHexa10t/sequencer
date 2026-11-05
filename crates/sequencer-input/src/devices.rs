//! What kind of input device something is — a keyboard, a mouse, a touchpad, a game
//! controller — decided by the rules udev decides it by, so that sequencer and the desktop
//! never disagree about a device.
//!
//! The desktop doesn't decide this itself. udev's `input_id` builtin reads each device's
//! capability bitmaps and tags it (`ID_INPUT_KEYBOARD`, `ID_INPUT_MOUSE`, …), and libinput —
//! under every Wayland compositor, and the X server's default driver — takes a device as what
//! its tags say, and ignores one they say nothing about. [`Capabilities::kinds`] is that
//! builtin's verdict, worked out from the same bitmaps by its rules, ported from systemd's
//! `src/udev/udev-builtin-input_id.c` (GPL-2.0-or-later). A port can't see udev's hardware
//! database, which corrects the verdict for a few models; `udevadm info` shows the tags a
//! device really has.
//!
//! The bitmaps come from sysfs ([`Capabilities::read`]), which describes every device whatever
//! its node allows — so a device this process can't open can still be named and judged — or
//! from an open device ([`Capabilities::of`]). The kernel keeps one copy of each, and both
//! read it.
//!
//! A hand-written rule decided this before, in the grab, and misjudged the first real
//! keyboards it met: the absolute axis a volume wheel adds made two keyboards touchpads, and a
//! mouse's 32 buttons, running on into the joystick buttons' codes, made it a controller.
//! udev's rules had an answer for both.

use std::collections::BTreeSet;
use std::ffi::c_ulong;
use std::fmt;
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};

/// Where the kernel lists input devices.
const SYSFS_INPUT: &str = "/sys/class/input";

/// Defines each kernel code named here, as `linux/input-event-codes.h` names it, and lists
/// them all beside the evdev crate's value for each, for a test to compare where that crate
/// is built: no code can be added unchecked.
macro_rules! codes {
    ($($(#[$attr:meta])* $name:ident = $value:literal => $evdev:expr;)*) => {
        $($(#[$attr])* const $name: u16 = $value;)*

        /// Every code above, beside the evdev crate's value for it.
        #[cfg(all(test, feature = "evdev", target_os = "linux"))]
        const AS_EVDEV_HAS_THEM: &[(&str, u16, u16)] = &[$((stringify!($name), $name, $evdev)),*];
    };
}

codes! {
    EV_KEY = 0x01 => evdev::EventType::KEY.0;
    EV_REL = 0x02 => evdev::EventType::RELATIVE.0;
    EV_SW = 0x05 => evdev::EventType::SWITCH.0;

    REL_X = 0x00 => evdev::RelativeAxisCode::REL_X.0;
    REL_Y = 0x01 => evdev::RelativeAxisCode::REL_Y.0;
    REL_HWHEEL = 0x06 => evdev::RelativeAxisCode::REL_HWHEEL.0;
    REL_WHEEL = 0x08 => evdev::RelativeAxisCode::REL_WHEEL.0;

    ABS_X = 0x00 => evdev::AbsoluteAxisCode::ABS_X.0;
    ABS_Y = 0x01 => evdev::AbsoluteAxisCode::ABS_Y.0;
    ABS_Z = 0x02 => evdev::AbsoluteAxisCode::ABS_Z.0;
    ABS_RX = 0x03 => evdev::AbsoluteAxisCode::ABS_RX.0;
    ABS_PRESSURE = 0x18 => evdev::AbsoluteAxisCode::ABS_PRESSURE.0;
    ABS_MT_SLOT = 0x2f => evdev::AbsoluteAxisCode::ABS_MT_SLOT.0;
    ABS_MT_POSITION_X = 0x35 => evdev::AbsoluteAxisCode::ABS_MT_POSITION_X.0;
    ABS_MT_POSITION_Y = 0x36 => evdev::AbsoluteAxisCode::ABS_MT_POSITION_Y.0;

    INPUT_PROP_DIRECT = 0x01 => evdev::PropType::DIRECT.0;
    INPUT_PROP_POINTING_STICK = 0x05 => evdev::PropType::POINTING_STICK.0;
    INPUT_PROP_ACCELEROMETER = 0x06 => evdev::PropType::ACCELEROMETER.0;

    KEY_ESC = 1 => evdev::KeyCode::KEY_ESC.0;
    KEY_LEFTCTRL = 29 => evdev::KeyCode::KEY_LEFTCTRL.0;
    KEY_S = 31 => evdev::KeyCode::KEY_S.0;
    KEY_CAPSLOCK = 58 => evdev::KeyCode::KEY_CAPSLOCK.0;
    KEY_NUMLOCK = 69 => evdev::KeyCode::KEY_NUMLOCK.0;
    KEY_INSERT = 110 => evdev::KeyCode::KEY_INSERT.0;
    KEY_MUTE = 113 => evdev::KeyCode::KEY_MUTE.0;
    KEY_CALC = 140 => evdev::KeyCode::KEY_CALC.0;
    KEY_FILE = 144 => evdev::KeyCode::KEY_FILE.0;
    KEY_MAIL = 155 => evdev::KeyCode::KEY_MAIL.0;
    KEY_PLAYPAUSE = 164 => evdev::KeyCode::KEY_PLAYPAUSE.0;
    KEY_BRIGHTNESSDOWN = 224 => evdev::KeyCode::KEY_BRIGHTNESSDOWN.0;
    // BTN_MISC, BTN_MOUSE, BTN_JOYSTICK, BTN_DIGI and BTN_TRIGGER_HAPPY name where a range of
    // buttons begins; the evdev crate knows each by its first button's name.
    BTN_MISC = 0x100 => evdev::KeyCode::BTN_0.0;
    BTN_0 = 0x100 => evdev::KeyCode::BTN_0.0;
    BTN_1 = 0x101 => evdev::KeyCode::BTN_1.0;
    BTN_MOUSE = 0x110 => evdev::KeyCode::BTN_LEFT.0;
    BTN_JOYSTICK = 0x120 => evdev::KeyCode::BTN_TRIGGER.0;
    BTN_DIGI = 0x140 => evdev::KeyCode::BTN_TOOL_PEN.0;
    BTN_TOOL_PEN = 0x140 => evdev::KeyCode::BTN_TOOL_PEN.0;
    BTN_TOOL_FINGER = 0x145 => evdev::KeyCode::BTN_TOOL_FINGER.0;
    BTN_TOUCH = 0x14a => evdev::KeyCode::BTN_TOUCH.0;
    BTN_STYLUS = 0x14b => evdev::KeyCode::BTN_STYLUS.0;
    KEY_OK = 0x160 => evdev::KeyCode::KEY_OK.0;
    BTN_DPAD_UP = 0x220 => evdev::KeyCode::BTN_DPAD_UP.0;
    BTN_DPAD_RIGHT = 0x223 => evdev::KeyCode::BTN_DPAD_RIGHT.0;
    KEY_ALS_TOGGLE = 0x230 => evdev::KeyCode::KEY_ALS_TOGGLE.0;
    BTN_TRIGGER_HAPPY = 0x2c0 => evdev::KeyCode::BTN_TRIGGER_HAPPY1.0;
    BTN_TRIGGER_HAPPY1 = 0x2c0 => evdev::KeyCode::BTN_TRIGGER_HAPPY1.0;
    BTN_TRIGGER_HAPPY40 = 0x2e7 => evdev::KeyCode::BTN_TRIGGER_HAPPY40.0;

    BUS_I2C = 0x18 => evdev::BusType::BUS_I2C.0;

    // Only the tests describe devices with these. udev reads the absolute axes themselves,
    // never asking whether the device sends absolute events at all.
    #[cfg(test)]
    EV_ABS = 0x03 => evdev::EventType::ABSOLUTE.0;
    #[cfg(test)]
    ABS_RY = 0x04 => evdev::AbsoluteAxisCode::ABS_RY.0;
    #[cfg(test)]
    ABS_HAT0X = 0x10 => evdev::AbsoluteAxisCode::ABS_HAT0X.0;
    #[cfg(test)]
    ABS_HAT0Y = 0x11 => evdev::AbsoluteAxisCode::ABS_HAT0Y.0;
    #[cfg(test)]
    ABS_VOLUME = 0x20 => evdev::AbsoluteAxisCode::ABS_VOLUME.0;
    #[cfg(test)]
    ABS_MISC = 0x28 => evdev::AbsoluteAxisCode::ABS_MISC.0;
    #[cfg(test)]
    INPUT_PROP_POINTER = 0x00 => evdev::PropType::POINTER.0;
    #[cfg(test)]
    KEY_VOLUMEUP = 115 => evdev::KeyCode::KEY_VOLUMEUP.0;
    #[cfg(test)]
    BTN_2 = 0x102 => evdev::KeyCode::BTN_2.0;
    #[cfg(test)]
    BTN_RIGHT = 0x111 => evdev::KeyCode::BTN_RIGHT.0;
    #[cfg(test)]
    BTN_MIDDLE = 0x112 => evdev::KeyCode::BTN_MIDDLE.0;
    #[cfg(test)]
    BTN_SOUTH = 0x130 => evdev::KeyCode::BTN_SOUTH.0;
    #[cfg(test)]
    BTN_EAST = 0x131 => evdev::KeyCode::BTN_EAST.0;
    #[cfg(test)]
    BTN_NORTH = 0x133 => evdev::KeyCode::BTN_NORTH.0;
    #[cfg(test)]
    BTN_WEST = 0x134 => evdev::KeyCode::BTN_WEST.0;
    #[cfg(test)]
    BTN_START = 0x13b => evdev::KeyCode::BTN_START.0;
}

/// Keys from as many separate groups as a keyboard has and a joystick, at most, one of: four
/// of them make a device with joystick buttons a keyboard with stray ones. udev's sample, not
/// a definition — libinput keeps the same.
const KEYBOARD_GROUP_KEYS: [u16; 10] = [
    KEY_LEFTCTRL,
    KEY_CAPSLOCK,
    KEY_NUMLOCK,
    KEY_INSERT,
    KEY_MUTE,
    KEY_CALC,
    KEY_FILE,
    KEY_MAIL,
    KEY_PLAYPAUSE,
    KEY_BRIGHTNESSDOWN,
];

/// What udev calls an input device: one of the `ID_INPUT_*` tags its `input_id` builtin sets.
/// A device can be several — a keyboard is [`Kind::Key`] too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// It sends keys: a keyboard, a media remote, a power button.
    Key,
    /// A full keyboard.
    Keyboard,
    /// A mouse — or another pointer with mouse buttons.
    Mouse,
    /// A pointing stick, such as a TrackPoint.
    PointingStick,
    /// A touchpad.
    Touchpad,
    /// A touchscreen.
    Touchscreen,
    /// A drawing tablet.
    Tablet,
    /// A drawing tablet's buttons, rings and strips.
    TabletPad,
    /// A joystick or game controller.
    Joystick,
    /// A motion sensor.
    Accelerometer,
    /// It reports switches: a lid, tablet mode, a headphone jack.
    Switch,
}

impl Kind {
    /// The udev property naming this kind, as `udevadm info` shows it.
    #[must_use]
    pub const fn udev_tag(self) -> &'static str {
        match self {
            Self::Key => "ID_INPUT_KEY",
            Self::Keyboard => "ID_INPUT_KEYBOARD",
            Self::Mouse => "ID_INPUT_MOUSE",
            Self::PointingStick => "ID_INPUT_POINTINGSTICK",
            Self::Touchpad => "ID_INPUT_TOUCHPAD",
            Self::Touchscreen => "ID_INPUT_TOUCHSCREEN",
            Self::Tablet => "ID_INPUT_TABLET",
            Self::TabletPad => "ID_INPUT_TABLET_PAD",
            Self::Joystick => "ID_INPUT_JOYSTICK",
            Self::Accelerometer => "ID_INPUT_ACCELEROMETER",
            Self::Switch => "ID_INPUT_SWITCH",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Key => "keys",
            Self::Keyboard => "a keyboard",
            Self::Mouse => "a mouse",
            Self::PointingStick => "a pointing stick",
            Self::Touchpad => "a touchpad",
            Self::Touchscreen => "a touchscreen",
            Self::Tablet => "a drawing tablet",
            Self::TabletPad => "a drawing tablet's pad",
            Self::Joystick => "a game controller",
            Self::Accelerometer => "a motion sensor",
            Self::Switch => "switches",
        })
    }
}

/// The codes of one type a device declares: one of its capability bitmaps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Bits(BTreeSet<u16>);

impl Bits {
    /// A bitmap as sysfs prints it: hexadecimal words, most significant first, leading zero
    /// words left out (the kernel's `input_print_bitmap`). A word is an `unsigned long` of the
    /// process reading it — a 64-bit kernel prints a 32-bit process its words in halves — so
    /// each holds `c_ulong::BITS` codes. A word that won't parse holds none but keeps its
    /// place, as udev reads it; an unreadable file's empty text holds none at all.
    fn parse(text: &str) -> Self {
        let mut codes = BTreeSet::new();
        for (index, word) in text.split_whitespace().rev().enumerate() {
            let Ok(word) = u64::from_str_radix(word, 16) else {
                continue;
            };
            let Some(first) = u32::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(c_ulong::BITS))
            else {
                break;
            };
            for bit in (0..c_ulong::BITS).filter(|bit| (word >> bit) & 1 == 1) {
                if let Some(code) = first
                    .checked_add(bit)
                    .and_then(|code| u16::try_from(code).ok())
                {
                    codes.insert(code);
                }
            }
        }
        Self(codes)
    }

    fn has(&self, code: u16) -> bool {
        self.0.contains(&code)
    }

    fn any_in(&self, codes: impl RangeBounds<u16>) -> bool {
        self.0.range(codes).next().is_some()
    }

    fn count_in(&self, codes: impl RangeBounds<u16>) -> usize {
        self.0.range(codes).count()
    }
}

impl FromIterator<u16> for Bits {
    fn from_iter<I: IntoIterator<Item = u16>>(codes: I) -> Self {
        Self(codes.into_iter().collect())
    }
}

/// What a device declares it can send: its event types, the codes of each type udev's rules
/// read, its properties, and the bus it is on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    events: Bits,
    keys: Bits,
    relative: Bits,
    absolute: Bits,
    properties: Bits,
    bus: u16,
}

impl Capabilities {
    /// What the device whose sysfs directory is `device` declares
    /// (`/sys/class/input/event<N>/device`). An attribute that can't be read declares nothing,
    /// as udev takes it.
    #[must_use]
    pub fn read(device: &Path) -> Self {
        Self::parse(|attribute| std::fs::read_to_string(device.join(attribute)).ok())
    }

    /// [`Self::read`], given each attribute's text by its sysfs name.
    fn parse(attribute: impl Fn(&str) -> Option<String>) -> Self {
        let bits = |name: &str| Bits::parse(&attribute(name).unwrap_or_default());
        Self {
            events: bits("capabilities/ev"),
            keys: bits("capabilities/key"),
            relative: bits("capabilities/rel"),
            absolute: bits("capabilities/abs"),
            properties: bits("properties"),
            bus: attribute("id/bustype")
                .and_then(|text| u16::from_str_radix(text.trim(), 16).ok())
                .unwrap_or_default(),
        }
    }

    /// What `device` declares, asked of the open device.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[must_use]
    pub fn of(device: &evdev::Device) -> Self {
        Self {
            events: device
                .supported_events()
                .iter()
                .map(|kind| kind.0)
                .collect(),
            keys: device
                .supported_keys()
                .map(|keys| keys.iter().map(|key| key.0).collect())
                .unwrap_or_default(),
            relative: device
                .supported_relative_axes()
                .map(|axes| axes.iter().map(|axis| axis.0).collect())
                .unwrap_or_default(),
            absolute: device
                .supported_absolute_axes()
                .map(|axes| axes.iter().map(|axis| axis.0).collect())
                .unwrap_or_default(),
            properties: device
                .properties()
                .iter()
                .map(|property| property.0)
                .collect(),
            bus: device.input_id().bus_type().0,
        }
    }

    /// The key and button codes it declares, lowest first.
    pub fn key_codes(&self) -> impl Iterator<Item = u16> + '_ {
        self.keys.0.iter().copied()
    }

    /// Whether it is a full keyboard ([`Kind::Keyboard`]).
    #[must_use]
    pub fn is_keyboard(&self) -> bool {
        self.kinds().contains(&Kind::Keyboard)
    }

    /// What udev calls it: the kinds its `input_id` builtin tags it with. Empty when udev gives
    /// it none — and libinput, given none, ignores it.
    #[must_use]
    pub fn kinds(&self) -> BTreeSet<Kind> {
        let mut kinds = self.pointer_kinds();
        let pointer = !kinds.is_empty();
        let keys = self.key_kinds();
        // A node with nothing but a scroll wheel counts as keys.
        if !pointer && keys.is_empty() && self.wheel() {
            kinds.insert(Kind::Key);
        }
        kinds.extend(keys);
        if self.events.has(EV_SW) {
            kinds.insert(Kind::Switch);
        }
        kinds
    }

    /// udev's pointer rules (`test_pointers`): motion sensors, drawing tablets and their pads,
    /// touchpads, touchscreens, mice, pointing sticks and joysticks.
    fn pointer_kinds(&self) -> BTreeSet<Kind> {
        let (keys, absolute, properties) = (&self.keys, &self.absolute, &self.properties);
        let has_keys = self.events.has(EV_KEY);
        let positioned = absolute.has(ABS_X) && absolute.has(ABS_Y);
        // Moving in three dimensions with no keys to press is a motion sensor, whatever else.
        if properties.has(INPUT_PROP_ACCELEROMETER)
            || (!has_keys && positioned && absolute.has(ABS_Z))
        {
            return BTreeSet::from([Kind::Accelerometer]);
        }
        let pen = keys.has(BTN_STYLUS) || keys.has(BTN_TOOL_PEN);
        let finger_not_pen = keys.has(BTN_TOOL_FINGER) && !keys.has(BTN_TOOL_PEN);
        let mouse_buttons = keys.any_in(BTN_MOUSE..BTN_JOYSTICK);
        let moves = self.events.has(EV_REL) && self.relative.has(REL_X) && self.relative.has(REL_Y);
        // Declaring every absolute axis up to the slot's declares a multitouch it hasn't got.
        let multitouch = absolute.has(ABS_MT_POSITION_X)
            && absolute.has(ABS_MT_POSITION_Y)
            && !(absolute.has(ABS_MT_SLOT) && absolute.has(ABS_MT_SLOT - 1));
        let direct = properties.has(INPUT_PROP_DIRECT);
        let touch = keys.has(BTN_TOUCH);
        let pad_buttons = keys.has(BTN_0) && keys.has(BTN_1) && !keys.has(BTN_TOOL_PEN);
        let wheel = self.wheel();
        let joystick_controls = self.joystick_controls();

        let (mut tablet, mut touchpad, mut touchscreen, mut joystick, mut absolute_mouse) =
            (false, false, false, false, false);
        if positioned {
            if pen {
                tablet = true;
            } else if finger_not_pen && !direct {
                touchpad = true;
            } else if mouse_buttons {
                // A mouse that reports where it is: a virtual machine's.
                absolute_mouse = true;
            } else if touch || direct {
                touchscreen = true;
            } else if joystick_controls > 0 {
                joystick = true;
            }
        } else if joystick_controls > 0 {
            joystick = true;
        }
        if multitouch {
            if pen {
                tablet = true;
            } else if finger_not_pen && !direct {
                touchpad = true;
            } else if touch || direct {
                touchscreen = true;
            }
        }
        let mut tablet_pad = tablet && pad_buttons;
        if pad_buttons && wheel && !moves {
            tablet = true;
            tablet_pad = true;
        }
        // Asked before the joystick verdict is reconsidered below, as udev asks it.
        let mouse = !tablet && !touchpad && !joystick && mouse_buttons && (moves || !positioned);
        // There are no I2C mice: one is the stick in a laptop's keyboard.
        let pointing_stick =
            properties.has(INPUT_PROP_POINTING_STICK) || (mouse && self.bus == BUS_I2C);
        if joystick {
            let keyboard_groups = if has_keys {
                KEYBOARD_GROUP_KEYS
                    .iter()
                    .filter(|key| keys.has(**key))
                    .count()
            } else {
                0
            };
            // A keyboard with stray joystick buttons, a lone joystick control, or a tablet
            // pad's wheel and buttons: none of them is a joystick.
            if keyboard_groups >= 4 || joystick_controls < 2 || (wheel && pad_buttons) {
                joystick = false;
            }
        }

        [
            (pointing_stick, Kind::PointingStick),
            (mouse || absolute_mouse, Kind::Mouse),
            (touchpad, Kind::Touchpad),
            (touchscreen, Kind::Touchscreen),
            (joystick, Kind::Joystick),
            (tablet, Kind::Tablet),
            (tablet_pad, Kind::TabletPad),
        ]
        .into_iter()
        .filter_map(|(is, kind)| is.then_some(kind))
        .collect()
    }

    /// How many joystick buttons and axes it declares. A mouse with more than 16 buttons runs
    /// on into the joystick buttons' codes, so with the last mouse button declared, no
    /// joystick button counts.
    fn joystick_controls(&self) -> usize {
        let keys = &self.keys;
        let buttons = if keys.has(BTN_JOYSTICK - 1) {
            0
        } else {
            keys.count_in(BTN_JOYSTICK..BTN_DIGI)
                + keys.count_in(BTN_TRIGGER_HAPPY1..=BTN_TRIGGER_HAPPY40)
                + keys.count_in(BTN_DPAD_UP..=BTN_DPAD_RIGHT)
        };
        buttons + self.absolute.count_in(ABS_RX..ABS_PRESSURE)
    }

    /// udev's key rules (`test_key`): whether it sends keys, and whether they make a full
    /// keyboard.
    fn key_kinds(&self) -> BTreeSet<Kind> {
        let mut kinds = BTreeSet::new();
        if !self.events.has(EV_KEY) {
            return kinds;
        }
        let keys = &self.keys;
        // Keys proper, not buttons: below the buttons' codes, or in the two blocks of keys
        // numbered above them.
        if keys.any_in(..BTN_MISC)
            || keys.any_in(KEY_OK..BTN_DPAD_UP)
            || keys.any_in(KEY_ALS_TOGGLE..BTN_TRIGGER_HAPPY)
        {
            kinds.insert(Kind::Key);
        }
        // Escape, the digit row, Q to P, A and S: every keyboard has them all, and nothing
        // else does.
        if (KEY_ESC..=KEY_S).all(|code| keys.has(code)) {
            kinds.insert(Kind::Keyboard);
        }
        kinds
    }

    /// Whether it has a scroll wheel, either way.
    fn wheel(&self) -> bool {
        self.events.has(EV_REL) && (self.relative.has(REL_WHEEL) || self.relative.has(REL_HWHEEL))
    }
}

/// Describing devices in tests: each part sets its event type, as the kernel does.
#[cfg(test)]
impl Capabilities {
    pub(crate) fn with_keys(mut self, keys: impl IntoIterator<Item = u16>) -> Self {
        self.events.0.insert(EV_KEY);
        self.keys.0.extend(keys);
        self
    }

    pub(crate) fn with_relative(mut self, axes: impl IntoIterator<Item = u16>) -> Self {
        self.events.0.insert(EV_REL);
        self.relative.0.extend(axes);
        self
    }

    pub(crate) fn with_absolute(mut self, axes: impl IntoIterator<Item = u16>) -> Self {
        self.events.0.insert(EV_ABS);
        self.absolute.0.extend(axes);
        self
    }

    pub(crate) fn with_properties(mut self, properties: impl IntoIterator<Item = u16>) -> Self {
        self.properties.0.extend(properties);
        self
    }

    pub(crate) fn with_switches(mut self) -> Self {
        self.events.0.insert(EV_SW);
        self
    }

    pub(crate) const fn on_bus(mut self, bus: u16) -> Self {
        self.bus = bus;
        self
    }

    /// Whether it declares the relative axis `axis` — what a test of a device's published
    /// shape asks beyond its kinds (the uinput one, which only the device backend builds).
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    pub(crate) fn declares_relative(&self, axis: u16) -> bool {
        self.relative.has(axis)
    }
}

/// An input device, as sysfs describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    /// `event<N>`: its node under `/dev/input`.
    pub node: String,
    /// The name it reports.
    pub name: String,
    /// Where it says it is attached (`phys`): a USB path for most, empty for many virtual
    /// devices — and sequencer's own mark on its stand-ins.
    pub phys: String,
    /// What it declares.
    pub capabilities: Capabilities,
}

impl InputDevice {
    /// Its device node.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        Path::new("/dev/input").join(&self.node)
    }
}

/// Every event device the kernel lists, in its order — `event2` before `event10`. Empty where
/// there is no sysfs to read.
#[must_use]
pub fn list() -> Vec<InputDevice> {
    let Ok(entries) = std::fs::read_dir(SYSFS_INPUT) else {
        return Vec::new();
    };
    let mut devices: Vec<InputDevice> = entries
        .flatten()
        .filter_map(|entry| {
            let node = entry.file_name().to_str()?.to_owned();
            if !node.starts_with("event") {
                return None;
            }
            let device = entry.path().join("device");
            let name = std::fs::read_to_string(device.join("name"))
                .map_or_else(|_| node.clone(), |name| name.trim().to_owned());
            let phys = std::fs::read_to_string(device.join("phys"))
                .map(|phys| phys.trim().to_owned())
                .unwrap_or_default();
            Some(InputDevice {
                capabilities: Capabilities::read(&device),
                name,
                phys,
                node,
            })
        })
        .collect();
    devices.sort_by_key(|device| node_number(&device.node));
    devices
}

/// `event<N>`'s number, so that a list follows the kernel's order rather than the
/// directory's: `event10` after `event9`, not after `event1`.
pub(crate) fn node_number(node: &str) -> u32 {
    node.trim_start_matches("event").parse().unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What sysfs prints for a device: each attribute's name and text.
    type Sysfs<'a> = &'a [(&'a str, &'a str)];

    /// A device as sysfs describes it: each attribute's text, as the kernel prints it.
    fn sysfs(attributes: Sysfs<'_>) -> Capabilities {
        Capabilities::parse(|name| {
            attributes
                .iter()
                .find(|(attribute, _)| *attribute == name)
                .map(|(_, text)| format!("{text}\n"))
        })
    }

    fn kinds(of: &[Kind]) -> BTreeSet<Kind> {
        of.iter().copied().collect()
    }

    /// Every key from Escape to Keypad . — the keys of any keyboard's main block.
    fn keyboard_keys() -> impl Iterator<Item = u16> {
        1..=83
    }

    /// What sysfs said of real devices, and what udev's rules make of each. The kernel's own
    /// text checks the bitmap reading, the codes and the rules together. It is text as a
    /// 64-bit process reads it — 64-bit words — so only such a process can check it.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn real_devices_are_what_udev_calls_them() {
        let full_keyboard = [
            ("capabilities/ev", "12001f"),
            (
                "capabilities/key",
                "3f00733fff 0 0 483ffff17aff32d bfd4444600000000 1 130ff38b17d007 \
                 ffe77bfad9415fff ffbeffdfffefffff fffffffffffffffe",
            ),
            ("capabilities/rel", "1040"),
            ("capabilities/abs", "100000000"),
            ("properties", "0"),
            ("id/bustype", "0003"),
        ];
        let cases: [(&str, Sysfs<'_>, &[Kind]); 10] = [
            (
                "a full-size keyboard whose media keys add a volume axis and a sideways wheel",
                &full_keyboard,
                &[Kind::Key, Kind::Keyboard],
            ),
            (
                "a mouse's keyboard interface, shaped the same",
                &[
                    ("capabilities/ev", "10001f"),
                    (
                        "capabilities/key",
                        "733eff 0 0 483ffff17aff32d bfd4444600000000 1 130ff38b17c007 \
                         ffe77bfad941dfff febeffdfffefffff fffffffffffffffe",
                    ),
                    ("capabilities/rel", "1040"),
                    ("capabilities/abs", "100000000"),
                ],
                &[Kind::Key, Kind::Keyboard],
            ),
            (
                "a mouse with 16 buttons",
                &[
                    ("capabilities/ev", "17"),
                    ("capabilities/key", "ffff0000 0 0 0 0"),
                    ("capabilities/rel", "1943"),
                ],
                &[Kind::Mouse],
            ),
            (
                "a mouse with 32 buttons, the last 16 on the joystick buttons' codes",
                &[
                    ("capabilities/ev", "17"),
                    ("capabilities/key", "ffffffff0000 0 0 0 0"),
                    ("capabilities/rel", "903"),
                ],
                &[Kind::Mouse],
            ),
            (
                "a power button",
                &[
                    ("capabilities/ev", "3"),
                    ("capabilities/key", "8000 10000000000000 0"),
                ],
                &[Kind::Key],
            ),
            (
                "a sound card's volume and mute keys",
                &[
                    ("capabilities/ev", "13"),
                    ("capabilities/key", "100000000000000 0 e000000000000 0"),
                ],
                &[Kind::Key],
            ),
            (
                "the brightness keys of a display",
                &[
                    ("capabilities/ev", "3"),
                    ("capabilities/key", "3e000b00000000 0 0 0"),
                ],
                &[Kind::Key],
            ),
            (
                "a vendor's control interface: one miscellaneous axis",
                &[
                    ("capabilities/ev", "9"),
                    ("capabilities/abs", "10000000000"),
                ],
                &[],
            ),
            ("a beeper", &[("capabilities/ev", "40001")], &[]),
            (
                "a headphone jack",
                &[("capabilities/ev", "21")],
                &[Kind::Switch],
            ),
        ];
        for (what, attributes, expected) in cases {
            assert_eq!(sysfs(attributes).kinds(), kinds(expected), "{what}");
        }
    }

    /// The volume axis that made a keyboard a touchpad to the old rule: an absolute axis that
    /// isn't a position says nothing about pointing.
    #[test]
    fn only_a_position_makes_an_absolute_device() {
        let keyboard = Capabilities::default()
            .with_keys(keyboard_keys())
            .with_relative([REL_HWHEEL])
            .with_absolute([ABS_VOLUME, ABS_MISC]);
        assert_eq!(keyboard.kinds(), kinds(&[Kind::Key, Kind::Keyboard]));
    }

    /// The pointing devices, each by what tells it from the others.
    #[test]
    fn pointers_are_told_apart() {
        let cases = [
            (
                "a touchpad: positions, a finger tool and no pen",
                Capabilities::default()
                    .with_keys([BTN_MOUSE, BTN_TOOL_FINGER, BTN_TOUCH])
                    .with_absolute([
                        ABS_X,
                        ABS_Y,
                        ABS_MT_SLOT,
                        ABS_MT_POSITION_X,
                        ABS_MT_POSITION_Y,
                    ])
                    .with_properties([INPUT_PROP_POINTER]),
                &[Kind::Touchpad][..],
            ),
            (
                "a touchscreen: positions, touched directly",
                Capabilities::default()
                    .with_keys([BTN_TOUCH])
                    .with_absolute([ABS_X, ABS_Y, ABS_MT_POSITION_X, ABS_MT_POSITION_Y])
                    .with_properties([INPUT_PROP_DIRECT]),
                &[Kind::Touchscreen],
            ),
            (
                "a multitouch-only touchscreen: the touch alone says so",
                Capabilities::default()
                    .with_keys([BTN_TOUCH])
                    .with_absolute([ABS_MT_POSITION_X, ABS_MT_POSITION_Y]),
                &[Kind::Touchscreen],
            ),
            (
                "a drawing tablet: positions and a pen",
                Capabilities::default()
                    .with_keys([BTN_TOOL_PEN, BTN_STYLUS, BTN_TOUCH])
                    .with_absolute([ABS_X, ABS_Y, ABS_PRESSURE]),
                &[Kind::Tablet],
            ),
            (
                "a tablet's pad, reporting through the tablet's positions",
                Capabilities::default()
                    .with_keys([BTN_0, BTN_1, BTN_2, BTN_STYLUS])
                    .with_absolute([ABS_X, ABS_Y]),
                &[Kind::Tablet, Kind::TabletPad],
            ),
            (
                "a tablet's pad with a wheel and no positions",
                Capabilities::default()
                    .with_keys([BTN_0, BTN_1])
                    .with_relative([REL_WHEEL]),
                &[Kind::Tablet, Kind::TabletPad],
            ),
            (
                "a mouse",
                Capabilities::default()
                    .with_keys([BTN_MOUSE, BTN_RIGHT, BTN_MIDDLE])
                    .with_relative([REL_X, REL_Y, REL_WHEEL]),
                &[Kind::Mouse],
            ),
            (
                "a mouse that reports positions: a virtual machine's",
                Capabilities::default()
                    .with_keys([BTN_MOUSE, BTN_RIGHT])
                    .with_absolute([ABS_X, ABS_Y]),
                &[Kind::Mouse],
            ),
            (
                "a pointing stick, by its property",
                Capabilities::default()
                    .with_keys([BTN_MOUSE, BTN_RIGHT, BTN_MIDDLE])
                    .with_relative([REL_X, REL_Y])
                    .with_properties([INPUT_PROP_POINTING_STICK]),
                &[Kind::Mouse, Kind::PointingStick],
            ),
            (
                "a pointing stick, by its bus: there are no I2C mice",
                Capabilities::default()
                    .with_keys([BTN_MOUSE, BTN_RIGHT])
                    .with_relative([REL_X, REL_Y])
                    .on_bus(BUS_I2C),
                &[Kind::Mouse, Kind::PointingStick],
            ),
            (
                "a motion sensor: three axes and nothing to press",
                Capabilities::default().with_absolute([ABS_X, ABS_Y, ABS_Z]),
                &[Kind::Accelerometer],
            ),
            (
                "a motion sensor by its property, keys or not",
                Capabilities::default()
                    .with_keys([BTN_0])
                    .with_absolute([ABS_X, ABS_Y])
                    .with_properties([INPUT_PROP_ACCELEROMETER]),
                &[Kind::Accelerometer],
            ),
        ];
        for (what, capabilities, expected) in cases {
            assert_eq!(capabilities.kinds(), kinds(expected), "{what}");
        }
    }

    /// Joysticks: by their buttons or their axes — and not every device with a joystick
    /// button is one.
    #[test]
    fn joysticks_are_told_from_what_merely_resembles_one() {
        let pad = Capabilities::default()
            .with_keys([BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_START])
            .with_absolute([ABS_X, ABS_Y, ABS_RX, ABS_RY, ABS_HAT0X, ABS_HAT0Y]);
        assert_eq!(pad.kinds(), kinds(&[Kind::Joystick]), "a game pad");
        let buttons_only = Capabilities::default().with_keys([BTN_SOUTH, BTN_EAST, BTN_START]);
        assert_eq!(
            buttons_only.kinds(),
            kinds(&[Kind::Joystick]),
            "a pad with no sticks"
        );
        // Two joystick buttons, so that only the key groups can outvote them.
        let stray = Capabilities::default().with_keys(keyboard_keys().chain([
            KEY_INSERT,
            BTN_JOYSTICK,
            BTN_JOYSTICK + 1,
        ]));
        assert_eq!(
            stray.kinds(),
            kinds(&[Kind::Key, Kind::Keyboard]),
            "a keyboard with a stray joystick button: four key groups outvote it"
        );
        let lone = Capabilities::default().with_keys([KEY_VOLUMEUP, BTN_JOYSTICK]);
        assert_eq!(
            lone.kinds(),
            kinds(&[Kind::Key]),
            "one joystick control is not a joystick"
        );
    }

    /// Keys, and what is not keys: buttons alone are not, a lone scroll wheel is, and a
    /// device claiming every axis up to the slot's has no multitouch.
    #[test]
    fn keys_are_keys_and_buttons_are_not() {
        let remote = Capabilities::default().with_keys([KEY_VOLUMEUP, KEY_MUTE, KEY_PLAYPAUSE]);
        assert_eq!(remote.kinds(), kinds(&[Kind::Key]), "a media remote");
        let pedal = Capabilities::default().with_keys([BTN_0, BTN_1, BTN_2]);
        assert_eq!(
            pedal.kinds(),
            kinds(&[]),
            "buttons and nothing else: udev gives it no kind"
        );
        let wheel = Capabilities::default().with_relative([REL_WHEEL]);
        assert_eq!(
            wheel.kinds(),
            kinds(&[Kind::Key]),
            "a node with only a wheel"
        );
        let lying = Capabilities::default()
            .with_keys([BTN_TOOL_FINGER])
            .with_absolute([
                ABS_MT_SLOT - 1,
                ABS_MT_SLOT,
                ABS_MT_POSITION_X,
                ABS_MT_POSITION_Y,
            ]);
        assert_eq!(
            lying.kinds(),
            kinds(&[]),
            "every axis declared is no multitouch"
        );
        let laptop_buttons = Capabilities::default()
            .with_keys([KEY_VOLUMEUP, KEY_MUTE])
            .with_switches();
        assert_eq!(
            laptop_buttons.kinds(),
            kinds(&[Kind::Key, Kind::Switch]),
            "keys and switches on one device"
        );
    }

    /// The keyboard rule — every code from Escape to S — asked of sysfs's text for an AT
    /// keyboard, a mouse, a power button and a game pad.
    #[test]
    fn only_a_full_key_set_makes_a_keyboard() {
        let keyboard =
            |key: &str| sysfs(&[("capabilities/ev", "3"), ("capabilities/key", key)]).is_keyboard();
        assert!(keyboard(
            "402000000 3803078f800d001 feffffdfffefffff fffffffffffffffe"
        ));
        assert!(!keyboard("ff0000 0 0 0 0"), "a mouse");
        assert!(!keyboard("10000000000000 0"), "a power button");
        assert!(!keyboard("7cdb000000000000 0 0 0 0"), "a game pad");
        assert!(!keyboard("0"), "no keys at all");
        assert!(!keyboard(""), "an unreadable sysfs file");
        assert!(
            keyboard("ffffffff fffffffe"),
            "a 32-bit word still holds codes 1-31"
        );
        assert!(!keyboard("7ffffffe"), "one short: KEY_S (31) missing");
        let no_key_events = sysfs(&[("capabilities/key", "fffffffffffffffe")]);
        assert!(
            !no_key_events.is_keyboard(),
            "keys a device doesn't say it sends are not asked about"
        );
    }

    /// Words are read most significant first, one `unsigned long` each; a word that won't
    /// parse keeps its place.
    #[test]
    fn bitmaps_read_as_the_kernel_prints_them() {
        let word = u16::try_from(c_ulong::BITS).unwrap();
        assert_eq!(Bits::parse("1 0"), [word].into_iter().collect());
        assert_eq!(Bits::parse("1 zz 1"), [0, 2 * word].into_iter().collect());
        assert_eq!(Bits::parse("0"), Bits::default());
        assert_eq!(Bits::parse(""), Bits::default());
        assert_eq!(Bits::parse("6\n"), [1, 2].into_iter().collect());
    }

    /// The bus is read in hexadecimal, as sysfs prints it; unreadable, it is none.
    #[test]
    fn the_bus_is_read_in_hexadecimal() {
        assert_eq!(sysfs(&[("id/bustype", "0018")]).bus, BUS_I2C);
        assert_eq!(sysfs(&[]).bus, 0);
    }

    /// Each udev tag is the one `udevadm info` shows, and each kind has its own words.
    #[test]
    fn each_kind_names_its_udev_tag() {
        assert_eq!(Kind::Keyboard.udev_tag(), "ID_INPUT_KEYBOARD");
        assert_eq!(Kind::TabletPad.udev_tag(), "ID_INPUT_TABLET_PAD");
        assert_eq!(Kind::Keyboard.to_string(), "a keyboard");
    }

    /// Whatever this machine has — none, in a container — the list is in the kernel's order
    /// and every entry is an event device.
    #[test]
    fn the_list_follows_the_kernels_order() {
        let devices = list();
        assert!(
            devices
                .iter()
                .all(|device| device.node.starts_with("event"))
        );
        assert!(
            devices
                .windows(2)
                .all(|pair| node_number(&pair[0].node) < node_number(&pair[1].node)),
            "{:?}",
            devices
                .iter()
                .map(|device| &device.node)
                .collect::<Vec<_>>()
        );
        assert!(node_number("event9") < node_number("event10"));
        assert_eq!(node_number("mouse0"), u32::MAX);
    }

    /// Every code named here is the kernel's, as the evdev crate has it.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[test]
    fn every_code_is_the_kernels() {
        for (name, ours, evdev) in AS_EVDEV_HAS_THEM {
            assert_eq!(ours, evdev, "{name}");
        }
    }
}
