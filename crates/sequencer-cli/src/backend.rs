//! Backend selection: which injection/capture pair a run opens, and the run itself.
//!
//! The one place that decides X11-versus-devices, so the decision cannot be made twice
//! with two answers. [`crate::elevate`] asks the same probe before any password prompt.

use sequencer_core::emit::EmitAction;
use sequencer_core::input::Key;
use sequencer_core::time::Duration;

use crate::binds::format::Profile;
use crate::binds::run;
use crate::runtime::{CountingSink, RunSummary};
use crate::{Deps, Result};

/// Whether this build has any input backend at all.
pub(crate) const AVAILABLE: bool = platform::AVAILABLE;

/// Runs one binds profile as a plain process — no manager, no on-disk set — using the
/// injected sink and pump if there are any, else the platform's pair.
///
/// `marker` is the action that begins each repetition, which is what the exit report
/// counts; `cadence` is the requested period, which sizes its burst window.
pub(crate) fn run_binds(
    profile: &Profile,
    marker: EmitAction,
    cadence: Duration,
    hotkeys: &[Key],
    deps: &mut Deps<'_>,
) -> Result<RunSummary> {
    if let (Some(sink), Some(pump)) = (deps.sink.as_deref_mut(), deps.pump.as_deref_mut()) {
        let started = deps.clock.now();
        let mut counting = CountingSink::new(sink, marker, cadence);
        run::run(profile, &mut counting, deps.clock, pump, 0, None)?;
        let wall = deps.clock.now().saturating_sub(started);
        return Ok(counting.finish(wall));
    }
    platform::run(profile, marker, cadence, hotkeys)
}

/// The parts that need a real operating system, and the stubs that stand in when there is
/// no backend for this one.
///
/// Either backend alone is a complete one, so this is gated on having *a* backend rather
/// than on having the device one. `--features cli,xtest` is a legitimate build: X11 only,
/// no device access, and no evdev in the consumer's dependency graph.
#[cfg(all(any(feature = "evdev", feature = "xtest"), target_os = "linux"))]
mod platform {
    use super::{CountingSink, Duration, EmitAction, Key, Profile, Result, RunSummary, run};
    use sequencer_core::emit::InputSink;
    use sequencer_core::time::Clock as _;
    use sequencer_input::{Epoch, SystemClock};
    #[cfg(feature = "evdev")]
    use sequencer_input::{EvdevCapture, UinputSink};

    pub(super) const AVAILABLE: bool = true;

    /// Opens the pair and drives the executor until the stop key or the source closing.
    pub(super) fn run(
        profile: &Profile,
        marker: EmitAction,
        cadence: Duration,
        hotkeys: &[Key],
    ) -> Result<RunSummary> {
        // One epoch shared by the clock and the capture threads, so an event's timestamp
        // and the executor's waits sit on the same timeline.
        let epoch = Epoch::start();
        let clock = SystemClock::from_epoch(epoch.instant());

        // Session mode's window: as root (if sudo brought us here), make sure the module
        // is loaded, open everything, then shed the privilege before anything runs.
        // The sink opens first: capture excludes our virtual device by name, so it has to
        // exist before the reader threads enumerate.
        crate::elevate::load_uinput_if_root();
        let (mut sink, mut capture) = open_pair(&epoch, hotkeys)?;
        let stream = capture.stream();
        crate::elevate::drop_root_after_open()?;

        let hold = hold_probe(&capture);
        let mut pump = crate::runtime::CapturePump::new(stream, &clock);
        let mut counting = CountingSink::new(sink.as_mut(), marker, cadence);
        let started = clock.now();
        let outcome = run::run(
            profile,
            &mut counting,
            &clock,
            &mut pump,
            0,
            hold.as_deref(),
        );
        let wall = clock.now().saturating_sub(started);
        // The probe borrows the capture; let go before stopping it.
        drop(hold);
        let dropped = pump.dropped();
        capture.stop();

        if dropped > 0 {
            tracing::warn!(dropped, "input events were lost while the loop was busy");
        }
        outcome?;
        Ok(counting.finish(wall))
    }

