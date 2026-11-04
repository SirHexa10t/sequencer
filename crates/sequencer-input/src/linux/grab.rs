//! The device backend's grab: keyboards held exclusively, and every key they send routed —
//! on to the session through a virtual keyboard, or kept as a profile's trigger.
//!
//! An exclusive grab (`EVIOCGRAB`) is the one way below the display server to keep a key from
//! the session, which is what a profile's trigger needs on Wayland, where nothing above can
//! swallow one. Its price — the process sits between the user and their keyboard — is the one
//! [`super::capture`], a listener, declines to pay. Here it is paid, and made safe three ways:
//! every grab is taken and held by a supervisor process, which ends it if the manager stops
//! answering ([`super::supervise`]) — the manager only reads through its own descriptor of
//! the same open device; a device is grabbed only once no key is held on it, since a key
//! held across the grab would stay down in the session for good; and sequencer's own virtual
//! devices are never grabbed, so nothing it sends comes back as the user's input.
//!
//! What is grabbed, for now ([`take`]): every device udev counts as sending keys
//! ([`crate::devices`]), short of the kinds below — keyboards, a mouse's keyboard interface
//! (where the mouse's own macros type), macro pads, media remotes, volume and brightness keys.
//! Left alone, each for its own reason ([`Left`]): mice and pointing sticks, until mouse
//! buttons can be triggers — and then the stand-in must carry the mouse's identity, since the
//! desktop keeps pointer settings per device; touchpads, tablets, touchscreens, game
//! controllers and motion sensors, until the device-detection procedure handles them (a
//! touchpad passed on loses palm detection and gestures, and games read controllers
//! directly); a device that reports switches as well, since the virtual keyboard has no way to
//! pass a switch on; a device the session never reads, since passing it on would add input
//! that was never there; and power and sleep buttons, which are logind's.
//!
//! Devices are opened once, at start. A manager started through sudo has shed root by the
//! time a keyboard is plugged in, so it can't open one: a keyboard plugged in later — or one
//! that comes back from a suspend as a new device — works as usual, without profiles, until
//! the manager runs again.
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

use evdev::{Device, EventType, InputEvent as DeviceEvent, KeyCode, SynchronizationCode};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use sequencer_core::emit::{Emit, EmitAction, Holdable, InputSink, SinkError};
use sequencer_core::input::{InputEvent, Key, Mods};

use crate::capture::{Epoch, EventQueue};
use crate::devices::{Capabilities, Kind};
use crate::linux::inject::{DEVICE_NAME, UinputSink};
use crate::linux::keymap;
use crate::linux::route::{ClaimError, ClaimOwner, DeviceId, Route, Router};
use crate::linux::supervise::{GrabRefused, Supervisor};

/// The name of the virtual keyboard that stands in for the grabbed devices.
pub const KEYBOARD_NAME: &str = "sequencer virtual keyboard";

/// The environment variable naming the one device the grab may take, for the live tests: a
/// test manager then holds the rig's keyboard and nothing of the user's. Unset — as it is
/// everywhere but the live suite — every device [`take`] grabs is grabbed.
pub const GRAB_ONLY_VARIABLE: &str = "SEQUENCER_TEST_GRAB_ONLY";

/// How long a device may keep a key down before the grab gives up on it: longer than any
/// keystroke, so the Enter that started the manager is long released.
const IDLE_WITHIN: Duration = Duration::from_secs(10);

