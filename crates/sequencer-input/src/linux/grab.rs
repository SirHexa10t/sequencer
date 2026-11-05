//! The device backend's grab: keyboards held exclusively, and every key they send routed —
//! on to the session through a virtual keyboard, or kept as a profile's trigger.
//!
//! An exclusive grab (`EVIOCGRAB`) is the one way below the display server to keep a key from
//! the session, which is what a profile's trigger needs on Wayland, where nothing above can
//! swallow one. Its price — the process sits between the user and their keyboard — is the one
//! [`super::capture`], a listener, declines to pay. Here it is paid, and made safe four ways:
//! every grab is taken and held by a supervisor process, which ends it if the manager stops
//! answering ([`super::supervise`]) — the manager only reads through its own descriptor of
//! the same open device; a device is grabbed only once no key is held on it, since a key
//! held across the grab would stay down in the session for good; sequencer's own virtual
//! devices are never grabbed, so nothing it sends comes back as the user's input; and a
//! reader that ends while the manager runs on — a failed read, a panic — gives back what it
//! held (`Holding`), since the supervisor, still beaten, would otherwise keep it grabbed,
//! and the next beat stops the manager ([`GrabLost`]) rather than let it run on half-deaf.
//!
//! What is grabbed, for now ([`take`]): every device udev counts as sending keys
//! ([`crate::devices`]), short of the kinds below — keyboards, a mouse's keyboard interface
//! (where the mouse's own macros type), macro pads, media remotes, volume and brightness keys.
//! Mice and pointing sticks are held apart, and only while a profile names one of their
//! buttons ([`Take::Pointer`]; `linux::pointers`). Left alone, each for its own reason
//! ([`Left`]): touchpads, tablets, touchscreens, game controllers and motion sensors, until
//! the device-detection procedure handles them (a touchpad passed on loses palm detection
//! and gestures, and games read controllers directly); a device that reports switches as
//! well, since the virtual keyboard has no way to pass a switch on; a device the session
//! never reads, since passing it on would add input that was never there; and power and
//! sleep buttons, which are logind's.
//!
//! Devices plugged in later — or coming back from a suspend as new devices — are taken the
//! same way, within about a second (`Arrivals`): found through sysfs, by the same rule,
//! once no key is held on them, under the same supervisor (developer's call, 2026-11-04). A
//! manager started through sudo has shed root by then, but kept the `input` group
//! (`elevate::drop_root_after_open` in the CLI), which opens them where the nodes are that
//! group's; where they are root's alone, a new device works as usual, without profiles, until
//! the manager runs again — said once in the log.
//!
//! The keyboards' lights need nothing from here: a grab redirects input only, and the
//! compositor still sets each physical keyboard's Caps Lock from the session's own state.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::os::fd::AsFd as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use evdev::{Device, EventType, InputEvent as DeviceEvent, KeyCode};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use sequencer_core::emit::{Emit, EmitAction, Holdable, InputSink, SinkError};
use sequencer_core::input::{InputEvent, Key, Mods};

use crate::capture::{Epoch, EventQueue};
use crate::devices::{self, Capabilities, InputDevice, Kind};
use crate::linux::inject::{DEVICE_NAME, UinputSink, code_for};
use crate::linux::keymap;
use crate::linux::route::{
    ClaimError, ClaimOwner, DeviceId, OUTPUT, PRESS, Passed, RELEASE, Route, Router, sort_batch,
};
use crate::linux::supervise::{GrabRefused, Supervisor};

/// The name of the virtual keyboard that stands in for the grabbed devices.
pub const KEYBOARD_NAME: &str = "sequencer virtual keyboard";

/// The environment variable limiting the grab to the devices whose names begin with it, for
/// the live tests: a test manager then holds the rig's devices, whose names all begin alike
/// (`linux::rig::TEST_DEVICES`), and nothing of the user's. Unset — as it is everywhere but
/// the live suite — every device [`take`] grabs is grabbed.
pub const GRAB_ONLY_VARIABLE: &str = "SEQUENCER_TEST_GRAB_ONLY";

/// The devices the grab may take, by name: every one, unless the live tests limit it
/// ([`GRAB_ONLY_VARIABLE`]). Read once, as the devices are opened.
#[derive(Clone, Debug, Default)]
pub(super) struct Only(Option<String>);

impl Only {
    fn from_env() -> Self {
        Self(std::env::var(GRAB_ONLY_VARIABLE).ok())
    }

    /// Whether the grab may take a device named `name`.
    pub(super) fn admits(&self, name: &str) -> bool {
        self.0
            .as_deref()
            .is_none_or(|start| name.starts_with(start))
    }
}

/// How long a device may keep a key down before the grab gives up on it: longer than any
/// keystroke, so the Enter that started the manager is long released.
const IDLE_WITHIN: Duration = Duration::from_secs(10);

/// How long between two looks for devices plugged in since the start: about a second,
/// sooner than a person types on a keyboard they have just plugged in. Timed rather than
/// counted in wakes, since a mouse wakes its reader a thousand times a second.
pub(super) const SCAN_EVERY: Duration = Duration::from_secs(1);

/// How often a reader wakes with nothing to read, to notice the grab ending.
pub(super) const READ_POLL: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 100_000_000,
};

/// Keys only a power or sleep button has. A device with nothing else is logind's.
const SYSTEM_KEYS: [KeyCode; 5] = [
    KeyCode::KEY_POWER,
    KeyCode::KEY_POWER2,
    KeyCode::KEY_SLEEP,
    KeyCode::KEY_SUSPEND,
    KeyCode::KEY_WAKEUP,
];

/// Why a device is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Left {
    /// One of sequencer's own virtual devices: what it sends is never the user's input.
    Ours,
    /// It sends no keys.
    NoKeys,
    /// A touchpad, touchscreen, drawing tablet, game controller or motion sensor, which
    /// given: for the device-detection procedure.
    NotYet(Kind),
    /// It reports switches as well — a lid, tablet mode, a radio's kill switch — and the
    /// virtual keyboard has no way to pass a switch on, so grabbing it would lose them.
    Switches,
    /// udev gives it no kind the session reads, so libinput ignores it: passed on through
    /// the virtual keyboard, its input would be new to the session.
    Ignored,
    /// A power or sleep button, which logind handles.
    System,
}

impl fmt::Display for Left {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ours => f.write_str("sequencer's own"),
            Self::NoKeys => f.write_str("sends no keys"),
            Self::NotYet(kind) => write!(f, "{kind} — not held yet"),
            Self::Switches => f.write_str(
                "reports switches too (a lid, tablet mode), which a virtual keyboard can't pass on",
            ),
            Self::Ignored => f.write_str("nothing in the session reads it"),
            Self::System => f.write_str("a power or sleep button — logind's"),
        }
    }
}

/// What the grab does with one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    /// Grabbed, its keys routed.
    Grab {
        /// It is a full keyboard: a grab holding none hears nothing typed.
        keyboard: bool,
    },
    /// A mouse or a pointing stick: held only while a profile names one of its buttons as a
    /// trigger, and then passed on through a stand-in of its own (`linux::pointers`).
    Pointer(Kind),
    /// Left alone, and why.
    Leave(Left),
}

/// The kinds left for the device-detection procedure, the most telling first.
const NOT_YET: [Kind; 6] = [
    Kind::Tablet,
    Kind::TabletPad,
    Kind::Touchpad,
    Kind::Touchscreen,
    Kind::Joystick,
    Kind::Accelerometer,
];

