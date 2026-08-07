//! The doctor command: what this machine can do, and the exact fix for what it cannot.

use sequencer_input::probe::{CheckResult, Step as FixStep};
use sequencer_input::{Requirement, SessionInfo};

use crate::args::DoctorArgs;
use crate::backend;
use crate::{Deps, Result, exit};

/// Every requirement the device backend needs, in report order.
///
/// Empty in an X11-only build: nothing there opens a device, so there is nothing to check
/// and reporting failures would be reporting on a code path that was compiled out.
#[cfg(feature = "evdev")]
const REQUIREMENTS: &[Requirement] = &[
    Requirement::UinputModuleLoaded,
    Requirement::UinputNodeWritable,
    Requirement::EvdevReadable,
];
#[cfg(not(feature = "evdev"))]
const REQUIREMENTS: &[Requirement] = &[];

/// `sequencer doctor`.
///
/// # Errors
///
/// If writing the report fails.
pub fn doctor(args: &DoctorArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let info = SessionInfo::detect();
    // Probed once and reused: every call opens a fresh X connection, and two calls could
    // in principle straddle a session ending and have the report contradict itself.
    let on_x11 = x11_handles_the_run();

    writeln!(
        deps.out,
        "sequencer {}  ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(deps.out, "session: {}", info.session)?;
    writeln!(deps.out, "backend: {}", backend_pair(on_x11))?;
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

    if !backend::AVAILABLE {
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
        if REQUIREMENTS.is_empty() && !on_x11 {
            writeln!(
                deps.out,
                "[fail] no X server answered, and this build has only the X11 backend. \
                 Rebuild with the `evdev` feature to run outside X."
            )?;
            return Ok(exit::FAILURE);
        }
        writeln!(deps.out, "\nReady.")?;
        return Ok(exit::OK);
    }
    if on_x11 {
        writeln!(
            deps.out,
            "\nNone of that is needed here: this is an X11 session, so clicks go through \
             XTEST and hotkeys through key grabs, and neither opens a device. The checks \
             above matter only if you later run without X."
        )?;
        return Ok(exit::OK);
    }
    {
        writeln!(
            deps.out,
            "\nUntil that is fixed, `sequencer detect-key` still works: it reads the \
             terminal, not the devices."
        )?;
        Ok(exit::FAILURE)
    }
}

/// Which backend pair a run would choose, for `doctor` to report.
const fn backend_pair(on_x11: bool) -> &'static str {
    if on_x11 {
        "X11 — XTEST for clicks, key grabs for hotkeys (no device access, no sudo)"
    } else {
        "uinput device for clicks, evdev for hotkeys"
    }
}

/// Whether the X11 pair will handle the whole run, making the device checks informational.
///
/// Asks the same question `platform::open_pair` will — [`sequencer_input::x11::is_usable`],
/// which connects rather than reading `$DISPLAY` — so the report cannot promise one backend
/// and the run pick the other.
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
