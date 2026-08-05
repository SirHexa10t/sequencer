//! What each subcommand actually does.

use sequencer_core::input::{EventKind, InputEvent, Key};
use sequencer_core::testutil::Harness;
use sequencer_core::time::Timestamp;
use sequencer_core::{CompiledProfile, Engine};
// Only the benchmark's live progress asks whether stderr is a terminal.
#[cfg(all(feature = "evdev", target_os = "linux"))]
use std::io::IsTerminal as _;

use sequencer_input::probe::{CheckResult, Step as FixStep};
use sequencer_input::{Requirement, SessionInfo};

use crate::args::{BenchArgs, DoctorArgs, SimulateArgs};
use crate::runtime::{RunSummary, run_engine};
use crate::{Deps, Error, Result, exit};

/// Every requirement the Linux backend needs, in report order.
const REQUIREMENTS: &[Requirement] = &[
    Requirement::UinputModuleLoaded,
    Requirement::UinputNodeWritable,
    Requirement::EvdevReadable,
];

/// Runs a validated profile, using the injected sink and pump if there are any.
pub(crate) fn run_profile(
    profile: CompiledProfile,
    max_emits_per_sec: u32,
    cadence: sequencer_core::time::Duration,
    hotkeys: &[Key],
    deps: &mut Deps<'_>,
) -> Result<RunSummary> {
    let mut engine = Engine::new(profile, 0);

    if let (Some(sink), Some(pump)) = (deps.sink.as_deref_mut(), deps.pump.as_deref_mut()) {
        return run_engine(
            &mut engine,
            sink,
            deps.clock,
            pump,
            max_emits_per_sec,
            cadence,
        );
    }
    platform::run(&mut engine, max_emits_per_sec, cadence, hotkeys)
}

/// `sequencer bench`.
///
/// # Errors
///
/// If there is no backend for this platform, or the devices cannot be opened.
#[cfg(all(feature = "evdev", target_os = "linux"))]
pub fn bench(args: &BenchArgs, deps: &mut Deps<'_>) -> Result<u8> {
    match args.cps {
        Some(rate) => writeln!(deps.out, "Measuring {rate}/s for {:.1}s...", args.seconds)?,
        None => writeln!(
            deps.out,
            "Measuring the ceiling for {:.1}s (no target rate)...",
            args.seconds
        )?,
    }
    deps.out.flush()?;

    let mut observer = BenchProgress {
        live: std::io::stderr().is_terminal(),
    };
    let result = sequencer_input::linux::bench::run(args.cps, args.seconds, &mut observer)?;
    if observer.live {
        // Wipe the progress line so the summary starts on clean ground.
        eprint!("\r\u{1b}[K");
    }

    writeln!(deps.out)?;
    if let Some(requested) = args.cps {
        writeln!(deps.out, "  requested   {requested:>10.0}/s")?;
    }
    writeln!(deps.out, "  emitted     {:>10.0}/s", result.emitted_rate())?;
    writeln!(
        deps.out,
        "  delivered   {:>10.0}/s",
        result.delivered_rate()
    )?;
    writeln!(
        deps.out,
        "\n{} presses written over {:.3}s; the kernel delivered {}.",
        result.emitted,
        result.elapsed.as_secs_f64(),
        result.delivered
    )?;

    // Emitted is what this process wrote; delivered is what a reader actually saw. A gap
    // means events were coalesced or dropped below us, which is the number that matters
    // and the one a rate computed purely from our own loop would never show.
    if result.delivered < result.emitted {
        let lost = result.emitted - result.delivered;
        writeln!(
            deps.out,
            "{lost} did not arrive: at this rate the kernel or the reader is the \
             bottleneck, not the loop."
        )?;
    }
    Ok(exit::OK)
}

/// Renders the benchmark's live progress, and sheds root once the devices are open.
///
/// Progress goes to **stderr**, not `deps.out`: it is a carriage-return-overwritten line
/// meant for a watching human, and mixing it into the stream a caller may be capturing
/// would leave a pile of `\r`-joined junk in their file. It is suppressed entirely when
/// stderr is not a terminal, so a redirected run just prints its summary.
#[cfg(all(feature = "evdev", target_os = "linux"))]
struct BenchProgress {
    live: bool,
}

#[cfg(all(feature = "evdev", target_os = "linux"))]
impl sequencer_input::linux::BenchObserver for BenchProgress {
    fn devices_open(
        &mut self,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        crate::elevate::drop_root_after_open().map_err(Into::into)
    }