/// The pointers, the more specific first: a pointing stick is a mouse to udev too.
const POINTERS: [Kind; 2] = [Kind::PointingStick, Kind::Mouse];

/// What the grab does with a device named `name` that declares `capabilities`.
#[must_use]
pub fn take(name: &str, capabilities: &Capabilities) -> Take {
    if name == DEVICE_NAME || name == KEYBOARD_NAME {
        return Take::Leave(Left::Ours);
    }
    if capabilities.key_codes().next().is_none() {
        return Take::Leave(Left::NoKeys);
    }
    let kinds = capabilities.kinds();
    let first = |among: &[Kind]| among.iter().copied().find(|kind| kinds.contains(kind));
    if first(&NOT_YET).is_none()
        && let Some(kind) = first(&POINTERS)
    {
        return Take::Pointer(kind);
    }
    let left = if let Some(kind) = first(&NOT_YET) {
        Left::NotYet(kind)
    } else if kinds.contains(&Kind::Switch) {
        Left::Switches
    } else if !kinds.contains(&Kind::Key) {
        Left::Ignored
    } else if capabilities
        .key_codes()
        .all(|code| SYSTEM_KEYS.contains(&KeyCode(code)))
    {
        Left::System
    } else {
        return Take::Grab {
            keyboard: kinds.contains(&Kind::Keyboard),
        };
    };
    Take::Leave(left)
}

/// The `phys` sequencer gives a mouse's stand-in, which otherwise has the mouse's own name
/// and IDs: how sequencer knows its own, where a name can't tell them apart.
pub const STAND_IN_PHYS: &std::ffi::CStr = c"sequencer-stand-in";

/// [`take`], for a device as sysfs lists it — which also tells sequencer's own stand-ins
/// by their `phys`, where the name can't.
#[must_use]
pub fn take_listed(device: &InputDevice) -> Take {
    take_with_phys(&device.name, &device.phys, &device.capabilities)
}

/// [`take`], with the device's `phys` to tell a stand-in by.
fn take_with_phys(name: &str, phys: &str, capabilities: &Capabilities) -> Take {
    if phys.as_bytes() == STAND_IN_PHYS.to_bytes() {
        return Take::Leave(Left::Ours);
    }
    take(name, capabilities)
}

/// Why the grab could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrabError {
    /// Nothing that sends keys could be opened — no access to `/dev/input`, most likely.
    #[error("no keyboard could be opened to grab")]
    NoDevices,
    /// The virtual keyboard could not be made.
    #[error("the virtual keyboard could not be made: {0}")]
    Uinput(#[source] SinkError),
    /// The supervisor went away as it took the grabs: with none to hold them, nothing may
    /// be grabbed.
    #[error("the supervisor went away as it took the grabs: {0}")]
    Supervisor(#[source] io::Error),
}

/// Why a grab can't go on, as its beat finds it: the manager must stop. A manager running
/// on with part of the devices given back would leave its profiles half-deaf, with no way
/// to tell which triggers still fire (developer's call, 2026-11-05).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrabLost {
    /// The supervisor is gone: it gave the devices back, or died.
    #[error("the supervisor is gone: {0}")]
    Supervisor(#[source] io::Error),
    /// The keyboards' reader ended on its own — a failed read, a panic — and gave them
    /// back: no key trigger can fire now.
    #[error("the keyboards' reader stopped, and gave them back")]
    KeyboardReader,
    /// The mice's reader ended on its own, and gave them back: no button trigger can fire.
    #[error("the mice's reader stopped, and gave them back")]
    MouseReader,
}

/// Which reader, if either, has ended while the grab runs on — on its own, then, since
/// the grab's end takes both away first.
fn reader_lost(
    keyboards: Option<&JoinHandle<()>>,
    mice: Option<&JoinHandle<()>>,
) -> Option<GrabLost> {
    if keyboards.is_some_and(JoinHandle::is_finished) {
        Some(GrabLost::KeyboardReader)
    } else if mice.is_some_and(JoinHandle::is_finished) {
        Some(GrabLost::MouseReader)
    } else {
        None
    }
}

/// Locks state the reader shares. A panic elsewhere while holding the lock leaves plain data
/// behind, still worth using: a grab must keep routing, not wedge the keyboard.
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A device the grab takes, open.
#[derive(Debug)]
struct Taken {
    id: DeviceId,
    device: Device,
    name: String,
    /// `event<N>`: its node under `/dev/input`.
    node: String,
    /// It is a full keyboard.
    keyboard: bool,
}

/// The devices the grab takes, open, and the virtual keyboard that stands in for them —
/// nothing grabbed yet. Opened while privileged, if privilege is what opens them;
/// [`Self::start`] once it has been shed.
#[derive(Debug)]
pub struct Opened {
    devices: Vec<Taken>,
    out: UinputSink,
    /// Every node decided about — opened, or left alone — so a later scan looks only at
    /// what arrives after.
    known: BTreeSet<String>,
    /// The devices it may take, as the later scans and the mice may too.
    only: Only,
}

impl Opened {
    /// Opens every device the grab takes ([`take`]) — only those [`GRAB_ONLY_VARIABLE`]
    /// admits, when it is set — and the virtual keyboard, which declares every key the
    /// devices have, and every key a keyboard plugged in later could have
    /// (`every_keyboard_key`), so that each can be passed on.
    ///
    /// # Errors
    ///
    /// [`GrabError::NoDevices`] when nothing to grab could be opened, and
    /// [`GrabError::Uinput`] when the virtual keyboard could not be made.
    pub fn open() -> Result<Self, GrabError> {
        let only = Only::from_env();
        let mut devices = Vec::new();
        let mut keys: BTreeSet<KeyCode> = every_keyboard_key().collect();
        let mut known = BTreeSet::new();
        let mut number = 0;
        for (path, device) in evdev::enumerate() {
            let name = device.name().unwrap_or_default().to_owned();
            let node = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            known.insert(node.clone());
            if !only.admits(&name) {
                log::debug!("leaving device={name} why=not {GRAB_ONLY_VARIABLE}");
                continue;
            }
            let capabilities = Capabilities::of(&device);
            let phys = device.physical_path().unwrap_or_default().to_owned();
            match take_with_phys(&name, &phys, &capabilities) {
                Take::Grab { keyboard } => {
                    number += 1;
                    keys.extend(capabilities.key_codes().map(KeyCode));
                    log::debug!("grabbing device={name} path={}", path.display());
                    devices.push(Taken {
                        id: DeviceId(number),
                        device,
                        name,
                        node,
                        keyboard,
                    });
                }
                // The mice are the pointer thread's, taken when a profile names their buttons.
                Take::Pointer(_) => log::debug!("pointer device={name} held only when named"),
                Take::Leave(why) => log::debug!("leaving device={name} why={why:?}"),
            }
        }
        if devices.is_empty() {
            return Err(GrabError::NoDevices);
        }
        let out = UinputSink::open_as(KEYBOARD_NAME, keys).map_err(GrabError::Uinput)?;
        Ok(Self {
            devices,
            out,
            known,
            only,
        })
    }

    /// The names of the devices the grab would take, for saying what it holds.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.devices
            .iter()
            .map(|taken| taken.name.clone())
            .collect()
    }

    /// Has `supervisor` grab every device once no key is held on it, and starts routing:
    /// reported triggers go to `queue`, stamped on `epoch`'s timeline. A device that stays
    /// busy — a key held throughout, or another program's grab on it — is left alone, said
    /// so in the log.
    ///
    /// # Errors
    ///
    /// [`GrabError::Supervisor`] if the supervisor went away as it took the grabs, and
    /// [`GrabError::NoDevices`] if no device could be grabbed in the end.
    pub fn start(
        self,
        supervisor: Supervisor,
        epoch: &Epoch,
        queue: EventQueue,
    ) -> Result<DeviceGrab, GrabError> {
        let mut grabbed = Vec::new();
        for taken in self.devices {
            if let Some(taken) = grab_when_idle(taken, &supervisor)? {
                grabbed.push(taken);
            }
        }
        if grabbed.is_empty() {
            supervisor.finish();
            return Err(GrabError::NoDevices);
        }
        let held = grabbed.iter().map(|taken| taken.name.clone()).collect();
        let holds_a_keyboard = grabbed.iter().any(|taken| taken.keyboard);
        let arrivals = Arrivals {
            known: self.known,
            warned: BTreeSet::new(),
            last_id: grabbed.iter().map(|taken| taken.id.0).max().unwrap_or(0),
            only: self.only.clone(),
        };
        // Shared with the reader, which grabs devices plugged in later; taken back out — and
        // so out of the reader's reach — when the grab ends.
        let supervisor = Arc::new(Mutex::new(Some(supervisor)));
        let shared = Arc::new(Mutex::new(Shared {
            router: Router::new(),
            out: self.out,
            stopping: false,
        }));
        let running = Arc::new(AtomicBool::new(true));
        let reader = {
            let (shared, running, epoch, queue, supervisor) = (
                Arc::clone(&shared),
                Arc::clone(&running),
                epoch.clone(),
                queue.clone(),
                Arc::clone(&supervisor),
            );
            std::thread::Builder::new()
                .name("sequencer-grab".into())
                .spawn(move || {
                    let ends = Ends {
                        shared: &shared,
                        queue: &queue,
                        epoch: &epoch,
                        running: &running,
                        supervisor: &supervisor,
                    };
                    read(grabbed, arrivals, &ends);
                })
        };
        let Ok(reader) = reader else {
            // No reader, so no grab may stand: the supervisor lets every device go at once.
            if let Some(supervisor) = lock(&supervisor).take() {
                supervisor.finish();
            }
            return Err(GrabError::NoDevices);
        };
        // The mice, on a thread of their own: held only while a claim names their buttons.
        let wanted = Arc::new(Mutex::new(BTreeSet::new()));
        let pointers = {
            let ends = super::pointers::Ends {
                shared: Arc::clone(&shared),
                queue: queue.clone(),
                epoch: epoch.clone(),
                running: Arc::clone(&running),
                supervisor: Arc::clone(&supervisor),
                wanted: Arc::clone(&wanted),
                only: self.only,
            };
            std::thread::Builder::new()
                .name("sequencer-pointers".into())
                .spawn(move || super::pointers::run(&ends))
                .inspect_err(|err| {
                    log::warn!(
                        "the pointer thread could not start ({err}), so mouse buttons can't be triggers"
                    );
                })
                .ok()
        };
        // Without its watcher the grab still runs, and says what it can no longer tell.
        let watcher = {
            let (shared, running) = (Arc::clone(&shared), Arc::clone(&running));
            std::thread::Builder::new()
                .name("sequencer-lock-watch".into())
                .spawn(move || watch_lock(&shared, &running))
                .inspect_err(|err| {
                    log::warn!(
                        "the lock watch could not start ({err}), so profiles keep acting at the lock screen"
                    );
                })
                .ok()
        };
        Ok(DeviceGrab {
            shared,
            running,
            reader: Some(reader),
            watcher,
            supervisor,
            owners: AtomicU32::new(0),
            held,
            holds_a_keyboard,
            queue,
            epoch: epoch.clone(),
            wanted,
            pointers,
        })
    }
}

