//! The mice a profile's triggers name, held through stand-ins of their own.
//!
//! A mouse is grabbed only while some applied profile claims one of its buttons as a trigger,
//! and let go again once none does (developer's call, 2026-11-04: don't reach for a device no
//! profile names). Held, it reaches the session through a stand-in: a virtual device with the
//! mouse's own name, IDs and everything it declares, marked by its `phys` ([`STAND_IN_PHYS`]),
//! the one thing that tells sequencer its own from the mouse.
//!
//! What the identity carries over is what is matched by name and IDs: the desktop's settings
//! for that mouse (KDE keeps pointer speed, scroll direction and handedness per device),
//! libinput's quirks for it, and udev's verdict on its kind. What it can't is udev's mouse
//! database — the DPI and wheel click angle libinput scales motion and scrolling by — which
//! udev consults only for a device on a USB or Bluetooth bus. A stand-in is on neither; udev
//! takes a Bluetooth mouse's bus from its bus number, which the stand-in copies, but a USB
//! one's from the USB device above it, which a stand-in has none of. So a USB mouse listed
//! there moves at libinput's default scale while it is held (systemd's
//! `60-persistent-input.rules` and `70-mouse.rules`, read 2026-11-04) — unless the optional
//! udev rule shipped in `udev/69-sequencer-stand-in.rules` is installed, which gives a USB
//! mouse's stand-in its bus and leaves the lookup to udev (developer's call, 2026-11-04). It
//! knows a stand-in by [`STAND_IN_PHYS`], and a test keeps the two, and the README's copy,
//! alike.
//!
//! Mice run on a thread of their own, apart from the keyboards': motion goes straight to the
//! stand-in, and only a button asks the router — under the lock a lifted tap holds for its
//! whole length — so a tap never stalls the pointer (developer's call). A mouse's buttons go
//! to its own stand-in, never the virtual keyboard, so the router counts it apart
//! ([`Router::pass_alone`](crate::linux::route::Router::pass_alone)).
//!
//! Mice are found as keyboards plugged in later are: through sysfs, about once a second
//! ([`SCAN_EVERY`]), and at once when the wanted buttons change. They are opened as the user
//! — a manager started through sudo has shed root by then, keeping the `input` group — and
//! their stand-ins are made through `/dev/uinput` the same way. Where either is root's
//! alone, mice can't be held, and the log says so once — for `/dev/uinput`, naming the
//! uinput rule that fixes it. Each is taken in the order that loses nothing it sends
//! (`take_in_order`).

use std::collections::BTreeSet;
use std::io;
use std::os::fd::AsFd as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AbsInfo, Device, InputEvent as DeviceEvent, KeyCode, UinputAbsSetup};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use sequencer_core::input::InputEvent;

use crate::capture::{Epoch, EventQueue};
use crate::devices::{self, InputDevice};
use crate::linux::grab::{
    Holding, Only, READ_POLL, SCAN_EVERY, STAND_IN_PHYS, Shared, Take, lock, take_listed,
};
use crate::linux::route::{DeviceId, Passed, sort_batch};
use crate::linux::supervise::{GrabRefused, Supervisor};

/// Where the mice's numbers start: far above any keyboard's, which count up from one.
const POINTER_IDS: u32 = 1 << 31;

/// What the pointer thread works with.
pub(super) struct Ends {
    pub(super) shared: Arc<Mutex<Shared>>,
    pub(super) queue: EventQueue,
    pub(super) epoch: Epoch,
    pub(super) running: Arc<AtomicBool>,
    pub(super) supervisor: Arc<Mutex<Option<Supervisor>>>,
    /// The buttons claimed as triggers now, kept current by every claim and its release.
    pub(super) wanted: Arc<Mutex<BTreeSet<KeyCode>>>,
    /// The devices the grab may take.
    pub(super) only: Only,
}