    fn sample(&mut self, sample: sequencer_input::linux::BenchSample) {
        if !self.live {
            return;
        }
        // `\r` + erase-to-end-of-line: one line, rewritten, no scrollback spam.
        eprint!(
            "\r\u{1b}[K  {:>5.1}s   emitting {:>8.0}/s   delivered {:>8.0}/s",
            sample.elapsed.as_secs_f64(),
            sample.emitted_rate(),
            sample.delivered_rate(),
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

/// `sequencer bench`, on a platform with no backend.
///
/// # Errors
///
/// Always: there is nothing to measure.
#[cfg(not(all(feature = "evdev", target_os = "linux")))]
pub fn bench(_args: &BenchArgs, _deps: &mut Deps<'_>) -> Result<u8> {
    platform::unsupported()
}

/// `sequencer doctor`.
///
/// # Errors
///
/// If writing the report fails.
pub fn doctor(args: &DoctorArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let info = SessionInfo::detect();

    writeln!(
        deps.out,
        "sequencer {}  ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(deps.out, "session: {}", info.session)?;
    writeln!(deps.out, "backend: {}", injection_backend())?;
    if args.global.verbose > 0 {
        for (label, value) in [
            ("DISPLAY", info.display.as_deref()),
            ("WAYLAND_DISPLAY", info.wayland_display.as_deref()),
            ("XDG_SESSION_TYPE", info.session_type.as_deref()),
        ] {
            if let Some(value) = value {
                writeln!(deps.out, "  {label}={value}")?;
            }
        }
    }
    writeln!(deps.out)?;

    if !platform::AVAILABLE {
        writeln!(
            deps.out,
            "[fail] no input backend: this build has none for {}. Only Linux is supported.",
            std::env::consts::OS
        )?;
        return Ok(exit::FAILURE);
    }

    let mut unmet = Vec::new();
    for &requirement in REQUIREMENTS {
        match requirement.check() {
            CheckResult::Pass => writeln!(deps.out, "[ok]   {}", requirement.label())?,
            CheckResult::Unknown(why) => {
                writeln!(deps.out, "[??]   {}: {why}", requirement.label())?;
            }
            CheckResult::Fail(detail) => {
                writeln!(deps.out, "[fail] {}: {detail}", requirement.label())?;
                unmet.push(requirement);
            }
        }
    }

    for requirement in &unmet {
        write_remediation(*requirement, deps)?;
    }

    if unmet.is_empty() {
        writeln!(deps.out, "\nReady.")?;
        return Ok(exit::OK);
    }
    if x11_handles_the_run() {
        writeln!(
            deps.out,
            "\nNone of that is needed here: this is an X11 session, so clicks go through \
             XTEST and hotkeys through key grabs. The checks above matter only if you \
             later run without X."
        )?;
        return Ok(exit::OK);
    }
    {
        writeln!(
            deps.out,
            "\nUntil that is fixed, `sequencer simulate` still works: it runs the engine \
             without touching an input device."
        )?;
        Ok(exit::FAILURE)
    }
}

/// Whether the X11 pair will handle the whole run, making the device checks informational.
fn x11_handles_the_run() -> bool {
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    {
        sequencer_input::x11::is_usable()
    }
    #[cfg(not(all(feature = "xtest", target_os = "linux")))]
    {
        false
    }
}

fn write_remediation(requirement: Requirement, deps: &mut Deps<'_>) -> Result<()> {
    let fix = requirement.remediation();
    writeln!(deps.out, "\n{}", fix.title)?;
    writeln!(deps.out, "  {}", fix.why)?;
    for step in &fix.steps {
        match step {
            FixStep::Shell(command) => writeln!(deps.out, "      $ {command}")?,
            FixStep::WriteFile { path, body } => {
                writeln!(deps.out, "      write {path}:")?;
                for line in body.lines() {
                    writeln!(deps.out, "          {line}")?;
                }
            }
            FixStep::Manual(text) => writeln!(deps.out, "      {text}")?,
        }
    }
    if let Some(caution) = fix.caution {
        writeln!(deps.out, "  NOTE: {caution}")?;
    }
    Ok(())
}

/// `sequencer simulate`.
///
/// # Errors
///
/// If the script cannot be read or parsed, or the settings are not runnable.
pub fn simulate(args: &SimulateArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let source = if args.script == std::path::Path::new("-") {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(&args.script).map_err(|source| Error::ScriptRead {
            path: args.script.display().to_string(),
            source,
        })?
    };
    let script = parse_script(&source)?;

    let profile = CompiledProfile::validate(args.clicker.config().to_profile()?)?;
    let mut harness = Harness::new(profile, 0);
    for (at, kind) in script {
        harness.at(at, InputEvent::physical(at, kind));
    }
    harness.run_until(Timestamp::from_millis(args.until_ms));

    writeln!(deps.out, "{}", harness.timeline())?;
    if !args.clicker.global.quiet {
        writeln!(
            deps.out,
            "\n{} actions, {} repetitions, {} skipped.",
            harness.sink().emitted.len(),
            harness.stats.iterations_started,
            harness.stats.slots_skipped
        )?;
        let leaked = harness.sink().leaked();
        if leaked.is_empty() {
            writeln!(deps.out, "nothing left held.")?;
        } else {
            writeln!(deps.out, "STILL HELD: {leaked:?}")?;
        }
    }
    Ok(exit::OK)
}

/// Parses `<milliseconds> <down|up> <key>` lines.
fn parse_script(source: &str) -> Result<Vec<(Timestamp, EventKind)>> {
    let mut events = Vec::new();
    for (number, raw) in source.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let at = number + 1;
        let mut parts = line.split_whitespace();
        let (Some(ms), Some(edge), Some(key), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::Script {
                line: at,
                detail: format!("expected `<milliseconds> <down|up> <key>`, got `{line}`"),
            });
        };

        let ms: u64 = ms.parse().map_err(|_| Error::Script {
            line: at,
            detail: format!("`{ms}` is not a whole number of milliseconds"),
        })?;
        let key: Key = key.parse().map_err(|err| Error::Script {
            line: at,
            detail: format!("{err}"),
        })?;
        let kind = match edge {
            "down" => EventKind::KeyDown(key),
            "up" => EventKind::KeyUp(key),
            other => {
                return Err(Error::Script {
                    line: at,
                    detail: format!("expected `down` or `up`, got `{other}`"),
                });
            }
        };
        events.push((Timestamp::from_millis(ms), kind));
    }
    Ok(events)
}

/// Which backend pair a run would choose right now, for `doctor` to report.
///
/// Asks the same question `platform::open_pair` will — [`sequencer_input::x11::is_usable`],
/// which connects rather than reading `$DISPLAY` — so the report cannot promise one backend
/// and the run pick the other.
///
/// On the X11 pair the uinput requirements listed below are informational: neither half
/// touches a device, and a refused grab is a hard error rather than a fall back to them.
fn injection_backend() -> &'static str {
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    if sequencer_input::x11::is_usable() {
        return "X11 — XTEST for clicks, key grabs for hotkeys (no device access, no sudo)";
    }
    "uinput device for clicks, evdev for hotkeys"
}

/// The parts that need a real operating system, and the stubs that stand in when there is
/// no backend for this one.
#[cfg(all(feature = "evdev", target_os = "linux"))]
mod platform {
    use super::{Result, RunSummary, run_engine};
    use sequencer_core::Engine;
    use sequencer_core::emit::InputSink;
    use sequencer_input::{Epoch, EvdevCapture, SystemClock};

