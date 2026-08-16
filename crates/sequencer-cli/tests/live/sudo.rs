//! The sudo-backed device tests: the parts that open `/dev/uinput` and read
//! `/dev/input` for real. They never prompt — they use a ticket the user already
//! cached (`sudo -v`, which SUDO-TEST.sh does up front) through the binary's own
//! session mode, so `elevate.rs` is exercised for real: sudo opens the devices,
//! root is dropped, the measurement runs unprivileged.

use std::process::{Command, Stdio};

use crate::harness::{self, TempConfig};

/// Whether the sudo-backed tests can run: a cached ticket to elevate with, and a
/// terminal on stdin (the binary's session mode refuses to elevate a pipeline).
fn ready() -> bool {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        eprintln!("skip: stdin is not a terminal, so session-mode sudo would refuse itself");
        return false;
    }
    let ticket = Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !ticket {
        eprintln!("skip: no cached sudo ticket — run `sudo -v` first (SUDO-TEST.sh does)");
    }
    ticket
}

/// Runs the binary with stdin INHERITED — session mode must see the terminal to
/// use the cached ticket — and everything else captured.
fn run_with_terminal(config: &TempConfig, args: &[&str]) -> (std::process::ExitStatus, String) {
    let output = Command::new(harness::bin())
        .args(args)
        .env("SEQUENCER_CONFIG_DIR", config.dir())
        .env("NO_COLOR", "1")
        .stdin(Stdio::inherit())
        .output()
        .expect("the binary runs");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status, text)
}

#[test]
#[ignore = "live: needs a cached sudo ticket for the device backend; SUDO-TEST.sh runs it in its second pass"]
fn bench_round_trips_through_uinput_with_cached_sudo() {
    let _serial = harness::serial();
    if !ready() {
        return;
    }
    let config = TempConfig::new();
    let (status, out) = run_with_terminal(&config, &["bench", "--seconds", "1"]);
    assert!(
        status.success(),
        "bench failed — `sequencer doctor` tells the machine's story: {out}"
    );
}

#[test]
#[ignore = "live: reports the real machine; SUDO-TEST.sh runs it in its second pass"]
fn doctor_reports_the_real_machine() {
    let _serial = harness::serial();
    let config = TempConfig::new();
    let (_, out) = harness::run(&config, &["doctor"]);
    assert!(
        out.contains("session:"),
        "doctor's report changed shape: {out}"
    );
}