/// Holds the mice whose buttons are wanted, and passes each one on through its stand-in,
/// until the grab ends. Stand-ins go with the thread.
pub(super) fn run(ends: &Ends) {
    let mut mice = Mice {
        held: Holding::new(Vec::new(), &ends.running, |mouse: Held| {
            log::warn!(
                "{} is given back to the session: its reader ended",
                mouse.name
            );
            let_go(mouse, ends);
        }),
        wanted: BTreeSet::new(),
        looked: None,
        warned: BTreeSet::new(),
        last_id: POINTER_IDS,
    };
    while ends.running.load(Ordering::Relaxed) {
        let wanted = lock(&ends.wanted).clone();
        if wanted != mice.wanted || mice.looked.is_none_or(|at| at.elapsed() >= SCAN_EVERY) {
            mice.look(wanted, ends);
        }
        mice.read(ends);
    }
}

/// A mouse held: the device, read here; its stand-in, written here.
struct Held {
    id: DeviceId,
    device: Device,
    stand_in: VirtualDevice,
    name: String,
    node: String,
    /// The buttons it has, as the wanted ones are matched against.
    buttons: BTreeSet<KeyCode>,
}

/// The mice held, and what the thread keeps between two looks for them.
struct Mice<'a> {
    /// Given back should the thread end before the grab does: a panic.
    held: Holding<'a, Held>,
    /// The buttons wanted at the last look: a change is looked at at once.
    wanted: BTreeSet<KeyCode>,
    looked: Option<Instant>,
    /// Nodes already warned about, so a mouse that stays out of reach is said once.
    warned: BTreeSet<String>,
    /// The last number given to a mouse.
    last_id: u32,
}

/// Whether a mouse with `buttons` has one of the `wanted` ones.
fn named(buttons: &BTreeSet<KeyCode>, wanted: &BTreeSet<KeyCode>) -> bool {
    !buttons.is_disjoint(wanted)
}