    pub(super) const AVAILABLE: bool = true;

    /// Opens the devices and drives the engine until it quits.
    pub(super) fn run(
        engine: &mut Engine,
        max_emits_per_sec: u32,
        cadence: sequencer_core::time::Duration,
        hotkeys: &[sequencer_core::input::Key],
    ) -> Result<RunSummary> {
        // One epoch shared by the clock and the capture threads, so an event's timestamp
        // and the engine's deadlines sit on the same timeline and the cadence
        // phase-locks to the physical press.
        let epoch = Epoch::start();
        let clock = SystemClock::from_epoch(epoch.instant());

        // Session mode's window: as root (if sudo brought us here), make sure the module
        // is loaded, open everything, then shed the privilege before the engine runs.
        // The sink opens first: capture excludes our virtual device by name, so it has to
        // exist before the reader threads enumerate.
        crate::elevate::load_uinput_if_root();
        let (mut sink, mut capture) = open_pair(&epoch, hotkeys)?;
        let stream = capture.stream();
        crate::elevate::drop_root_after_open()?;

        let mut pump = crate::runtime::CapturePump::new(stream, &clock);
        let summary = run_engine(
            engine,
            sink.as_mut(),
            &clock,
            &mut pump,
            max_emits_per_sec,
            cadence,
        );
        let dropped = pump.dropped();
        capture.stop();

        if dropped > 0 {
            tracing::warn!(dropped, "input events were lost while the loop was busy");
        }
        summary
    }

