//! What the profile manager runs on: the grabbed devices where it can, X11 key grabs where it
//! must.
//!
//! The device backend comes first in any session it can open in (developer's call,
//! 2026-11-03): it alone keeps keys as positions in a Wayland session, tells left from right,
//! holds a trigger and keeps a key from every app, and it does all of that in an X11 session
//! too. X11 grabs are the fallback when the devices can't be opened — through Xwayland in a
//! Wayland session, so native Wayland apps see only part of a profile there.
//!
//! The manager sees one shape whichever runs: a [`Grab`] per set of triggers, one [`Sink`],
//! and the backend's own answers to "is that key down?" and "tap this clean".

use sequencer_core::emit::{Emit, Holdable, InputSink, SinkError};
use sequencer_core::input::{Key, Mods};
use sequencer_core::time::Duration;
#[cfg(feature = "evdev")]
use sequencer_input::linux::grab::{DeviceClaim, DeviceGrab, DeviceSink, GrabError, Opened};
use sequencer_input::{Epoch, EventQueue, GrabCapture, KeyProbe, LiftedTap, XTestSink};

use crate::{Error, Result};

/// The backend a manager runs on.
#[derive(Debug)]
pub(super) enum Backend {
    /// X11 key grabs and XTEST: no device access needed.
    X11 {
        /// "Is that key still down?", asked of the X server.
        probe: Option<Box<KeyProbe>>,
        /// The clean tap between lifted modifiers.
        lifter: Option<Box<LiftedTap>>,
    },
    /// The grabbed devices, re-emitted through a virtual keyboard.
    #[cfg(feature = "evdev")]
    Devices(DeviceGrab),
}

/// One live grab of some triggers, from whichever backend took it.
#[derive(Debug)]
pub(super) enum Grab {
    X11(GrabCapture),
    #[cfg(feature = "evdev")]
    Devices(DeviceClaim),
}

impl Grab {
    /// Lets the triggers go.
    pub(super) fn stop(&mut self) {
        match self {
            Self::X11(capture) => capture.stop(),
            #[cfg(feature = "evdev")]
            Self::Devices(claim) => claim.stop(),
        }
    }
}

/// The sink profiles send through, from whichever backend runs.
#[derive(Debug)]
pub(super) enum Sink {
    X11(Box<XTestSink>),
    #[cfg(feature = "evdev")]
    Devices(DeviceSink),
}

impl InputSink for Sink {
    fn emit(&mut self, emit: &Emit) -> std::result::Result<(), SinkError> {
        match self {
            Self::X11(sink) => sink.emit(emit),
            #[cfg(feature = "evdev")]
            Self::Devices(sink) => sink.emit(emit),
        }
    }

    fn flush(&mut self) -> std::result::Result<(), SinkError> {
        match self {
            Self::X11(sink) => sink.flush(),
            #[cfg(feature = "evdev")]
            Self::Devices(sink) => sink.flush(),
        }
    }

    fn release_all(&mut self) {
        match self {
            Self::X11(sink) => sink.release_all(),
            #[cfg(feature = "evdev")]
            Self::Devices(sink) => sink.release_all(),
        }
    }
}

/// Why the devices weren't used, when they weren't: said once at start, since in a Wayland
/// session it is the difference between a whole profile and part of one.
pub(super) type Fallback = Option<String>;

/// Devices a privileged run opened before shedding root, for the manager it goes on to be.
#[derive(Debug, Default)]
pub(super) struct Preopened {
    #[cfg(feature = "evdev")]
    devices: Option<Opened>,
}

/// Opens the devices while this run is root — session mode's sudo run — and sheds root at
/// once, before anything else happens, so that nothing of the user's is ever written as
/// root. A run that isn't root opens nothing here: the manager opens what it can as the
/// user.
///
/// # Errors
///
/// Root could not be shed: the run is refused rather than go on as root.
#[cfg_attr(
    not(feature = "evdev"),
    allow(
        clippy::unnecessary_wraps,
        reason = "only a run that opens the devices has root to shed"
    )
)]
pub(super) fn open_privileged() -> Result<Preopened> {
    #[cfg(feature = "evdev")]
    {
        if !crate::elevate::is_root() {
            return Ok(Preopened::default());
        }
        let devices = Opened::open().ok();
        crate::elevate::drop_root_after_open()?;
        Ok(Preopened { devices })
    }
    #[cfg(not(feature = "evdev"))]
    Ok(Preopened::default())
}

/// Opens the backend this session gets — the devices when they open, else X11 — with its
/// sink, and why the devices weren't used when they weren't. `preopened` holds what a
/// privileged run opened already. Triggers it reports go to `queue`, on `epoch`'s timeline.
///
/// # Errors
///
/// Neither backend can run: no device access and no X server either.
#[cfg_attr(
    not(feature = "evdev"),
    allow(
        unused_variables,
        clippy::needless_pass_by_value,
        reason = "only the device backend takes the queue and the opened devices at start"
    )
)]
pub(super) fn open(
    epoch: &Epoch,
    queue: &EventQueue,
    preopened: Preopened,
) -> Result<(Backend, Sink, Fallback)> {
    #[cfg(feature = "evdev")]
    let fallback = match devices(epoch, queue, preopened.devices) {
        Ok((grab, sink)) => return Ok((Backend::Devices(grab), Sink::Devices(sink), None)),
        Err(why) => Some(why),
    };
    #[cfg(not(feature = "evdev"))]
    let fallback = None;
    if !sequencer_input::x11::is_usable() {
        return Err(Error::NotImplemented(format!(
            "the profile manager needs the input devices or an X11 session{}",
            fallback
                .as_ref()
                .map(|why| format!(" ({why})"))
                .unwrap_or_default()
        )));
    }
    let backend = Backend::X11 {
        probe: KeyProbe::open().map(Box::new),
        lifter: LiftedTap::open().map(Box::new),
    };
    Ok((backend, Sink::X11(Box::new(XTestSink::open()?)), fallback))
}