impl Mice<'_> {
    /// Lets go of the mice no profile names any longer, and holds the ones that newly are —
    /// plugged in since, or named since.
    fn look(&mut self, wanted: BTreeSet<KeyCode>, ends: &Ends) {
        self.wanted = wanted;
        self.looked = Some(Instant::now());
        let (kept, unwanted) = std::mem::take(&mut *self.held)
            .into_iter()
            .partition(|mouse| named(&mouse.buttons, &self.wanted));
        *self.held = kept;
        for mouse in unwanted {
            log::info!(
                "letting go of device={}: no profile names its buttons",
                mouse.name
            );
            let_go(mouse, ends);
        }
        if self.wanted.is_empty() {
            return;
        }
        let listed = devices::list();
        self.warned
            .retain(|node| listed.iter().any(|device| device.node == *node));
        for device in listed {
            if let Some(mouse) = self.take_in(device, ends) {
                lock(&ends.shared).router.pass_alone(mouse.id);
                log::info!("holding device={} for its buttons", mouse.name);
                self.held.push(mouse);
            }
        }
    }

    /// `device`, held — opened, grabbed under the supervisor, given its stand-in — when it
    /// is a mouse not held yet, with a wanted button and none down; `None` otherwise, said
    /// in the log once per device when something stood in the way. A mouse with a button
    /// down is tried again at the next look.
    fn take_in(&mut self, device: InputDevice, ends: &Ends) -> Option<Held> {
        if self.held.iter().any(|mouse| mouse.node == device.node)
            || !matches!(take_listed(&device), Take::Pointer(_))
            || !ends.only.admits(&device.name)
        {
            return None;
        }
        let buttons: BTreeSet<KeyCode> = device.capabilities.key_codes().map(KeyCode).collect();
        if !named(&buttons, &self.wanted) {
            return None;
        }
        let mut warn_once = |what: String| {
            if self.warned.insert(device.node.clone()) {
                log::warn!("{what}");
            }
        };
        let opened = match Device::open(device.path()) {
            Ok(opened) => opened,
            Err(err) => {
                warn_once(format!(
                    "{} can't be opened ({err}), so its buttons can't be triggers",
                    device.name
                ));
                return None;
            }
        };
        // A button held as the grab takes effect would stay down in the session for good.
        if opened
            .get_key_state()
            .is_ok_and(|keys| keys.iter().next().is_some())
        {
            return None;
        }
        if let Err(err) = opened.set_nonblocking(true) {
            warn_once(format!(
                "{} could not be read without blocking, so it was left alone: {err}",
                device.name
            ));
            return None;
        }
        let taken = take_in_order(
            || prepare_stand_in(&opened),
            || match lock(&ends.supervisor).as_ref() {
                Some(supervisor) => supervisor.grab(opened.as_fd()),
                None => Err(GrabRefused::Gone(io::Error::other("the grab is ending"))),
            },
            VirtualDeviceBuilder::build,
            || {
                if let Some(supervisor) = lock(&ends.supervisor).as_ref() {
                    let _ = supervisor.release(opened.as_fd());
                }
            },
        );
        let stand_in = match taken {
            Ok(stand_in) => stand_in,
            Err(not_taken) => {
                if let Some(said) = not_taken.said_of(&device.name) {
                    warn_once(said);
                }
                return None;
            }
        };
        self.last_id += 1;
        Some(Held {
            id: DeviceId(self.last_id),
            device: opened,
            stand_in,
            name: device.name,
            node: device.node,
            buttons,
        })
    }

    /// Reads every mouse held, passing each frame on through its stand-in; a mouse gone —
    /// unplugged, or its wireless link dropped — is let go, and held again if it comes back
    /// while a profile still names it.
    fn read(&mut self, ends: &Ends) {
        let ready: Vec<usize> = {
            let mut fds: Vec<PollFd<'_>> = self
                .held
                .iter()
                .map(|mouse| PollFd::new(&mouse.device, PollFlags::IN))
                .collect();
            // With no mouse held, this is the wait until the next look.
            match poll(&mut fds, Some(&READ_POLL)) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(err) => {
                    log::warn!("reading the mice failed: {err}");
                    std::thread::sleep(SCAN_EVERY);
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
            let mouse = &mut self.held[index];
            let fetched: io::Result<Vec<DeviceEvent>> =
                mouse.device.fetch_events().map(Iterator::collect);
            match fetched {
                Ok(events) => pass_on(mouse, &events, ends),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => gone.push(index),
            }
        }
        for index in gone.into_iter().rev() {
            let mouse = self.held.remove(index);
            log::warn!(
                "{} went away; it is held again if it comes back",
                mouse.name
            );
            let_go(mouse, ends);
        }
    }
}

/// Lets `mouse` go: the supervisor ends its grab and closes its handle, the router forgets
/// it — a trigger it held down comes up for the manager — and its stand-in goes, with
/// whatever it held down.
fn let_go(mouse: Held, ends: &Ends) {
    if let Some(supervisor) = lock(&ends.supervisor).as_ref()
        && let Err(err) = supervisor.release(mouse.device.as_fd())
    {
        log::warn!("{} could not be let go: {err}", mouse.name);
    }
    let forgotten = lock(&ends.shared).router.forget(mouse.id);
    for (kind, mods) in forgotten.reports {
        let _ = ends
            .queue
            .offer(InputEvent::physical(ends.epoch.now(), kind).with_mods(mods));
    }
    // Last, so that a button the stand-in holds comes up once the mouse is the session's.
    drop(mouse);
}

/// The name a stand-in takes: its mouse's own, so the desktop's settings for that mouse
/// apply — or a plain one for a device that reports none, since uinput refuses to make a
/// device without a name. A USB mouse that names nothing itself is named by the kernel,
/// "HID vvvv:pppp" from its IDs (`usbhid`), so a real one rarely lands here.
fn stand_in_name(name: Option<&str>) -> &str {
    name.filter(|name| !name.is_empty()).unwrap_or("mouse")
}

