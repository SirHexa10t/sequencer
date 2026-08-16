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

use assert_cmd::Command as ProcessCommand;
use predicates::prelude::*;

use sequencer_cli::runtime::ScriptedPump;
use sequencer_cli::{
    ClickerArgs, Command, Deps, DetectKeyArgs, DoctorArgs, GlobalArgs, ProfileApplyArgs, dispatch,
    exit,
};
use sequencer_core::emit::EmitAction;
use sequencer_core::input::{Button, EventKind, InputEvent, Key};
use sequencer_core::testutil::VirtualClock;
use sequencer_core::time::Timestamp;
use sequencer_input::MockInjector;

fn bin() -> ProcessCommand {
    ProcessCommand::cargo_bin("sequencer").expect("binary should build")
}

fn press(key: Key) -> (Timestamp, InputEvent) {
    (
        Timestamp::ZERO,
        InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(key)),
    )
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

/// The exit report's two promises: the sent line carries its own caveat — this process
/// counts what it hands to the backend and cannot see what arrives — and the run closes
/// with a stopwatch line, because the user chose when to stop and may want to know how
/// long that was.
#[test]
fn the_exit_report_caveats_sent_and_ends_with_a_stopwatch() {
    let mut out = Vec::new();
    let clock = VirtualClock::new();
    let mut sink = MockInjector::new();
    let mut pump = ScriptedPump::new([(
        Timestamp::ZERO,
        InputEvent::physical(Timestamp::ZERO, EventKind::KeyDown(Key::F9)),
    )]);

    let mut deps = Deps::new(&mut out, &clock);
    deps.sink = Some(&mut sink);
    deps.pump = Some(&mut pump);
    dispatch(&Command::Clicker(ClickerArgs::new()), &mut deps).expect("should run");

    let report = String::from_utf8(out).expect("the report is text");
    assert!(
        report.contains("sent") && report.contains("not all may arrive"),
        "the sent line must say the count is what was sent, not what arrived: {report}"
    );
    assert!(
        report.lines().last().unwrap().starts_with("ran "),
        "the last line is the stopwatch: {report}"
    );
}

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
        kb_key: Some(Key::F),
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
fn the_prototypes_flags_still_work() {
    // `--toggle --cps 30 --key_press f` is a command line someone may have in their shell
    // history from the Python version. In-process with an injected pair, so the flags are
    // proven to take EFFECT — the emitted action is a key press, not the default click —
    // rather than merely to parse.
    use sequencer_cli::clap::Parser as _;
    let cli = sequencer_cli::Cli::try_parse_from([
        "sequencer",
        "clicker",
        "--toggle",
        "--cps",
        "30",
        "--key_press",
        "f",
    ])
    .expect("the prototype's flags should parse");

    let mut out = Vec::new();
    let clock = VirtualClock::new();
    let mut sink = MockInjector::new();
    let watcher = sink.clone();
    // Toggle latches on at the release, and the engine phase-locks to the event's own
    // timestamp — so both edges sit at zero, where the virtual clock already is. The
    // pump then ends and shutdown releases whatever the toggle began.
    let mut pump = ScriptedPump::new([
        press(Key::F9),
        (
            Timestamp::ZERO,
            InputEvent::physical(Timestamp::ZERO, EventKind::KeyUp(Key::F9)),
        ),
    ]);
    let mut deps = Deps::new(&mut out, &clock);
    deps.sink = Some(&mut sink);
    deps.pump = Some(&mut pump);
    dispatch(&cli.command, &mut deps).expect("should run");

    let actions: Vec<EmitAction> = watcher.recorded().iter().map(|e| e.action).collect();
    assert!(
        actions.contains(&EmitAction::KeyDown(Key::F)),
        "--key_press f must emit key presses: {actions:?}"
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, EmitAction::ButtonDown(_))),
        "no default clicks once --key_press takes effect: {actions:?}"
    );
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
        global: GlobalArgs::new(),
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
}

#[test]
fn an_invalid_profile_is_a_usage_error_that_names_the_problem() {
    let dir = std::env::temp_dir().join("sequencer-cli-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("dangling-hold.toml");
    std::fs::write(&file, "[binds.F6]\nseq = [\"PRESS ctrl\"]\n").expect("write profile");

    bin()
        .arg("profile-apply")
        .arg(&file)
        .assert()
        .failure()
        .code(i32::from(exit::USAGE))
        .stderr(predicate::str::contains("never RELEASEd"));

    std::fs::remove_file(&file).ok();
}

#[test]
fn a_missing_profile_fails_without_a_backtrace() {
    bin()
        .args(["profile-apply", "/nonexistent/binds.toml"])
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
