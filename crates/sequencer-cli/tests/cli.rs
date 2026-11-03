//! End-to-end tests for the command line.
//!
//! Two tiers. The in-process ones go through `dispatch` with an injected sink and pump,
//! so they run in microseconds on a machine with no display server and can assert on the
//! exact actions produced. The black-box ones spawn the real binary, and cover only what
//! the in-process tier cannot see: exit codes and real standard output.

// Everything here drives the clap surface, which the `cli` feature owns. Without it the
// crate exports no parser to test, so the whole file stands down rather than failing to
// compile a build that is deliberately clap-free.
#![cfg(feature = "cli")]
// Test code: unwrapping is how a test reports a failure.
#![allow(clippy::unwrap_used)]

use std::process::{ExitStatus, Stdio};

use sequencer_cli::runtime::ScriptedPump;
use sequencer_cli::{Command, Deps, DetectKeyArgs, DoctorArgs, ProfileApplyArgs, dispatch, exit};
use sequencer_core::emit::EmitAction;
use sequencer_core::input::{Button, EventKind, InputEvent, Key};
use sequencer_core::testutil::VirtualClock;
use sequencer_core::time::Timestamp;
use sequencer_input::MockInjector;

/// How one run of the real binary ended, and what it printed on each stream.
struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// Runs the binary cargo built for this test target to completion, stdin closed.
///
/// Plain `std::process`, the same way the live suite's harness does it: exit codes and
/// stream contents are all these checks need.
fn run(args: &[&str]) -> Finished {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sequencer"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    Finished {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn press(key: Key) -> (Timestamp, InputEvent) {
    (
        Timestamp::ZERO,
        InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(key)),
    )
}

// -------------------------------------------------------------------------- in-process

/// detect-key's contract: every press prints its bindable name exactly once, releases
/// print nothing, and mouse buttons use the binds spelling (`mouse1`), not `left` —
/// which would be indistinguishable from the arrow key.
#[test]
fn detect_key_names_each_press_once_and_nothing_else() {
    let mut out = Vec::new();
    let clock = VirtualClock::new();
    let mut pump = ScriptedPump::new(
        [
            EventKind::KeyDown(Key::F9),
            EventKind::KeyUp(Key::F9),
            EventKind::ButtonDown(Button::Left),
            EventKind::ButtonUp(Button::Left),
            EventKind::KeyDown(Key::F9),
        ]
        .map(|kind| (Timestamp::ZERO, InputEvent::physical(Timestamp::ZERO, kind))),
    );

    let mut deps = Deps::new(&mut out, &clock);
    deps.pump = Some(&mut pump);
    let code = dispatch(&Command::DetectKey(DetectKeyArgs::new()), &mut deps).expect("should run");

    assert_eq!(code, exit::OK);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.ends_with("F9\nmouse1\nF9\n"),
        "one line per press, none per release: {text}"
    );
    assert!(
        text.contains("capslock"),
        "the illustrative keyboard prints first: {text}"
    );
    // The reference is the ONLY place these sets are listed now — the binds template
    // points here instead of repeating them — so their presence is a contract.
    for set_member in [
        "wheel-up",
        "pad-south",
        "volume-up",
        "mouse4",
        "rctrl",
        "hid:",
    ] {
        assert!(
            text.contains(set_member),
            "the printed reference must list `{set_member}`: {text}"
        );
    }
}

#[test]
fn doctor_reports_the_session_and_never_panics() {
    let mut out = Vec::new();
    let clock = VirtualClock::new();
    let mut deps = Deps::new(&mut out, &clock);

    let code = dispatch(&Command::Doctor(DoctorArgs::new()), &mut deps).expect("should report");

    let report = String::from_utf8(out).expect("utf-8");
    assert!(report.contains("session:"), "{report}");
    assert!(
        code == exit::OK || code == exit::FAILURE,
        "unexpected exit {code}"
    );
}