/// Has `supervisor` grab `taken` once no key is held on it: `Ok(None)` when it stays busy —
/// a key held throughout, another program's grab — or can't be read without blocking, said
/// so in the log.
///
/// # Errors
///
/// The supervisor went away: nothing may be grabbed without it.
fn grab_when_idle(taken: Taken, supervisor: &Supervisor) -> Result<Option<Taken>, GrabError> {
    let name = &taken.name;
    if !idle_within(&taken.device, IDLE_WITHIN) {
        log::warn!(
            "a key stayed down on {name}, so it was left ungrabbed: profiles don't apply to it this run"
        );
        return Ok(None);
    }
    // Before the grab, so that a device the reader couldn't poll is never grabbed at all.
    if let Err(err) = taken.device.set_nonblocking(true) {
        log::warn!("{name} could not be read without blocking, so it was left ungrabbed: {err}");
        return Ok(None);
    }
    match supervisor.grab(taken.device.as_fd()) {
        Ok(()) => Ok(Some(taken)),
        Err(GrabRefused::Busy) => {
            log::warn!(
                "{name} is held by another program (keyd, kanata or input-remapper?), so profiles don't apply to it"
            );
            Ok(None)
        }
        Err(GrabRefused::Gone(err)) => Err(GrabError::Supervisor(err)),
        Err(err) => {
            log::warn!("{name} could not be grabbed: {err}");
            Ok(None)
        }
    }
}

/// Whether some open file holds `node`'s device grabbed, asked by trying to grab it — a
/// trial grab that, when it takes, is let go at once. What the tests ask of a grab, from
/// outside it.
#[cfg(any(test, feature = "live-tests"))]
pub(crate) fn is_grabbed(node: &std::path::Path) -> io::Result<bool> {
    let mut probe = Device::open(node)?;
    match probe.grab() {
        Ok(()) => probe.ungrab().map(|()| false),
        Err(err) if Errno::from_io_error(&err) == Some(Errno::BUSY) => Ok(true),
        Err(err) => Err(err),
    }
}

/// How often the lock is asked about: a password is typed seconds after the screen locks,
/// and a quarter of a second is well inside that.
const LOCK_POLL: Duration = Duration::from_millis(250);

/// Follows the session's lock until the grab ends: while it is locked the router claims
/// nothing and profiles send only releases, so the password reaches the lock screen as
/// typed. A session whose lock can't be read is said so once, and goes unwatched.
fn watch_lock(shared: &Mutex<Shared>, running: &AtomicBool) {
    let unwatched = |why: &str| {
        log::warn!(
            "{why}, so the grab can't tell when the screen locks: profiles keep acting at the lock screen"
        );
    };
    let Ok(mut bus) = crate::dbus::Connection::system() else {
        return unwatched("no system bus");
    };
    let Ok(uid) = crate::dbus::effective_uid() else {
        return unwatched("no user id to ask logind about");
    };
    let session = crate::logind::display_session(&mut bus, uid);
    while running.load(Ordering::Relaxed) {
        let Some(locked) = crate::logind::locked(&mut bus, &session) else {
            return unwatched("logind doesn't say whether the screen is locked");
        };
        let mut shared = lock(shared);
        if shared.router.paused() != locked {
            shared.router.set_paused(locked);
            log::info!(
                "screen locked={locked}: profiles {}",
                if locked { "paused" } else { "resumed" }
            );
        }
        drop(shared);
        std::thread::sleep(LOCK_POLL);
    }
}