    /// Opens the injection sink and the hotkey source as a **pair**.
    ///
    /// The two halves are chosen together, never independently. On a usable X11 session
    /// both go through the X server; everywhere else both go through the input devices. A
    /// mixed pair would be the worst of each — XTEST's rate with evdev's permission
    /// requirement, and a run that decided it needed no password discovering otherwise
    /// halfway through opening.
    ///
    /// Which session it is was already settled by [`sequencer_input::x11::is_usable`],
    /// which actually connects rather than trusting `$DISPLAY`. That is the same answer
    /// [`crate::elevate`] used to decide whether to ask for a password, so the two cannot
    /// disagree.
    ///
    /// A grab the X server refuses — another program already owns the key — is a hard
    /// error rather than a fallback. Dropping to the device path there would demand device
    /// access, and possibly a password, for a problem no privilege can fix: the answer is
    /// a different `--activate` key, and the error says so.
    ///
    /// On the device path the sink opens first, because capture skips our own virtual
    /// device by name and so needs it to exist before the reader threads enumerate.
    ///
    /// In an X11-only build there is no device path to fall back to, and a session with no
    /// X server is simply out of backends.
    fn open_pair(epoch: &Epoch, hotkeys: &[Key]) -> Result<(Box<dyn InputSink>, Capture)> {
        #[cfg(feature = "xtest")]
        if sequencer_input::x11::is_usable() {
            let sink = sequencer_input::XTestSink::open()?;
            // The binding policy, same as the manager's: each hotkey names its modifier
            // state exactly, so `shift F9` passes through to the application — which is
            // what the executor's trigger matching expects to have been grabbed.
            let chords: Vec<Vec<Key>> = hotkeys.iter().map(|key| vec![*key]).collect();
            let (queue, stream) = sequencer_input::CaptureStream::channel(256);
            let capture = sequencer_input::GrabCapture::start_into(epoch, &chords, queue)?;
            // The grab never delivers the trigger's release (its active grab ends on the
            // press, so the release routes elsewhere); a held loop asks the server instead.
            let probe = sequencer_input::KeyProbe::open().map(Box::new);
            tracing::info!(
                keys = hotkeys.len(),
                "X11: injecting through XTEST, hotkeys through key grabs (no device access)"
            );
            return Ok((Box::new(sink), Capture::Grab(capture, Some(stream), probe)));
        }

        // Only the X11 grab has to name individual keys; the device backend watches every
        // device and lets the executor decide what it cares about.
        let _ = hotkeys;
        #[cfg(feature = "evdev")]
        {
            let sink = UinputSink::open()?;
            let mut capture = EvdevCapture::new(epoch.clone());
            let stream = capture.start()?;
            tracing::info!(
                devices = capture.watching(),
                "devices: injecting through uinput, hotkeys from /dev/input"
            );
            Ok((Box::new(sink), Capture::Evdev(capture, Some(stream))))
        }
        #[cfg(not(feature = "evdev"))]
        {
            let _ = epoch;
            Err(crate::Error::NotImplemented(
                "this build has only the X11 backend, and no X server answered. Rebuild \
                 with the `evdev` feature for Wayland and the console."
                    .to_owned(),
            ))
        }
    }

    /// How a held loop learns its trigger came up, per backend: the device path delivers
    /// the release as an event and needs nothing here; the X11 grab needs the server
    /// asked, which is what the probe opened beside it is for.
    fn hold_probe(capture: &Capture) -> Option<Box<dyn Fn(Key) -> bool + '_>> {
        match capture {
            #[cfg(feature = "evdev")]
            Capture::Evdev(..) => None,
            #[cfg(feature = "xtest")]
            Capture::Grab(_, _, probe) => probe.as_deref().map(|probe| {
                Box::new(move |key: Key| probe.any_down(&[key])) as Box<dyn Fn(Key) -> bool + '_>
            }),
        }
    }

    /// Whichever capture backend the run picked, with one shape for the caller.
    enum Capture {
        #[cfg(feature = "evdev")]
        Evdev(EvdevCapture, Option<sequencer_input::CaptureStream>),
        #[cfg(feature = "xtest")]
        Grab(
            sequencer_input::GrabCapture,
            Option<sequencer_input::CaptureStream>,
            Option<Box<sequencer_input::KeyProbe>>,
        ),
    }

    impl Capture {
        /// Takes the event stream. Called once, right after opening.
        fn stream(&mut self) -> sequencer_input::CaptureStream {
            match self {
                #[cfg(feature = "evdev")]
                Self::Evdev(_, stream) => stream.take(),
                #[cfg(feature = "xtest")]
                Self::Grab(_, stream, _) => stream.take(),
            }
            .expect("the stream is taken exactly once, right after opening")
        }

        fn stop(&mut self) {
            match self {
                #[cfg(feature = "evdev")]
                Self::Evdev(capture, _) => capture.stop(),
                #[cfg(feature = "xtest")]
                Self::Grab(capture, _, _) => capture.stop(),
            }
        }
    }
}

#[cfg(not(all(any(feature = "evdev", feature = "xtest"), target_os = "linux")))]
mod platform {
    use super::{Duration, EmitAction, Key, Profile, Result, RunSummary};
    use crate::Error;

    pub(super) const AVAILABLE: bool = false;

    pub(super) fn unsupported<T>() -> Result<T> {
        Err(Error::NotImplemented(format!(
            "no input backend for {}; only Linux is supported.",
            std::env::consts::OS
        )))
    }

    pub(super) fn run(
        _profile: &Profile,
        _marker: EmitAction,
        _cadence: Duration,
        _hotkeys: &[Key],
    ) -> Result<RunSummary> {
        unsupported()
    }
}
