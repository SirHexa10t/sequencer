//! End-to-end tests for the command line.
//!
//! Two tiers. The in-process ones go through `dispatch` with an injected sink and pump,
//! so they run in microseconds on a machine with no display server and can assert on the
//! exact actions produced. The black-box ones spawn the real binary, and cover only what
//! the in-process tier cannot see: exit codes and real standard output.

// Test code: unwrapping is how a test reports a failure.
#![allow(clippy::unwrap_used)]

use assert_cmd::Command as ProcessCommand;
use predicates::prelude::*;

use sequencer_cli::runtime::ScriptedPump;
use sequencer_cli::{ClickerArgs, Command, Deps, DoctorArgs, dispatch, exit};
use sequencer_core::emit::EmitAction;
use sequencer_core::input::{Button, EventKind, InputEvent, Key};
use sequencer_core::testutil::VirtualClock;
use sequencer_core::time::Timestamp;
use sequencer_input::MockInjector;

fn bin() -> ProcessCommand {
    ProcessCommand::cargo_bin("sequencer").expect("binary should build")
}

// -------------------------------------------------------------------------- in-process

#[test]
fn clicking_through_dispatch_reaches_the_injected_sink() {
    let mut out = Vec::new();
    let clock = VirtualClock::new();
    let mut sink = MockInjector::new();
    let watcher = sink.clone();
    let mut pump = ScriptedPump::new([(
        Timestamp::ZERO,
        InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::F9)),
    )]);

    let mut deps = Deps::new(&mut out, &clock);
    deps.sink = Some(&mut sink);
    deps.pump = Some(&mut pump);

    let code = dispatch(&Command::Clicker(ClickerArgs::new()), &mut deps).expect("should run");

    assert_eq!(code, exit::OK);
    let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
    assert_eq!(
        actions,
        vec![
            EmitAction::ButtonDown(Button::Left),
            EmitAction::ButtonUp(Button::Left)
        ]
    );
    assert_eq!(
        watcher.release_all_calls(),
        1,
        "the drop guard must release"
    );
}

#[test]
fn a_key_binding_reaches_the_sink_as_a_key_press() {
    let mut out = Vec::new();
    let clock = VirtualClock::new();
    let mut sink = MockInjector::new();
    let watcher = sink.clone();
    let mut pump = ScriptedPump::new([(
        Timestamp::ZERO,
        InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::F9)),
    )]);

    let mut deps = Deps::new(&mut out, &clock);
    deps.sink = Some(&mut sink);
    deps.pump = Some(&mut pump);

    let args = ClickerArgs {
        key: Some(Key::F),
        ..ClickerArgs::new()
    };
    dispatch(&Command::Clicker(args), &mut deps).expect("should run");

    let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
    // The key goes down, and shutdown releases it because the hold has not elapsed.
    assert_eq!(actions.first(), Some(&EmitAction::KeyDown(Key::F)));
    assert!(
        actions.contains(&EmitAction::KeyUp(Key::F)),
        "the key must come back up: {actions:?}"
    );
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
    bin().arg("--help").assert().success();
    bin().arg("--version").assert().success();
}

#[test]
fn a_bare_invocation_shows_help_rather_than_guessing() {
    // No implied subcommand: with more modes coming, silently running the clicker would
    // be a surprise rather than a convenience.
    bin()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage").and(predicate::str::contains("clicker")));
}

#[test]
fn a_bad_rate_is_a_usage_error() {
    for bad in ["0", "-5", "banana"] {
        bin()
            .args(["clicker", "--cps", bad])
            .assert()
            .failure()
            .code(i32::from(exit::USAGE));
    }
}

#[test]
fn an_unknown_key_names_itself_in_the_error() {
    bin()
        .args(["clicker", "--activate", "nosuchkey"])
        .assert()
        .failure()
        .code(i32::from(exit::USAGE))
        .stderr(predicate::str::contains("nosuchkey"));
}

#[test]
fn a_dry_run_explains_the_settings_and_touches_nothing() {
    bin()
        .args(["clicker", "--cps", "20", "--dry-run"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("left click at 20/s")
                .and(predicate::str::contains("while f9 is held"))
                .and(predicate::str::contains("f8 quits"))
                .and(predicate::str::contains("dry run")),
        );
}

#[test]
fn the_prototypes_flags_still_work() {
    // `--toggle --cps 30 --key_press f` is a command line someone may have in their shell
    // history from the Python version.
    bin()
        .args([
            "clicker",
            "--toggle",
            "--cps",
            "30",
            "--key_press",
            "f",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("f key press at 30/s")
                .and(predicate::str::contains("after tapping f9")),
        );
}

#[test]
fn simulate_replays_a_script_and_prints_the_timeline() {
    bin()
        .args(["simulate", "tests/fixtures/hold.txt", "--until-ms", "1000"])
        .assert()
        .success()
        .stdout(
            // Eleven clicks, one every 50ms, from a 520ms hold at 20/s.
            predicate::str::contains("0 BD:left BU:left | 50 BD:left BU:left")
                .and(predicate::str::contains("| 500 BD:left BU:left"))
                .and(predicate::str::contains("11 repetitions"))
                .and(predicate::str::contains("nothing left held")),
        );
}

#[test]
fn a_broken_script_line_is_a_usage_error_that_says_which_line() {
    let dir = std::env::temp_dir().join("sequencer-cli-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let script = dir.join("broken.txt");
    std::fs::write(&script, "0 down f9\nthis is not an event\n").expect("write script");

    bin()
        .arg("simulate")
        .arg(&script)
        .assert()
        .failure()
        .code(i32::from(exit::USAGE))
        .stderr(predicate::str::contains("line 2"));

    std::fs::remove_file(&script).ok();
}

#[test]
fn a_missing_script_fails_without_a_backtrace() {
    bin()
        .args(["simulate", "/nonexistent/script.txt"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error:"));
}

#[test]
fn clicking_without_device_access_fails_with_something_actionable() {
    // Only meaningful where the devices are unavailable: on a machine that is set up,
    // `clicker` would block waiting for the trigger key rather than return. That is also
    // exactly the case whose error message needs to be good.
    if std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .is_ok()
    {
        eprintln!("skipping: this machine can open /dev/uinput, so clicker would block");
        return;
    }
    bin()
        .args(["clicker"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("doctor"));
}

#[test]
fn doctor_runs_on_a_headless_machine() {
    bin()
        .arg("doctor")
        .assert()
        .stdout(predicate::str::contains("session:"));
}