/// Whether `device` reports no key down within `within`: a key held when the grab takes
/// effect would stay down in the session, since its release would then come here instead.
/// A device that can't say is taken as idle.
fn idle_within(device: &Device, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        match device.get_key_state() {
            Ok(keys) if keys.iter().next().is_some() => {}
            _ => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the readers and the manager share.
#[derive(Debug)]
pub(super) struct Shared {
    pub(super) router: Router,
    /// The virtual keyboard: everything passed on, and everything a profile sends.
    out: UinputSink,
    /// Set as the grab ends, before any device is let go: a frame read after that is the
    /// session's already, and passing it on would type it twice.
    pub(super) stopping: bool,
}

/// What the reader works with besides its devices: where it routes to, and the grab's own
/// state.
struct Ends<'a> {
    shared: &'a Mutex<Shared>,
    queue: &'a EventQueue,
    epoch: &'a Epoch,
    running: &'a AtomicBool,
    supervisor: &'a Mutex<Option<Supervisor>>,
}

/// Reads every grabbed device until the grab ends, routing what each sends — and about once
/// a second looks for devices plugged in since, taking those the rule takes.
fn read(devices: Vec<Taken>, mut arrivals: Arrivals, ends: &Ends<'_>) {
    let mut devices = Holding::new(devices, ends.running, |taken: Taken| {
        log::warn!(
            "{} is given back to the session: its reader ended",
            taken.name
        );
        let_go(taken, ends);
    });
    let mut looked: Option<Instant> = None;
    while ends.running.load(Ordering::Relaxed) {
        if looked.is_none_or(|at| at.elapsed() >= SCAN_EVERY) {
            arrivals.admit(&mut devices, ends.supervisor);
            looked = Some(Instant::now());
        }
        let ready: Vec<usize> = {
            let mut fds: Vec<PollFd<'_>> = devices
                .iter()
                .map(|taken| PollFd::new(&taken.device, PollFlags::IN))
                .collect();
            // With nothing grabbed — the last keyboard unplugged — this is the wait until the
            // next look for one.
            match poll(&mut fds, Some(&READ_POLL)) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(err) => {
                    log::warn!("reading the grabbed devices failed: {err}");
                    return;
                }
            }
            fds.iter()
                .enumerate()
                .filter(|(_, fd)| !fd.revents().is_empty())
                .map(|(index, _)| index)
                .collect()
        };
        let mut gone = Vec::new();
        for index in ready {
            let taken = &mut devices[index];
            match taken.device.fetch_events() {
                Ok(events) => {
                    let events: Vec<DeviceEvent> = events.collect();
                    route(taken.id, &events, ends.shared, ends.queue, ends.epoch);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => gone.push(index),
            }
        }
        for index in gone.into_iter().rev() {
            let taken = devices.remove(index);
            // Its node may come back, a device plugged in again: then it is new.
            arrivals.known.remove(&taken.node);
            log::warn!(
                "{} went away; profiles take it again if it comes back",
                taken.name
            );
            let_go(taken, ends);
        }
    }
}

/// Lets `taken` go for good — gone from the system, or given back as its reader ends.
fn let_go(taken: Taken, ends: &Ends<'_>) {
    // The supervisor ends the grab and closes its handle too, or it keeps one for every
    // device that ever went.
    if let Some(supervisor) = lock(ends.supervisor).as_ref() {
        let _ = supervisor.release(taken.device.as_fd());
    }
    let forgotten = {
        let mut shared = lock(ends.shared);
        let forgotten = shared.router.forget(taken.id);
        // What it held on the virtual keyboard comes up there, or it stays down for good: no
        // release from it can arrive now.
        if !forgotten.release.is_empty() {
            let ups: Vec<DeviceEvent> = forgotten
                .release
                .iter()
                .map(|code| DeviceEvent::new(EventType::KEY.0, code.0, 0))
                .collect();
            let _ = shared.out.forward(&ups);
        }
        forgotten
    };
    // Its held triggers come up for the manager, so no profile waits on a release that can't
    // arrive now.
    for (kind, mods) in forgotten.reports {
        let _ = ends
            .queue
            .offer(InputEvent::physical(ends.epoch.now(), kind).with_mods(mods));
    }
    // Its handle last, closing the open file the grab was on.
    drop(taken);
}

/// What a reader holds, given back if the reader ends on its own — its read failed, or it
/// panicked — rather than with the grab. The supervisor would keep the grabs otherwise, and
/// with nobody reading, those devices would be dead to the session until the manager
/// stopped. The keyboards' reader and the mice's thread each hold theirs in one.
pub(super) struct Holding<'a, T> {
    held: Vec<T>,
    /// Down once the grab is ending, when the supervisor lets everything go itself.
    running: &'a AtomicBool,
    give_back: Box<dyn FnMut(T) + 'a>,
}

impl<'a, T> Holding<'a, T> {
    /// `held`, given back one by one through `give_back` should the reader end while
    /// `running` is still up.
    pub(super) fn new(
        held: Vec<T>,
        running: &'a AtomicBool,
        give_back: impl FnMut(T) + 'a,
    ) -> Self {
        Self {
            held,
            running,
            give_back: Box::new(give_back),
        }
    }
}

impl<T> std::ops::Deref for Holding<'_, T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Vec<T> {
        &self.held
    }
}

impl<T> std::ops::DerefMut for Holding<'_, T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.held
    }
}

impl<T> Drop for Holding<'_, T> {
    fn drop(&mut self) {
        // Ended with the grab: the supervisor has let everything go already.
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        for held in self.held.drain(..) {
            (self.give_back)(held);
        }
    }
}

/// Devices plugged in after the start, taken as the ones at the start were: by the same rule
/// ([`take`]), once no key is held on them, under the same supervisor.
///
/// Every node decided about — held, or left alone — is remembered until it goes from sysfs,
/// so a scan looks only at what is new. One that can't be opened yet is tried again at the
/// next scan: udev sets a new node's permissions a moment after the kernel makes it, and a
/// key held as it arrives keeps it waiting as at the start.
#[derive(Debug)]
struct Arrivals {
    known: BTreeSet<String>,
    /// Nodes already warned about, so a device that stays unopenable is said once.
    warned: BTreeSet<String>,
    /// The last number given to a device.
    last_id: u32,
    /// The devices the grab may take.
    only: Only,
}

impl Arrivals {
    /// Looks for devices that arrived since the last scan, and grabs those the rule takes.
    fn admit(&mut self, devices: &mut Vec<Taken>, supervisor: &Mutex<Option<Supervisor>>) {
        for device in self.arrived(devices::list()) {
            if let Some(taken) = self.take_in(device, supervisor) {
                log::info!("holding device={} since it was plugged in", taken.name);
                devices.push(taken);
            }
        }
    }

    /// The devices `listed` that no scan has decided about yet. Nodes no longer listed are
    /// forgotten, so a node that comes back is new.
    fn arrived(&mut self, listed: Vec<InputDevice>) -> Vec<InputDevice> {
        self.known
            .retain(|node| listed.iter().any(|device| device.node == *node));
        self.warned
            .retain(|node| listed.iter().any(|device| device.node == *node));
        listed
            .into_iter()
            .filter(|device| !self.known.contains(&device.node))
            .collect()
    }

