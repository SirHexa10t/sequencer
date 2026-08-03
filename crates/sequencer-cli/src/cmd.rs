//! What each subcommand actually does.

use sequencer_core::click::{ActivationMode, ClickAction, ClickConfig};
use sequencer_core::input::{EventKind, InputEvent, Key};
use sequencer_core::testutil::Harness;
use sequencer_core::time::Timestamp;
use sequencer_core::{CompiledProfile, Engine};
use sequencer_input::probe::{CheckResult, Step as FixStep};
use sequencer_input::{Requirement, SessionInfo};

use crate::args::{BenchArgs, ClickerArgs, DoctorArgs, SimulateArgs};
use crate::runtime::{RunSummary, fuse_limit, run_engine};
use crate::{Deps, Error, Result, exit};

/// Every requirement the Linux backend needs, in report order.
const REQUIREMENTS: &[Requirement] = &[
    Requirement::UinputModuleLoaded,
    Requirement::UinputNodeWritable,
    Requirement::EvdevReadable,
];

/// `sequencer clicker`.
///
/// # Errors
///
/// If the settings do not describe a runnable profile, or the input devices cannot be
/// opened.
pub fn clicker(args: &ClickerArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let config = args.config();
    let profile = CompiledProfile::validate(config.to_profile()?)?;

    if args.dry_run {
        describe(&config, deps)?;
        writeln!(deps.out, "\ndry run: nothing was sent to any application.")?;
        return Ok(exit::OK);
    }

    if !args.global.quiet {
        describe(&config, deps)?;
        writeln!(deps.out)?;
        deps.out.flush()?;
    }

    let summary = run_profile(profile, fuse_limit(config.cps), deps)?;
    report(&summary, args, deps)?;
    Ok(exit::OK)
}

/// Runs a validated profile, using the injected sink and pump if there are any.
fn run_profile(
    profile: CompiledProfile,
    max_emits_per_sec: u32,
    deps: &mut Deps<'_>,
) -> Result<RunSummary> {
    let mut engine = Engine::new(profile, 0);

    if let (Some(sink), Some(pump)) = (deps.sink.as_deref_mut(), deps.pump.as_deref_mut()) {
        return run_engine(&mut engine, sink, deps.clock, pump, max_emits_per_sec);
    }
    platform::run(&mut engine, max_emits_per_sec)
}

/// Prints the settings in the words a user would use, so a surprising result can be
/// traced to a flag rather than guessed at.
fn describe(config: &ClickConfig, deps: &mut Deps<'_>) -> Result<()> {
    let what = match config.action {
        ClickAction::Button(button) => format!("{button} click"),
        ClickAction::Key(key) => format!("{key} key press"),
    };
    let how = match config.mode {
        ActivationMode::Hold => format!("while {} is held", config.activate),
        ActivationMode::Toggle => format!("after tapping {}, until tapped again", config.activate),
    };
    let limit = match config.limit {
        0 => String::from("no limit"),
        n => format!("stopping after {n}"),
    };
    writeln!(
        deps.out,
        "{what} at {cps}/s, {how} ({limit}). {quit} quits.",
        cps = config.cps,
        quit = config.quit,
    )?;
    Ok(())
}

fn report(summary: &RunSummary, args: &ClickerArgs, deps: &mut Deps<'_>) -> Result<()> {
    if args.global.quiet {
        return Ok(());
    }
    match summary.achieved_cps() {
        Some(rate) => writeln!(
            deps.out,
            "{} actions over {} repetitions, {rate:.0}/s achieved (asked for {}/s).",
            summary.emitted, summary.iterations, args.cps
        )?,
        None => writeln!(
            deps.out,
            "{} actions sent over {} repetitions.",
            summary.emitted, summary.iterations
        )?,
    }
    if summary.slots_skipped > 0 {
        writeln!(
            deps.out,
            "{} repetitions were skipped: this machine could not keep up with {}/s. \
             The achieved rate above is what it can actually do.",
            summary.slots_skipped, args.cps
        )?;
    }
    if summary.throttled > 0 {
        writeln!(
            deps.out,
            "{} actions were dropped by the output rate limit.",
            summary.throttled
        )?;
    }
    Ok(())
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

    let result = sequencer_input::linux::bench::run(args.cps, args.seconds)?;

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
        Ok(exit::OK)
    } else {
        writeln!(
            deps.out,
            "\nUntil that is fixed, `sequencer simulate` and `clicker --dry-run` still \
             work: neither touches an input device."
        )?;
        Ok(exit::FAILURE)
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

/// The parts that need a real operating system, and the stubs that stand in when there is
/// no backend for this one.
#[cfg(all(feature = "evdev", target_os = "linux"))]
mod platform {
    use super::{Result, RunSummary, run_engine};
    use sequencer_core::Engine;
    use sequencer_input::{EvdevCapture, SystemClock, UinputSink};

    pub(super) const AVAILABLE: bool = true;

    /// Opens the devices and drives the engine until it quits.
    pub(super) fn run(engine: &mut Engine, max_emits_per_sec: u32) -> Result<RunSummary> {
        // One epoch shared by the clock and the capture threads, so an event's timestamp
        // and the engine's deadlines sit on the same timeline and the cadence
        // phase-locks to the physical press.
        let epoch = sequencer_input::Epoch::start();
        let clock = SystemClock::from_epoch(epoch.instant());

        // The sink opens first: capture excludes our virtual device by name, so it has to
        // exist before the reader threads enumerate.
        let mut sink = UinputSink::open()?;
        let mut capture = EvdevCapture::new(epoch);
        let stream = capture.start()?;
        tracing::info!(devices = capture.watching(), "watching input devices");

        let mut pump = crate::runtime::CapturePump::new(stream, &clock);
        let summary = run_engine(engine, &mut sink, &clock, &mut pump, max_emits_per_sec);
        let dropped = pump.dropped();
        capture.stop();

        if dropped > 0 {
            tracing::warn!(dropped, "input events were lost while the loop was busy");
        }
        summary
    }
}

#[cfg(not(all(feature = "evdev", target_os = "linux")))]
mod platform {
    use super::{Error, Result, RunSummary};
    use sequencer_core::Engine;

    pub(super) const AVAILABLE: bool = false;

    pub(super) fn unsupported<T>() -> Result<T> {
        Err(Error::NotImplemented(format!(
            "no input backend for {}; only Linux is supported. `sequencer simulate` and \
             `clicker --dry-run` work anywhere.",
            std::env::consts::OS
        )))
    }

    pub(super) fn run(_engine: &mut Engine, _max_emits_per_sec: u32) -> Result<RunSummary> {
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