    /// Opens the injection sink and the hotkey source as a **pair**.
    ///
    /// The two halves are chosen together, never independently. On a usable X11 session
    /// both go through the X server; everywhere else both go through the input devices.
    /// A mixed pair would be the worst of each: XTEST's rate with evdev's permission
    /// requirement, and a run that decided it needed no password discovering otherwise
    /// halfway through opening.
    ///
    /// Which session it is was already settled by [`sequencer_input::x11::is_usable`],
    /// which actually connects rather than trusting `$DISPLAY`. That is the same answer
    /// [`crate::elevate`] used to decide whether to ask for a password, so the two cannot
    /// disagree.
    ///
    /// A grab the X server refuses — another program already owns the key — is a hard
    /// error rather than a fallback. Dropping to the device path there would demand
    /// device access, and possibly a password, for a problem no privilege can fix: the
    /// answer is a different `--activate` key, and the error says so.
    fn open_pair(
        epoch: &Epoch,
        hotkeys: &[sequencer_core::input::Key],
    ) -> Result<(Box<dyn InputSink>, Capture)> {
        #[cfg(feature = "xtest")]
        if sequencer_input::x11::is_usable() {
            let sink = sequencer_input::XTestSink::open()?;
            let (capture, stream) = sequencer_input::GrabCapture::start(epoch, hotkeys)?;
            tracing::info!(
                keys = hotkeys.len(),
                "X11: injecting through XTEST, hotkeys through key grabs"
            );
            return Ok((Box::new(sink), Capture::Grab(capture, Some(stream))));
        }

        // Only the X11 grab has to name individual keys; the device backend watches every
        // device and lets the engine decide what it cares about.
        let _ = hotkeys;
        let sink = sequencer_input::UinputSink::open()?;
        let mut capture = EvdevCapture::new(epoch.clone());
        let stream = capture.start()?;
        tracing::info!(
            devices = capture.watching(),
            "devices: injecting through uinput, hotkeys from /dev/input"
        );
        Ok((Box::new(sink), Capture::Evdev(capture, Some(stream))))
    }

    /// Whichever capture backend the run picked, with one shape for the caller.
    enum Capture {
        Evdev(EvdevCapture, Option<sequencer_input::CaptureStream>),
        #[cfg(feature = "xtest")]
        Grab(
            sequencer_input::GrabCapture,
            Option<sequencer_input::CaptureStream>,
        ),
    }

    impl Capture {
        /// Takes the event stream. Called once; the backend keeps filling it until `stop`.
        fn stream(&mut self) -> sequencer_input::CaptureStream {
            match self {
                Self::Evdev(_, stream) => stream.take(),
                #[cfg(feature = "xtest")]
                Self::Grab(_, stream) => stream.take(),
            }
            .expect("the stream is taken exactly once, right after opening")
        }

        fn stop(&mut self) {
            match self {
                Self::Evdev(capture, _) => capture.stop(),
                #[cfg(feature = "xtest")]
                Self::Grab(capture, _) => capture.stop(),
            }
        }
    }
}

#[cfg(not(all(feature = "evdev", target_os = "linux")))]
mod platform {
    use super::{Error, Result, RunSummary};
    use sequencer_core::Engine;

    pub(super) const AVAILABLE: bool = false;

    pub(super) fn unsupported<T>() -> Result<T> {
        Err(Error::NotImplemented(format!(
            "no input backend for {}; only Linux is supported. `sequencer simulate` \
             works anywhere.",
            std::env::consts::OS
        )))
    }

    pub(super) fn run(
        _engine: &mut Engine,
        _max_emits_per_sec: u32,
        _cadence: sequencer_core::time::Duration,
        _hotkeys: &[sequencer_core::input::Key],
    ) -> Result<RunSummary> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_parses_with_comments_and_blank_lines() {
        let script = "\
# start clicking
0 down f9

520 up f9   # and stop
";
        let events = parse_script(script).expect("should parse");
        assert_eq!(
            events,
            vec![
                (Timestamp::ZERO, EventKind::KeyDown(Key::F9)),
                (Timestamp::from_millis(520), EventKind::KeyUp(Key::F9)),
            ]
        );
    }

    #[test]
    fn a_bad_script_line_says_which_line() {
        for (script, expected_line) in [
            ("0 down f9\nnonsense\n", 2),
            ("0 sideways f9\n", 1),
            ("abc down f9\n", 1),
            ("0 down nosuchkey\n", 1),
            ("0 down f9 extra\n", 1),
        ] {
            let err = parse_script(script).expect_err("should reject");
            let Error::Script { line, .. } = err else {
                panic!("expected a script error, got {err:?}");
            };
            assert_eq!(line, expected_line, "for {script:?}");
        }
    }

    #[test]
    fn an_empty_script_is_valid_and_produces_nothing() {
        assert_eq!(parse_script("\n\n# only comments\n").unwrap(), Vec::new());
    }
}