    /// Opens and grabs `device` if the rule takes it — remembered as decided either way,
    /// unless it couldn't be opened or held a key, which the next scan tries again.
    fn take_in(
        &mut self,
        device: InputDevice,
        supervisor: &Mutex<Option<Supervisor>>,
    ) -> Option<Taken> {
        if !self.only.admits(&device.name) {
            self.known.insert(device.node);
            return None;
        }
        let keyboard = match take_listed(&device) {
            Take::Grab { keyboard } => keyboard,
            // The mice are the pointer thread's.
            Take::Pointer(_) => {
                self.known.insert(device.node);
                return None;
            }
            Take::Leave(why) => {
                log::debug!("leaving device={} why={why:?}", device.name);
                self.known.insert(device.node);
                return None;
            }
        };
        let opened = match Device::open(device.path()) {
            Ok(opened) => opened,
            Err(err) => {
                if self.warned.insert(device.node.clone()) {
                    log::warn!(
                        "{} was plugged in but can't be opened ({err}), so profiles don't apply to it until the manager runs again",
                        device.name
                    );
                }
                return None;
            }
        };
        if opened
            .get_key_state()
            .is_ok_and(|keys| keys.iter().next().is_some())
        {
            return None;
        }
        self.known.insert(device.node.clone());
        if let Err(err) = opened.set_nonblocking(true) {
            log::warn!(
                "{} could not be read without blocking, so it was left ungrabbed: {err}",
                device.name
            );
            return None;
        }
        // `None` once the grab is ending: nothing more is taken then.
        let refused = lock(supervisor).as_ref()?.grab(opened.as_fd());
        match refused {
            Ok(()) => {
                self.last_id += 1;
                Some(Taken {
                    id: DeviceId(self.last_id),
                    device: opened,
                    name: device.name,
                    node: device.node,
                    keyboard,
                })
            }
            Err(GrabRefused::Busy) => {
                log::warn!(
                    "{} is held by another program (keyd, kanata or input-remapper?), so profiles don't apply to it",
                    device.name
                );
                None
            }
            Err(err) => {
                log::warn!("{} could not be grabbed: {err}", device.name);
                None
            }
        }
    }
}

/// Every key a keyboard can send: the `KEY_*` codes, short of the button ranges among them.
/// The virtual keyboard declares them all, so a keyboard plugged in later can be passed on
/// whatever keys it has. The buttons stay out because no keyboard key lives there, and
/// because the session judges a device's kind by its buttons: today's udev keeps a device
/// with keys from four keyboard groups a keyboard whatever joystick buttons it adds, but a
/// virtual keyboard taken for a joystick would be ignored by libinput, and everything passed
/// on with it — not a risk worth a key nobody has.
fn every_keyboard_key() -> impl Iterator<Item = KeyCode> {
    // From `linux/input-event-codes.h`: buttons run 0x100-0x15f, the D-pad 0x220-0x223, the
    // trigger-happy buttons 0x2c0-0x2e7; KEY_MAX is 0x2ff.
    (0x001..0x100)
        .chain(0x160..0x220)
        .chain(0x224..0x2c0)
        .chain(0x2e8..=0x2ff)
        .map(KeyCode)
}

/// Routes one batch from `device`: each frame on to the virtual keyboard without the keys a
/// profile keeps, and the kept presses and releases to the manager.
fn route(
    device: DeviceId,
    events: &[DeviceEvent],
    shared: &Mutex<Shared>,
    queue: &EventQueue,
    epoch: &Epoch,
) {
    let mut shared = lock(shared);
    if shared.stopping {
        return;
    }
    let sorted = sort_batch(events, |code, value| {
        Some(shared.router.route(device, code, value))
    });
    for passed in sorted {
        match passed {
            Passed::Frame(frame) => {
                let _ = shared.out.forward(&frame);
            }
            Passed::Report(kind, mods) => {
                if !queue.offer(InputEvent::physical(epoch.now(), kind).with_mods(mods)) {
                    log::trace!("trigger queue full, event dropped");
                }
            }
        }
    }
}

/// The grabbed devices, routing — what the manager runs profiles on.
#[derive(Debug)]
pub struct DeviceGrab {
    shared: Arc<Mutex<Shared>>,
    running: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    /// Follows the session's lock, pausing the routing while it is locked.
    watcher: Option<JoinHandle<()>>,
    /// Holds every grab; the one that can end them. Shared with the reader, which grabs the
    /// devices plugged in later; `None` once the grab has ended.
    supervisor: Arc<Mutex<Option<Supervisor>>>,
    owners: AtomicU32,
    /// The names of the devices grabbed, in the order they were.
    held: Vec<String>,
    /// Whether a full keyboard is among them.
    holds_a_keyboard: bool,
    /// Where triggers are reported — a profile's output too, when it fires one — on whose
    /// timeline.
    queue: EventQueue,
    epoch: Epoch,
    /// The mouse buttons claimed as triggers now, which the pointer thread holds mice for:
    /// set at every claim and its release, under a lock of its own.
    wanted: Arc<Mutex<BTreeSet<KeyCode>>>,
    /// Holds the mice a claim names, and passes them on.
    pointers: Option<JoinHandle<()>>,
}

impl DeviceGrab {
    /// The names of the devices grabbed, in the order they were — one physical device is
    /// often several of one name.
    #[must_use]
    pub fn held(&self) -> &[String] {
        &self.held
    }

    /// Whether a full keyboard is among the devices grabbed: without one, nothing typed
    /// reaches a profile.
    #[must_use]
    pub const fn holds_a_keyboard(&self) -> bool {
        self.holds_a_keyboard
    }

    /// Claims `chords` as triggers: from now on each one's press, with exactly its modifier
    /// classes held, is kept from the session and reported. The claim lasts until stopped
    /// or dropped.
    ///
    /// # Errors
    ///
    /// [`ClaimError`]: a chord that is not one key plus modifiers, or one already claimed.
    pub fn claim(&self, chords: &[Vec<Holdable>]) -> Result<DeviceClaim, ClaimError> {
        let owner = ClaimOwner(self.owners.fetch_add(1, Ordering::Relaxed));
        let mut shared = lock(&self.shared);
        shared.router.claim(owner, chords)?;
        *lock(&self.wanted) = shared.router.claimed_buttons();
        Ok(DeviceClaim {
            owner,
            shared: Some(Arc::clone(&self.shared)),
            wanted: Arc::clone(&self.wanted),
        })
    }

    /// The sink a profile sends through: the virtual keyboard, in order with what is passed
    /// on.
    #[must_use]
    pub fn sink(&self) -> DeviceSink {
        DeviceSink {
            shared: Arc::clone(&self.shared),
            queue: self.queue.clone(),
            epoch: self.epoch.clone(),
        }
    }

    /// Whether any of `keys` is down on a grabbed device.
    #[must_use]
    pub fn any_down(&self, keys: &[Key]) -> bool {
        lock(&self.shared).router.any_down(keys)
    }