/// Why a mouse wasn't taken.
#[derive(Debug)]
enum NotTaken {
    /// Its stand-in can't be had — refused before the mouse was ever grabbed.
    NoStandIn(io::Error),
    /// The grab was refused.
    Refused(GrabRefused),
    /// The stand-in failed as it was made, after the grab, which was let go again.
    Unmade(io::Error),
}

impl NotTaken {
    /// What the log says of `name`, not taken — nothing while the grab is ending.
    fn said_of(&self, name: &str) -> Option<String> {
        Some(match self {
            Self::NoStandIn(err) if err.kind() == io::ErrorKind::PermissionDenied => format!(
                "{name}'s buttons can't be triggers: its stand-in is made through /dev/uinput, \
                 which this manager can't open — it let root go once the devices were open, \
                 keeping your user and the `input` group alone — and the uinput rule in the \
                 README's setup (step 3) is what gives that group the node"
            ),
            Self::NoStandIn(err) => {
                format!("{name} could not be given a stand-in, so it was left alone: {err}")
            }
            Self::Refused(GrabRefused::Busy) => {
                format!("{name} is held by another program, so its buttons can't be triggers")
            }
            Self::Refused(GrabRefused::Gone(_)) => return None,
            Self::Refused(err) => format!("{name} could not be grabbed: {err}"),
            Self::Unmade(err) => {
                format!("{name} could not be given a stand-in, so it was let go again: {err}")
            }
        })
    }
}

/// Takes a mouse in the one order that loses nothing it sends: its stand-in `prepare`d
/// first — opened through `/dev/uinput` and given all it will be, none of which the system
/// sees yet — so that a stand-in that can't be had is refused before the mouse is grabbed;
/// then the `grab`; then the stand-in `make`, which the system sees only now, the grab let
/// go again (`release`) should that fail. Grabbing first and letting go a moment later, as
/// every look once did where no stand-in could be made, lost what the mouse sent meanwhile
/// — a button's release among it, which left the session holding the button (found
/// 2026-11-05). And a mouse another program holds never has a stand-in made and unmade.
fn take_in_order<Prepared, StandIn>(
    prepare: impl FnOnce() -> io::Result<Prepared>,
    grab: impl FnOnce() -> Result<(), GrabRefused>,
    make: impl FnOnce(Prepared) -> io::Result<StandIn>,
    release: impl FnOnce(),
) -> Result<StandIn, NotTaken> {
    let prepared = prepare().map_err(NotTaken::NoStandIn)?;
    grab().map_err(NotTaken::Refused)?;
    make(prepared).map_err(|err| {
        release();
        NotTaken::Unmade(err)
    })
}

/// Everything the stand-in for `mouse` will be — its name, its IDs, everything it declares,
/// absolute axes with their ranges — marked by [`STAND_IN_PHYS`] alone, and not yet made:
/// the system sees nothing of it until `build`.
fn prepare_stand_in(mouse: &Device) -> io::Result<VirtualDeviceBuilder<'_>> {
    let mut builder = VirtualDevice::builder()?
        .name(stand_in_name(mouse.name()))
        .input_id(mouse.input_id())
        .with_phys(STAND_IN_PHYS)?
        .with_properties(mouse.properties())?;
    if let Some(keys) = mouse.supported_keys() {
        builder = builder.with_keys(keys)?;
    }
    if let Some(axes) = mouse.supported_relative_axes() {
        builder = builder.with_relative_axes(axes)?;
    }
    if let Some(misc) = mouse.misc_properties() {
        builder = builder.with_msc(misc)?;
    }
    if let Some(axes) = mouse.supported_absolute_axes() {
        let state = mouse.get_abs_state()?;
        for axis in axes {
            let info = state[usize::from(axis.0)];
            let range = AbsInfo::new(
                info.value,
                info.minimum,
                info.maximum,
                info.fuzz,
                info.flat,
                info.resolution,
            );
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, range))?;
        }
    }
    Ok(builder)
}