/// The device backend, started: the devices `opened` already, or opened now as the user;
/// root shed, if still held; the supervisor started as the user; the grab taken under it.
/// `Err` says why it couldn't be, for the fallback's note.
#[cfg(feature = "evdev")]
fn devices(
    epoch: &Epoch,
    queue: &EventQueue,
    opened: Option<Opened>,
) -> std::result::Result<(DeviceGrab, DeviceSink), String> {
    let opened = match opened {
        Some(opened) => opened,
        None => Opened::open().map_err(|err| match err {
            GrabError::NoDevices => "no access to the input devices".to_owned(),
            other => other.to_string(),
        })?,
    };
    crate::elevate::drop_root_after_open().map_err(|err| err.to_string())?;
    let supervisor = crate::supervisor::start()
        .map_err(|err| format!("no supervisor to hold the keyboards: {err}"))?;
    log::info!("grabbing devices={:?}", opened.names());
    let grab = opened
        .start(supervisor, epoch, queue.clone())
        .map_err(|err| err.to_string())?;
    let sink = grab.sink();
    Ok((grab, sink))
}

impl Backend {
    /// Takes `chords` as triggers, their presses fed to `queue`.
    ///
    /// # Errors
    ///
    /// Why the backend refused: a chord it can't take, or one another program holds.
    pub(super) fn grab(
        &self,
        epoch: &Epoch,
        chords: &[Vec<Key>],
        queue: &EventQueue,
    ) -> std::result::Result<Grab, String> {
        match self {
            Self::X11 { .. } => GrabCapture::start_into(epoch, chords, queue.clone())
                .map(Grab::X11)
                .map_err(|err| err.to_string()),
            #[cfg(feature = "evdev")]
            Self::Devices(grab) => grab
                .claim(chords)
                .map(Grab::Devices)
                .map_err(|err| err.to_string()),
        }
    }

    /// Whether any of `keys` is physically down. Without an X probe the answer is "not that
    /// we can see", and taps fire at once.
    pub(super) fn any_down(&self, keys: &[Key]) -> bool {
        match self {
            Self::X11 { probe, .. } => probe.as_ref().is_some_and(|probe| probe.any_down(keys)),
            #[cfg(feature = "evdev")]
            Self::Devices(grab) => grab.any_down(keys),
        }
    }

    /// Taps `targets` clean of `held`'s classes they don't name; `false` sends the caller to
    /// plain injection. `lift_primary` is X11's: the trigger's own key, which a grabbed
    /// device's virtual keyboard never saw go down, so never needs lifting.
    pub(super) fn tap_lifted(
        &self,
        targets: &[Holdable],
        held: Mods,
        lift_primary: Option<Key>,
        duration: Duration,
    ) -> bool {
        match self {
            Self::X11 { lifter, .. } => lifter
                .as_ref()
                .is_some_and(|lifter| lifter.tap(targets, held, lift_primary, duration)),
            #[cfg(feature = "evdev")]
            Self::Devices(grab) => grab.tap_lifted(targets, held, duration),
        }
    }

    /// Tells the supervisor the manager still answers; `false` once it is gone. X11 has no
    /// supervisor, and always answers.
    #[cfg_attr(
        not(feature = "evdev"),
        allow(dead_code, reason = "only a device grab has a supervisor to beat")
    )]
    pub(super) fn beat(&self) -> bool {
        match self {
            Self::X11 { .. } => true,
            #[cfg(feature = "evdev")]
            Self::Devices(grab) => grab.beat().is_ok(),
        }
    }

    /// The modifiers the X server still believes held with no key holding them — the stuck
    /// keys XTEST can leave. A grab's virtual keyboard can't leave any: removing it releases
    /// all it held.
    pub(super) fn stuck(&self) -> Vec<Mods> {
        match self {
            Self::X11 { probe, .. } => probe
                .as_ref()
                .and_then(|probe| probe.snapshot())
                .map(|state| state.stuck())
                .unwrap_or_default(),
            #[cfg(feature = "evdev")]
            Self::Devices(_) => Vec::new(),
        }
    }

    /// What the device backend holds: the names of the devices it grabbed, and whether a
    /// keyboard is among them. `None` on X11, which holds nothing.
    pub(super) fn holding(&self) -> Option<(&[String], bool)> {
        match self {
            Self::X11 { .. } => None,
            #[cfg(feature = "evdev")]
            Self::Devices(grab) => Some((grab.held(), grab.holds_a_keyboard())),
        }
    }

    /// Whether this is the device backend — the one with a supervisor to beat for.
    #[cfg(feature = "evdev")]
    pub(super) const fn is_devices(&self) -> bool {
        matches!(self, Self::Devices(_))
    }

    /// Ends the backend: the devices' grab let go, the supervisor told so. X11's grabs go
    /// with their captures.
    #[cfg_attr(
        not(feature = "evdev"),
        allow(clippy::unused_self, reason = "only a device grab has anything to end")
    )]
    pub(super) fn stop(&mut self) {
        #[cfg(feature = "evdev")]
        if let Self::Devices(grab) = self {
            grab.stop();
        }
    }
}