#[test]
fn doctor_explains_the_cost_of_the_access_it_asks_for() {
    let clock = VirtualClock::new();
    let mut out = Vec::new();
    let mut deps = Deps::new(&mut out, &clock);
    dispatch(&Command::Doctor(DoctorArgs::new()), &mut deps).expect("should report");
    let report = String::from_utf8(out).expect("utf-8");

    // On a machine that is already set up there is nothing to remediate, so this only
    // asserts the pairing: if the report asks for the `input` group, it must also say
    // what that group can do. Asking for keylogging-capable access without saying so
    // would not be a fair trade.
    if report.contains("usermod -aG input") {
        assert!(
            report.contains("keylogging"),
            "asked for the input group without explaining it:\n{report}"
        );
    }
}

// ------------------------------------------------------------------------------ process

#[test]
fn help_and_version_succeed() {
    let help = run(&["--help"]);
    assert!(help.status.success(), "{}", help.stderr);
    let version = run(&["--version"]);
    assert!(version.status.success(), "{}", version.stderr);
}

#[test]
fn a_bare_invocation_shows_help_rather_than_guessing() {
    // No implied subcommand: with more modes coming, silently running one of them would
    // be a surprise rather than a convenience.
    let bare = run(&[]);
    assert!(!bare.status.success());
    assert!(bare.stderr.contains("Usage"), "{}", bare.stderr);
    assert!(bare.stderr.contains("profile-apply"), "{}", bare.stderr);
}

#[test]
fn a_bad_rate_is_a_usage_error() {
    for bad in ["0", "-5", "banana"] {
        let refused = run(&["bench", "--cps", bad]);
        assert_eq!(
            refused.status.code(),
            Some(i32::from(exit::USAGE)),
            "--cps {bad}: {}",
            refused.stderr
        );
    }
}

/// The shipped template is a runnable profile: profile-apply accepts it and its PgUp
/// mirror actually fires against the injected pair. Its chord bind is grabbable now,
/// so nothing about the file is skipped.
#[test]
fn profile_apply_accepts_the_shipped_template() {
    let mut out = Vec::new();
    let clock = VirtualClock::new();
    let mut sink = MockInjector::new();
    let watcher = sink.clone();
    let mut pump = ScriptedPump::new([
        press(Key::PageUp),
        (
            Timestamp::ZERO,
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyUp(Key::PageUp)),
        ),
    ]);
    let mut deps = Deps::new(&mut out, &clock);
    deps.sink = Some(&mut sink);
    deps.pump = Some(&mut pump);

    let args = ProfileApplyArgs {
        files: vec![concat!(env!("CARGO_MANIFEST_DIR"), "/../../example_profile.toml").into()],
        ..ProfileApplyArgs::new()
    };
    let code = dispatch(&Command::ProfileApply(args), &mut deps).expect("should run");

    assert_eq!(code, exit::OK);
    let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
    assert_eq!(
        actions,
        vec![
            EmitAction::KeyDown(Key::VolumeUp),
            EmitAction::KeyUp(Key::VolumeUp)
        ],
        "the template's PgUp mirror must fire"
    );
    assert_eq!(
        watcher.release_all_calls(),
        1,
        "the drop guard must release"
    );
}

#[test]
fn an_invalid_profile_is_a_usage_error_that_names_the_problem() {
    let dir = std::env::temp_dir().join(format!("sequencer-cli-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("dangling-hold.toml");
    std::fs::write(&file, "[binds.F6]\nseq = [\"PRESS ctrl\"]\n").expect("write profile");

    let refused = run(&[
        "profile-apply",
        file.to_str().expect("the temp path is UTF-8"),
    ]);
    assert_eq!(
        refused.status.code(),
        Some(i32::from(exit::USAGE)),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("never RELEASEd"),
        "{}",
        refused.stderr
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_missing_profile_fails_without_a_backtrace() {
    let missing = run(&["profile-apply", "/nonexistent/binds.toml"]);
    assert!(!missing.status.success());
    assert!(missing.stderr.contains("error:"), "{}", missing.stderr);
}

#[test]
fn doctor_runs_on_a_headless_machine() {
    let report = run(&["doctor"]);
    assert!(report.stdout.contains("session:"), "{}", report.stdout);
}