/// One batch from `mouse` on to its stand-in, frame by frame: motion and the wheel as they
/// came; each button asked of the router — kept and reported when a profile claims it —
/// under the shared lock, taken just for the asking.
fn pass_on(mouse: &mut Held, events: &[DeviceEvent], ends: &Ends) {
    if !ends.running.load(Ordering::Relaxed) {
        return;
    }
    let sorted = sort_batch(events, |code, value| {
        let mut shared = lock(&ends.shared);
        (!shared.stopping).then(|| shared.router.route(mouse.id, code, value))
    });
    for passed in sorted {
        match passed {
            Passed::Frame(frame) => {
                let _ = mouse.stand_in.emit(&frame);
            }
            Passed::Report(kind, mods) => {
                let reported = InputEvent::physical(ends.epoch.now(), kind);
                if !ends.queue.offer(reported.with_mods(mods)) {
                    log::trace!("trigger queue full, a button press dropped");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(codes: &[KeyCode]) -> BTreeSet<KeyCode> {
        codes.iter().copied().collect()
    }

    /// The optional udev rule knows a stand-in by the `phys` sequencer gives it, puts a USB
    /// mouse's stand-in on its bus and nothing more — and the README hands out the same rule.
    #[test]
    fn the_udev_rule_knows_a_stand_in_by_its_phys_and_the_readme_hands_it_out() {
        let file = include_str!("../../../../udev/69-sequencer-stand-in.rules");
        let rules: Vec<&str> = file
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .collect();
        let [rule] = rules[..] else {
            panic!("one rule, not {rules:?}");
        };
        let phys = STAND_IN_PHYS.to_str().unwrap();
        assert!(
            rule.contains(&format!("ATTRS{{phys}}==\"{phys}\"")),
            "{rule}"
        );
        assert!(rule.contains("ATTRS{id/bustype}==\"0003\""), "{rule}");
        assert!(rule.ends_with("ENV{ID_BUS}=\"usb\""), "{rule}");
        assert!(
            include_str!("../../../../README.md").contains(rule),
            "the README's copy of the rule differs from the file"
        );
    }

    /// A device made with the stand-ins' `phys` carries it, as sysfs shows it to the grab and
    /// to udev. evdev 0.13.2 sent that request with the wrong size and the kernel refused
    /// it, so no stand-in could be made (found live, 2026-11-04; see the evdev pin in the
    /// workspace manifest). Needs `/dev/uinput` alone: the device is never opened.
    #[test]
    fn a_stand_in_carries_its_phys() {
        if !crate::linux::inject::uinput_available() {
            eprintln!("skipping: /dev/uinput is not writable here");
            return;
        }
        let mut buttons = evdev::AttributeSet::<KeyCode>::new();
        buttons.insert(KeyCode::BTN_SIDE);
        let mut device = VirtualDevice::builder()
            .expect("/dev/uinput opens")
            .name("sequencer phys check")
            .with_phys(STAND_IN_PHYS)
            .expect("the kernel takes the stand-ins' phys")
            .with_keys(&buttons)
            .expect("a button")
            .build()
            .expect("the device is made");
        let syspath = device.get_syspath().expect("its sysfs entry");
        let phys = std::fs::read_to_string(syspath.join("phys")).expect("its phys");
        assert_eq!(phys.trim_end(), STAND_IN_PHYS.to_str().unwrap());
    }

    /// The steps `take_in_order` ran, in order, and whether the mouse was taken.
    fn steps(
        prepare: io::Result<()>,
        grab: Result<(), GrabRefused>,
        make: io::Result<()>,
    ) -> (Vec<&'static str>, bool) {
        let ran = std::cell::RefCell::new(Vec::new());
        let taken = take_in_order(
            || {
                ran.borrow_mut().push("prepare");
                prepare
            },
            || {
                ran.borrow_mut().push("grab");
                grab
            },
            |()| {
                ran.borrow_mut().push("make");
                make
            },
            || ran.borrow_mut().push("release"),
        );
        (ran.into_inner(), taken.is_ok())
    }

    /// A mouse is taken in order: its stand-in prepared, the mouse grabbed, the stand-in
    /// made — and nothing let go.
    #[test]
    fn a_mouse_is_taken_stand_in_first_grab_second() {
        assert_eq!(
            steps(Ok(()), Ok(()), Ok(())),
            (vec!["prepare", "grab", "make"], true)
        );
    }

    /// A stand-in that can't be had is refused before the mouse is grabbed at all: nothing
    /// the mouse sends is ever caught and lost.
    #[test]
    fn a_stand_in_that_cant_be_had_is_refused_before_any_grab() {
        let refused = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(
            steps(Err(refused), Ok(()), Ok(())),
            (vec!["prepare"], false)
        );
    }

    /// A grab refused — the mouse another program's — makes no stand-in: the prepared one
    /// was never shown to the system.
    #[test]
    fn a_refused_grab_makes_no_stand_in() {
        assert_eq!(
            steps(Ok(()), Err(GrabRefused::Busy), Ok(())),
            (vec!["prepare", "grab"], false)
        );
    }

    /// A stand-in failing as it is made lets the grab go again at once.
    #[test]
    fn a_stand_in_failing_as_it_is_made_lets_the_grab_go() {
        let failed = io::Error::from(io::ErrorKind::InvalidInput);
        assert_eq!(
            steps(Ok(()), Ok(()), Err(failed)),
            (vec!["prepare", "grab", "make", "release"], false)
        );
    }

    /// A stand-in refused for want of `/dev/uinput` names the fix; a mouse another program
    /// holds says so; and nothing is said while the grab is ending.
    #[test]
    fn a_mouse_not_taken_says_why() {
        let denied = NotTaken::NoStandIn(io::Error::from(io::ErrorKind::PermissionDenied));
        let said = denied.said_of("Wireless Mouse").unwrap();
        assert!(
            said.starts_with("Wireless Mouse's buttons can't be triggers")
                && said.contains("/dev/uinput")
                && said.contains("uinput rule")
                && said.contains("step 3"),
            "{said}"
        );
        let busy = NotTaken::Refused(GrabRefused::Busy).said_of("Wireless Mouse");
        assert!(busy.is_some_and(|said| said.contains("another program")));
        let ending = NotTaken::Refused(GrabRefused::Gone(io::Error::other("the grab is ending")));
        assert_eq!(ending.said_of("Wireless Mouse"), None);
    }

    /// A stand-in takes its mouse's own name, however plain — and one that reports none still
    /// gets a stand-in, under a name of its own.
    #[test]
    fn a_stand_in_is_named_as_its_mouse_or_plainly() {
        assert_eq!(stand_in_name(Some("HID 1d57:fa60")), "HID 1d57:fa60");
        assert_eq!(stand_in_name(Some("")), "mouse");
        assert_eq!(stand_in_name(None), "mouse");
    }

    /// A mouse is wanted while a profile names one of its own buttons — a side button on a
    /// mouse without one names nothing there, and no claim at all wants every mouse let go.
    #[test]
    fn a_mouse_is_wanted_for_its_own_buttons_alone() {
        let three_buttons = codes(&[KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE]);
        let with_sides = codes(&[KeyCode::BTN_LEFT, KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA]);
        let side = codes(&[KeyCode::BTN_SIDE]);
        assert!(named(&with_sides, &side));
        assert!(!named(&three_buttons, &side), "it has no side button");
        assert!(!named(&with_sides, &BTreeSet::new()), "nothing claimed");
    }
}