    /// Taps `targets` clean: held modifiers of `held`'s classes the targets don't name come up
    /// first and go back down after — those still held by then — all on the virtual keyboard,
    /// in one go, so nothing the hand does lands in between. The tap itself is a profile's
    /// output, sent by a bind running `depth` deep: routed as [`DeviceSink`] routes it, with the
    /// lifted modifiers out of the chord while it lasts, as the session sees them. `false` when
    /// a target has no code, or the virtual keyboard refused.
    #[must_use]
    pub fn tap_lifted(
        &self,
        targets: &[Holdable],
        held: Mods,
        duration: Duration,
        depth: u8,
    ) -> bool {
        let Some(codes) = targets
            .iter()
            .map(|target| keymap::holdable_to_code(*target))
            .collect::<Option<Vec<KeyCode>>>()
        else {
            return false;
        };
        let keys: Vec<Key> = targets
            .iter()
            .filter_map(|target| match target {
                Holdable::Key(key) => Some(*key),
                Holdable::Button(_) => None,
            })
            .collect();
        let key = |code: KeyCode, value: i32| DeviceEvent::new(EventType::KEY.0, code.0, value);
        let mut shared = lock(&self.shared);
        // Locked, a profile taps nothing: done, as far as the profile is concerned.
        if shared.router.paused() {
            return true;
        }
        let lifted = shared.router.lift_plan(&keys, held);
        let lifts: Vec<DeviceEvent> = lifted.iter().map(|code| key(*code, 0)).collect();
        let mut sent = lifts.is_empty() || shared.out.forward(&lifts).is_ok();
        shared.router.set_lifted(&lifted);
        for &code in &codes {
            sent &= send_output(&mut shared, code, PRESS, depth, &self.queue, &self.epoch).is_ok();
        }
        std::thread::sleep(duration);
        for &code in codes.iter().rev() {
            sent &=
                send_output(&mut shared, code, RELEASE, depth, &self.queue, &self.epoch).is_ok();
        }
        shared.router.set_lifted(&[]);
        // Back down only what the hand still holds; one let go meanwhile stays up.
        let restore: Vec<DeviceEvent> = lifted
            .iter()
            .filter(|code| shared.router.holds(**code))
            .map(|code| key(*code, 1))
            .collect();
        if !restore.is_empty() {
            sent &= shared.out.forward(&restore).is_ok();
        }
        sent
    }

    /// Tells the supervisor the manager still answers. Due every
    /// [`super::supervise::BEAT_EVERY`].
    ///
    /// # Errors
    ///
    /// Why the grab can't go on, and the manager must stop: the supervisor is gone — it gave
    /// the keyboards back — or a reader stopped on its own and gave back what it held.
    pub fn beat(&self) -> Result<(), GrabLost> {
        if let Some(lost) = reader_lost(self.reader.as_ref(), self.pointers.as_ref()) {
            return Err(lost);
        }
        lock(&self.supervisor)
            .as_ref()
            .map_or(Ok(()), Supervisor::beat)
            .map_err(GrabLost::Supervisor)
    }

    /// Ends the grab: nothing more is passed on, the supervisor lets every device go back to
    /// the session — at once, holding every grab — and the virtual keyboard lets go of what
    /// profiles held. Stopping twice does nothing the second time.
    pub fn stop(&mut self) {
        let Some(supervisor) = lock(&self.supervisor).take() else {
            return;
        };
        // The readers stop first — the mice's passes nothing on once this is down — and then
        // the devices go back.
        self.running.store(false, Ordering::SeqCst);
        lock(&self.shared).stopping = true;
        supervisor.finish();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        if let Some(pointers) = self.pointers.take() {
            let _ = pointers.join();
        }
        release_output(&mut lock(&self.shared), &self.queue, &self.epoch);
    }
}

impl Drop for DeviceGrab {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A claim on some triggers, until stopped or dropped.
#[derive(Debug)]
pub struct DeviceClaim {
    owner: ClaimOwner,
    shared: Option<Arc<Mutex<Shared>>>,
    /// The grab's wanted buttons, kept current as this claim goes.
    wanted: Arc<Mutex<BTreeSet<KeyCode>>>,
}

impl DeviceClaim {
    /// Lets the triggers go: their presses pass on to the session again. A trigger that is
    /// down stays kept until it comes up. Stopping twice does nothing the second time.
    pub fn stop(&mut self) {
        if let Some(shared) = self.shared.take() {
            let mut shared = lock(&shared);
            shared.router.release(self.owner);
            *lock(&self.wanted) = shared.router.claimed_buttons();
        }
    }
}

impl Drop for DeviceClaim {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A profile's sink: the virtual keyboard the grabbed devices are passed on through.
///
/// Each event is routed as a hand's would be — so it can fire another profile's trigger,
/// while the chain is shallower than [`crate::linux::route::CHAIN_DEPTH`] — and what goes on
/// is written at once, as a frame of its own. Staged instead, it would wait for the next
/// flush, which a sequence reaches only at the end of its pass: every wait and tap length in
/// between would be squeezed out, all of a pass leaving at once.
#[derive(Debug)]
pub struct DeviceSink {
    shared: Arc<Mutex<Shared>>,
    queue: EventQueue,
    epoch: Epoch,
}

impl InputSink for DeviceSink {
    fn emit(&mut self, emit: &Emit) -> Result<(), SinkError> {
        let (code, value) = output_event(emit.action)?;
        let mut shared = lock(&self.shared);
        // Locked, only releases go out: nothing types into the lock screen, and nothing
        // pressed before the lock is left down.
        if shared.router.paused() && value == PRESS {
            return Ok(());
        }
        send_output(
            &mut shared,
            code,
            value,
            emit.level,
            &self.queue,
            &self.epoch,
        )
    }

    /// Nothing waits to be flushed: every event was written as it was sent.
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn release_all(&mut self) {
        release_output(&mut lock(&self.shared), &self.queue, &self.epoch);
    }
}

/// A key or button a profile sends, as the code and the value it goes out with — or why it
/// can't go out at all.
fn output_event(action: EmitAction) -> Result<(KeyCode, i32), SinkError> {
    if let Some(what) = action.holds() {
        return code_for(what).map(|code| (code, PRESS));
    }
    if let Some(what) = action.releases() {
        return code_for(what).map(|code| (code, RELEASE));
    }
    Err(match action {
        EmitAction::Scroll { .. } => SinkError::Unsupported("scrolling"),
        EmitAction::CursorTo { .. } | EmitAction::CursorBy { .. } => {
            SinkError::Unsupported("moving the cursor")
        }
        // `EmitAction` is non-exhaustive. Refusing an action added later is right: silently
        // ignoring one would look like the backend worked.
        _ => SinkError::Unsupported("that action"),
    })
}

/// One event of a profile's output, sent by a bind running `depth` deep: routed, and what
/// goes on written at once. A trigger it fires is reported one deeper, so the bind it runs
/// sends at that depth in turn.
fn send_output(
    shared: &mut Shared,
    code: KeyCode,
    value: i32,
    depth: u8,
    queue: &EventQueue,
    epoch: &Epoch,
) -> Result<(), SinkError> {
    match shared.router.route_output(code, value, depth) {
        Route::Forward => shared
            .out
            .forward(&[DeviceEvent::new(EventType::KEY.0, code.0, value)]),
        Route::Report(kind, mods) => {
            let fired = InputEvent::synthetic(epoch.now(), depth.saturating_add(1), kind);
            if !queue.offer(fired.with_mods(mods)) {
                log::trace!("trigger queue full, a chained event dropped");
            }
            Ok(())
        }
        Route::Keep => Ok(()),
    }
}

/// Lets up all that profiles' output holds on the virtual keyboard — but what a hand holds
/// too — and ends the triggers it holds down, so no chained bind waits on them.
fn release_output(shared: &mut Shared, queue: &EventQueue, epoch: &Epoch) {
    let forgotten = shared.router.forget(OUTPUT);
    if !forgotten.release.is_empty() {
        let ups: Vec<DeviceEvent> = forgotten
            .release
            .iter()
            .map(|code| DeviceEvent::new(EventType::KEY.0, code.0, 0))
            .collect();
        let _ = shared.out.forward(&ups);
    }
    for (kind, mods) in forgotten.reports {
        let _ = queue.offer(InputEvent::synthetic(epoch.now(), 1, kind).with_mods(mods));
    }
}

#[cfg(test)]
mod tests {
    use evdev::{AbsoluteAxisCode, PropType, RelativeAxisCode};