/// How often the reader wakes with nothing to read, to notice the grab ending.
const READ_POLL: Timespec = Timespec {
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
    /// A mouse or a pointing stick, which given: grabbed once mouse buttons can be triggers.
    Pointer(Kind),
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
            Self::Pointer(kind) => {
                write!(f, "{kind} — not held yet, so its buttons can't be triggers")
            }
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
    let left = if let Some(kind) = first(&NOT_YET) {
        Left::NotYet(kind)
    } else if let Some(kind) = first(&POINTERS) {
        Left::Pointer(kind)
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

/// Locks state the reader shares. A panic elsewhere while holding the lock leaves plain data
/// behind, still worth using: a grab must keep routing, not wedge the keyboard.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A device the grab takes, open.
#[derive(Debug)]
struct Taken {
    id: DeviceId,
    device: Device,
    name: String,
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
}

impl Opened {
    /// Opens every device the grab takes ([`take`]) — only the one [`GRAB_ONLY_VARIABLE`]
    /// names, when it names one — and the virtual keyboard, which declares every key the
    /// devices have so that each can be passed on.
    ///
    /// # Errors
    ///
    /// [`GrabError::NoDevices`] when nothing to grab could be opened, and
    /// [`GrabError::Uinput`] when the virtual keyboard could not be made.
    pub fn open() -> Result<Self, GrabError> {
        let only = std::env::var(GRAB_ONLY_VARIABLE).ok();
        let mut devices = Vec::new();
        let mut keys = BTreeSet::new();
        let mut number = 0;
        for (path, device) in evdev::enumerate() {
            let name = device.name().unwrap_or_default().to_owned();
            if only.as_deref().is_some_and(|only| only != name) {
                log::debug!("leaving device={name} why=not {GRAB_ONLY_VARIABLE}");
                continue;
            }
            let capabilities = Capabilities::of(&device);
            match take(&name, &capabilities) {
                Take::Grab { keyboard } => {
                    number += 1;
                    keys.extend(capabilities.key_codes().map(KeyCode));
                    log::debug!("grabbing device={name} path={}", path.display());
                    devices.push(Taken {
                        id: DeviceId(number),
                        device,
                        name,
                        keyboard,
                    });
                }
                Take::Leave(why) => log::debug!("leaving device={name} why={why:?}"),
            }
        }
        if devices.is_empty() {
            return Err(GrabError::NoDevices);
        }
        let out = UinputSink::open_as(KEYBOARD_NAME, keys).map_err(GrabError::Uinput)?;
        Ok(Self { devices, out })
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
        let shared = Arc::new(Mutex::new(Shared {
            router: Router::new(),
            out: self.out,
            stopping: false,
        }));
        let running = Arc::new(AtomicBool::new(true));
        let reader = {
            let (shared, running, epoch) =
                (Arc::clone(&shared), Arc::clone(&running), epoch.clone());
            std::thread::Builder::new()
                .name("sequencer-grab".into())
                .spawn(move || read(grabbed, &shared, &queue, &epoch, &running))
        };
        let Ok(reader) = reader else {
            // No reader, so no grab may stand: the supervisor lets every device go at once.
            supervisor.finish();
            return Err(GrabError::NoDevices);
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
            supervisor: Some(supervisor),
            owners: AtomicU32::new(0),
            held,
            holds_a_keyboard,
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

/// Ends the grab `device` holds, and closes it — quietly when the device has gone: evdev,
/// dropping a handle whose grab it couldn't end, would print that it failed, though an
/// unplugged device holds no grab. Such a handle is left open instead; it is a closing
/// process's, and goes with it.
pub(crate) fn let_go(mut device: Device) {
    if device.ungrab().is_err() {
        std::mem::forget(device);
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

/// What the reader and the manager share.
#[derive(Debug)]
struct Shared {
    router: Router,
    /// The virtual keyboard: everything passed on, and everything a profile sends.
    out: UinputSink,
    /// Set as the grab ends, before any device is let go: a frame read after that is the
    /// session's already, and passing it on would type it twice.
    stopping: bool,
}

/// Reads every grabbed device until the grab ends, routing what each sends.
fn read(
    mut devices: Vec<Taken>,
    shared: &Mutex<Shared>,
    queue: &EventQueue,
    epoch: &Epoch,
    running: &AtomicBool,
) {
    while running.load(Ordering::Relaxed) && !devices.is_empty() {
        let ready: Vec<usize> = {
            let mut fds: Vec<PollFd<'_>> = devices
                .iter()
                .map(|taken| PollFd::new(&taken.device, PollFlags::IN))
                .collect();
            match poll(&mut fds, Some(&READ_POLL)) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(err) => {
                    log::warn!("reading the grabbed devices failed, so they are let go: {err}");
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
                    route(taken.id, &events, shared, queue, epoch);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => gone.push(index),
            }
        }
        for index in gone.into_iter().rev() {
            let Taken { id, name, .. } = devices.remove(index);
            // Its held triggers come up for the manager, so no profile waits on a release
            // that can't arrive now.
            for (kind, mods) in lock(shared).router.forget(id) {
                let _ = queue.offer(InputEvent::physical(epoch.now(), kind).with_mods(mods));
            }
            log::warn!("{name} went away; profiles don't apply to it until the manager runs again");
        }
    }
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
    let mut frame = Vec::new();
    for event in events {
        if event.event_type() == EventType::SYNCHRONIZATION {
            // The frame ends here; the virtual keyboard's own report closes it there.
            if event.code() == SynchronizationCode::SYN_REPORT.0 && !frame.is_empty() {
                let _ = shared.out.forward(&frame);
                frame.clear();
            }
            continue;
        }
        if event.event_type() != EventType::KEY {
            frame.push(*event);
            continue;
        }
        match shared
            .router
            .route(device, KeyCode(event.code()), event.value())
        {
            Route::Forward => frame.push(*event),
            Route::Report(kind, mods) => {
                if !queue.offer(InputEvent::physical(epoch.now(), kind).with_mods(mods)) {
                    log::trace!("trigger queue full, event dropped");
                }
            }
            Route::Keep => {}
        }
    }
    if !frame.is_empty() {
        let _ = shared.out.forward(&frame);
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
    /// Holds every grab; the one that can end them.
    supervisor: Option<Supervisor>,
    owners: AtomicU32,
    /// The names of the devices grabbed, in the order they were.
    held: Vec<String>,
    /// Whether a full keyboard is among them.
    holds_a_keyboard: bool,
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
    pub fn claim(&self, chords: &[Vec<Key>]) -> Result<DeviceClaim, ClaimError> {
        let owner = ClaimOwner(self.owners.fetch_add(1, Ordering::Relaxed));
        lock(&self.shared).router.claim(owner, chords)?;
        Ok(DeviceClaim {
            owner,
            shared: Some(Arc::clone(&self.shared)),
        })
    }

    /// The sink a profile sends through: the virtual keyboard, in order with what is passed
    /// on.
    #[must_use]
    pub fn sink(&self) -> DeviceSink {
        DeviceSink {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Whether any of `keys` is down on a grabbed device.
    #[must_use]
    pub fn any_down(&self, keys: &[Key]) -> bool {
        lock(&self.shared).router.any_down(keys)
    }

    /// Taps `targets` clean: held modifiers of `held`'s classes the targets don't name come up
    /// first and go back down after — those still held by then — all on the virtual keyboard,
    /// in one go, so nothing the hand does lands in between. `false` when a target has no
    /// code, or the virtual keyboard refused.
    #[must_use]
    pub fn tap_lifted(&self, targets: &[Holdable], held: Mods, duration: Duration) -> bool {
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
        let downs: Vec<DeviceEvent> = codes.iter().map(|code| key(*code, 1)).collect();
        let ups: Vec<DeviceEvent> = codes.iter().rev().map(|code| key(*code, 0)).collect();
        let mut sent = (lifts.is_empty() || shared.out.forward(&lifts).is_ok())
            && shared.out.forward(&downs).is_ok();
        std::thread::sleep(duration);
        sent &= shared.out.forward(&ups).is_ok();
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
    /// The supervisor is gone — it gave the keyboards back — and the manager must stop.
    pub fn beat(&self) -> io::Result<()> {
        self.supervisor.as_ref().map_or(Ok(()), Supervisor::beat)
    }

    /// Ends the grab: nothing more is passed on, the supervisor lets every device go back to
    /// the session — at once, holding every grab — and the virtual keyboard lets go of what
    /// profiles held. Stopping twice does nothing the second time.
    pub fn stop(&mut self) {
        let Some(supervisor) = self.supervisor.take() else {
            return;
        };
        lock(&self.shared).stopping = true;
        supervisor.finish();
        self.running.store(false, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        lock(&self.shared).out.release_all();
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
}

impl DeviceClaim {
    /// Lets the triggers go: their presses pass on to the session again. A trigger that is
    /// down stays kept until it comes up. Stopping twice does nothing the second time.
    pub fn stop(&mut self) {
        if let Some(shared) = self.shared.take() {
            lock(&shared).router.release(self.owner);
        }
    }
}

impl Drop for DeviceClaim {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A profile's sink: the virtual keyboard the grabbed devices are passed on through.
#[derive(Debug)]
pub struct DeviceSink {
    shared: Arc<Mutex<Shared>>,
}

impl InputSink for DeviceSink {
    fn emit(&mut self, emit: &Emit) -> Result<(), SinkError> {
        let mut shared = lock(&self.shared);
        // Locked, only releases go out: nothing types into the lock screen, and nothing
        // pressed before the lock is left down.
        if shared.router.paused()
            && !matches!(emit.action, EmitAction::KeyUp(_) | EmitAction::ButtonUp(_))
        {
            return Ok(());
        }
        shared.out.emit(emit)
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        lock(&self.shared).out.flush()
    }

    fn release_all(&mut self) {
        lock(&self.shared).out.release_all();
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
        assert_eq!(
            take("Wireless Mouse", &mouse()),
            Take::Leave(Left::Pointer(Kind::Mouse))
        );
        let many_buttons = Capabilities::default()
            .with_keys(KeyCode::BTN_LEFT.0..KeyCode::BTN_LEFT.0 + 32)
            .with_relative([RelativeAxisCode::REL_X.0, RelativeAxisCode::REL_Y.0]);
        assert_eq!(
            take("Many-Button Mouse", &many_buttons),
            Take::Leave(Left::Pointer(Kind::Mouse)),
            "32 buttons are a mouse's, not a controller's"
        );
        let stick = mouse().with_properties([PropType::POINTING_STICK.0]);
        assert_eq!(
            take("TrackPoint", &stick),
            Take::Leave(Left::Pointer(Kind::PointingStick))
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

    /// Each reason reads as a clause after a device's name, says which kind where it names
    /// one, and is no other reason's.
    #[test]
    fn each_reason_has_its_own_words() {
        let reasons = [
            Left::Ours,
            Left::NoKeys,
            Left::Pointer(Kind::Mouse),
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
                | Left::Pointer(_)
                | Left::NotYet(_)
                | Left::Switches
                | Left::Ignored
                | Left::System => {}
            }
        }
        let words: BTreeSet<String> = reasons.iter().map(ToString::to_string).collect();
        assert_eq!(words.len(), reasons.len(), "{words:?}");
        assert!(
            Left::Pointer(Kind::PointingStick)
                .to_string()
                .starts_with("a pointing stick")
        );
        assert!(
            Left::NotYet(Kind::Joystick)
                .to_string()
                .starts_with("a game controller")
        );
    }
}