    use super::*;

    fn codes(keys: &[KeyCode]) -> Vec<u16> {
        keys.iter().map(|key| key.0).collect()
    }

    /// A keyboard's main block: every code from Escape (1) to Keypad . (83).
    fn keyboard() -> Capabilities {
        Capabilities::default().with_keys(1..=83)
    }

    fn mouse() -> Capabilities {
        Capabilities::default()
            .with_keys(codes(&[
                KeyCode::BTN_LEFT,
                KeyCode::BTN_RIGHT,
                KeyCode::BTN_MIDDLE,
            ]))
            .with_relative([
                RelativeAxisCode::REL_X.0,
                RelativeAxisCode::REL_Y.0,
                RelativeAxisCode::REL_WHEEL.0,
            ])
    }

    /// Whatever udev counts as sending keys is grabbed, whatever it calls itself — and the
    /// grab knows which of what it holds are keyboards.
    #[test]
    fn whatever_sends_keys_is_grabbed() {
        let a_keyboard = Take::Grab { keyboard: true };
        assert_eq!(take("USB Keyboard", &keyboard()), a_keyboard);
        assert_eq!(
            take("Gaming Mouse Keyboard", &keyboard()),
            a_keyboard,
            "a mouse's keyboard interface"
        );
        let volume_wheel = keyboard()
            .with_relative([RelativeAxisCode::REL_HWHEEL.0])
            .with_absolute([AbsoluteAxisCode::ABS_VOLUME.0]);
        assert_eq!(
            take("Gaming Keyboard", &volume_wheel),
            a_keyboard,
            "a volume wheel's axis is no touchpad's"
        );
        let remote = Capabilities::default().with_keys(codes(&[
            KeyCode::KEY_VOLUMEUP,
            KeyCode::KEY_VOLUMEDOWN,
            KeyCode::KEY_PLAYPAUSE,
            KeyCode::KEY_POWER,
        ]));
        assert_eq!(
            take("Consumer Control", &remote),
            Take::Grab { keyboard: false },
            "a power key among media keys is still a remote"
        );
    }

    /// What is left alone, and why: each kind for its own reason.
    #[test]
    fn each_kind_left_alone_has_its_reason() {
        assert_eq!(take(KEYBOARD_NAME, &keyboard()), Take::Leave(Left::Ours));
        assert_eq!(take(DEVICE_NAME, &keyboard()), Take::Leave(Left::Ours));
        let lid = Capabilities::default().with_switches();
        assert_eq!(take("Lid Switch", &lid), Take::Leave(Left::NoKeys));
        assert_eq!(take("Wireless Mouse", &mouse()), Take::Pointer(Kind::Mouse));
        let many_buttons = Capabilities::default()
            .with_keys(KeyCode::BTN_LEFT.0..KeyCode::BTN_LEFT.0 + 32)
            .with_relative([RelativeAxisCode::REL_X.0, RelativeAxisCode::REL_Y.0]);
        assert_eq!(
            take("Many-Button Mouse", &many_buttons),
            Take::Pointer(Kind::Mouse),
            "32 buttons are a mouse's, not a controller's"
        );
        let stick = mouse().with_properties([PropType::POINTING_STICK.0]);
        assert_eq!(
            take("TrackPoint", &stick),
            Take::Pointer(Kind::PointingStick)
        );
        let touchpad = Capabilities::default()
            .with_keys(codes(&[
                KeyCode::BTN_LEFT,
                KeyCode::BTN_TOOL_FINGER,
                KeyCode::BTN_TOUCH,
            ]))
            .with_absolute([AbsoluteAxisCode::ABS_X.0, AbsoluteAxisCode::ABS_Y.0]);
        assert_eq!(
            take("Touchpad", &touchpad),
            Take::Leave(Left::NotYet(Kind::Touchpad))
        );
        let pad = Capabilities::default().with_keys(codes(&[
            KeyCode::BTN_SOUTH,
            KeyCode::BTN_EAST,
            KeyCode::BTN_START,
        ]));
        assert_eq!(
            take("Game Pad", &pad),
            Take::Leave(Left::NotYet(Kind::Joystick)),
            "a pad with no sticks is still a controller"
        );
        let extra_buttons = Capabilities::default()
            .with_keys(codes(&[KeyCode::KEY_MICMUTE, KeyCode::KEY_CONFIG]))
            .with_switches();
        assert_eq!(
            take("Laptop Extra Buttons", &extra_buttons),
            Take::Leave(Left::Switches),
            "its tablet-mode switch would be lost"
        );
        let buttons_only = Capabilities::default().with_keys(codes(&[
            KeyCode::BTN_0,
            KeyCode::BTN_1,
            KeyCode::BTN_2,
        ]));
        assert_eq!(
            take("Button Box", &buttons_only),
            Take::Leave(Left::Ignored),
            "the session never reads buttons alone"
        );
        let power = Capabilities::default().with_keys(codes(&[KeyCode::KEY_POWER]));
        assert_eq!(take("Power Button", &power), Take::Leave(Left::System));
        let sleep =
            Capabilities::default().with_keys(codes(&[KeyCode::KEY_SLEEP, KeyCode::KEY_WAKEUP]));
        assert_eq!(take("Sleep Button", &sleep), Take::Leave(Left::System));
    }

    /// The virtual keyboard declares every key a keyboard could send — so a keyboard plugged
    /// in later is passed on whole — besides what it clicks with, and udev must still call it a
    /// keyboard and a mouse: as a joystick or a tablet, libinput would ignore it, and with it
    /// everything passed on.
    #[test]
    fn the_virtual_keyboard_stays_a_keyboard_whatever_it_declares() {
        let pointer_axes = [
            RelativeAxisCode::REL_X.0,
            RelativeAxisCode::REL_Y.0,
            RelativeAxisCode::REL_WHEEL.0,
            RelativeAxisCode::REL_HWHEEL.0,
        ];
        let kinds = Capabilities::default()
            .with_keys(
                every_keyboard_key()
                    .chain(keymap::all_codes())
                    .map(|code| code.0),
            )
            .with_relative(pointer_axes)
            .kinds();
        assert!(
            kinds.contains(&Kind::Keyboard) && kinds.contains(&Kind::Mouse),
            "{kinds:?}"
        );
        for not in [
            Kind::Joystick,
            Kind::Tablet,
            Kind::Touchpad,
            Kind::Touchscreen,
        ] {
            assert!(!kinds.contains(&not), "{not:?} in {kinds:?}");
        }
    }

    fn listed(nodes: &[&str]) -> Vec<InputDevice> {
        nodes
            .iter()
            .map(|node| InputDevice {
                node: (*node).to_owned(),
                name: "USB Keyboard".to_owned(),
                phys: "usb-0000:00:14.0-1/input0".to_owned(),
                capabilities: keyboard(),
            })
            .collect()
    }

    fn arrivals(known: &[&str]) -> Arrivals {
        Arrivals {
            known: known.iter().map(|node| (*node).to_owned()).collect(),
            warned: BTreeSet::new(),
            last_id: 1,
            only: Only::default(),
        }
    }

    /// A scan sees each device once: what the start decided about isn't new, a device decided
    /// about isn't new again — and a node that goes and comes back, plugged in again, is.
    #[test]
    fn a_scan_sees_each_device_once_and_a_returning_one_again() {
        let mut arrivals = arrivals(&["event3"]);
        let nodes = |devices: Vec<InputDevice>| {
            devices
                .into_iter()
                .map(|device| device.node)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            nodes(arrivals.arrived(listed(&["event3", "event7"]))),
            ["event7"]
        );
        arrivals.known.insert("event7".to_owned());
        assert!(
            arrivals.arrived(listed(&["event3", "event7"])).is_empty(),
            "nothing new"
        );
        assert!(
            arrivals.arrived(listed(&["event3"])).is_empty(),
            "event7 unplugged"
        );
        assert_eq!(
            nodes(arrivals.arrived(listed(&["event3", "event7"]))),
            ["event7"],
            "plugged in again, it is new"
        );
    }

    /// What the rule leaves alone or gives the pointer thread — and, in the live tests, what
    /// they don't admit — is decided at once, never opened here, and never looked at again.
    #[test]
    fn an_arrival_left_alone_is_remembered_unopened() {
        let no_supervisor = Mutex::new(None);
        let mut arrivals = arrivals(&[]);
        let a_mouse = InputDevice {
            node: "event8".to_owned(),
            name: "Wireless Mouse".to_owned(),
            phys: "usb-0000:00:14.0-2/input0".to_owned(),
            capabilities: mouse(),
        };
        assert!(arrivals.take_in(a_mouse, &no_supervisor).is_none());
        assert!(arrivals.known.contains("event8"));

        arrivals.only = Only(Some("sequencer test ".to_owned()));
        let a_keyboard = listed(&["event9"]).remove(0);
        assert!(arrivals.take_in(a_keyboard, &no_supervisor).is_none());
        assert!(
            arrivals.known.contains("event9"),
            "not a device the tests admit"
        );
    }

    /// A reader ending on its own — its read failed — gives back everything it held, each
    /// once, in order.
    #[test]
    fn a_reader_ending_on_its_own_gives_back_all_it_held() {
        let running = AtomicBool::new(true);
        let given_back = std::cell::RefCell::new(Vec::new());
        drop(Holding::new(
            vec!["keyboard", "macro pad"],
            &running,
            |held| given_back.borrow_mut().push(held),
        ));
        assert_eq!(given_back.into_inner(), ["keyboard", "macro pad"]);
    }

    /// A reader ending with the grab gives back nothing: the supervisor has let everything
    /// go already.
    #[test]
    fn a_reader_ending_with_the_grab_gives_back_nothing() {
        let running = AtomicBool::new(false);
        let given_back = std::cell::RefCell::new(Vec::new());
        drop(Holding::new(vec!["keyboard"], &running, |held| {
            given_back.borrow_mut().push(held);
        }));
        assert!(given_back.into_inner().is_empty());
    }

    /// A reader that panics gives back everything it still held as it unwinds — and not
    /// what it had let go of before, as it does an unplugged device.
    #[test]
    fn a_reader_that_panics_gives_back_all_it_still_held() {
        let running = AtomicBool::new(true);
        let given_back = std::cell::RefCell::new(Vec::new());
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut holding = Holding::new(
                vec!["keyboard", "unplugged", "macro pad"],
                &running,
                |held| given_back.borrow_mut().push(held),
            );
            holding.remove(1);
            panic!("a bug in the reader");
        }));
        assert!(unwound.is_err());
        assert_eq!(given_back.into_inner(), ["keyboard", "macro pad"]);
    }

    /// A reader that has ended is what the next beat finds — the keyboards' first — and a
    /// reader still running is not.
    #[test]
    fn a_beat_finds_a_reader_that_ended() {
        let ended = std::thread::spawn(|| {});
        while !ended.is_finished() {
            std::thread::yield_now();
        }
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let running = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        assert!(matches!(
            reader_lost(Some(&ended), Some(&running)),
            Some(GrabLost::KeyboardReader)
        ));
        assert!(matches!(
            reader_lost(Some(&running), Some(&ended)),
            Some(GrabLost::MouseReader)
        ));
        assert!(reader_lost(Some(&running), None).is_none());
        assert!(reader_lost(None, None).is_none());
        drop(stop);
        running.join().unwrap();
        ended.join().unwrap();
    }

    /// Unlimited, the grab may take any device; limited by the live tests, only those whose
    /// names begin as the rig's do — never the user's, nor sequencer's own.
    #[test]
    fn the_live_tests_limit_the_grab_to_the_rigs_devices() {
        assert!(Only::default().admits("USB Keyboard"));
        let rig = Only(Some("sequencer test ".to_owned()));
        assert!(rig.admits("sequencer test keyboard"));
        assert!(rig.admits("sequencer test mouse"));
        assert!(!rig.admits("USB Keyboard"));
        assert!(!rig.admits(KEYBOARD_NAME));
    }

    /// A mouse's stand-in has the mouse's own name and everything it declares; its `phys` is
    /// all that tells it apart — and is enough for sequencer never to hold its own stand-in.
    #[test]
    fn a_stand_in_is_known_by_its_phys() {
        let mouse = InputDevice {
            node: "event5".to_owned(),
            name: "Wireless Mouse".to_owned(),
            phys: "usb-0000:00:14.0-2/input0".to_owned(),
            capabilities: mouse(),
        };
        assert_eq!(take_listed(&mouse), Take::Pointer(Kind::Mouse));
        let stand_in = InputDevice {
            node: "event21".to_owned(),
            phys: STAND_IN_PHYS.to_str().unwrap().to_owned(),
            ..mouse
        };
        assert_eq!(take_listed(&stand_in), Take::Leave(Left::Ours));
    }

    /// Each reason reads as a clause after a device's name, says which kind where it names
    /// one, and is no other reason's.
    #[test]
    fn each_reason_has_its_own_words() {
        let reasons = [
            Left::Ours,
            Left::NoKeys,
            Left::NotYet(Kind::Touchpad),
            Left::Switches,
            Left::Ignored,
            Left::System,
        ];
        // Exhaustive: a new reason stops this building, so it isn't forgotten above.
        for reason in reasons {
            match reason {
                Left::Ours
                | Left::NoKeys
                | Left::NotYet(_)
                | Left::Switches
                | Left::Ignored
                | Left::System => {}
            }
        }
        let words: BTreeSet<String> = reasons.iter().map(ToString::to_string).collect();
        assert_eq!(words.len(), reasons.len(), "{words:?}");
        assert!(
            Left::NotYet(Kind::Joystick)
                .to_string()
                .starts_with("a game controller")
        );
    }
}
