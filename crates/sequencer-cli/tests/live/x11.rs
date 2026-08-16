//! The manager against a real X session: grabs, injection, per-profile emergency
//! stops, and signal handling. Keys are synthesized through our own XTEST sink —
//! grabs intercept synthetic input exactly like physical input, so no external
//! tool is needed. Every chord avoids `alt` (on some setups a synthesized alt
//! never reaches the modifier state, seen in the field) and every injected
//! *target* is Pause, which every desktop ignores.

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sequencer_cli::core::emit::{Emit, EmitAction, InputSink as _};
use sequencer_cli::core::input::Key;
use sequencer_cli::core::time::Timestamp;
use sequencer_cli::input::XTestSink;

use crate::harness::{self, TempConfig};

/// Whether the X tests can run here at all; refuses outright (rather than skipping)
/// when a real manager is live, because its grabs would fight the tests'.
fn ready() -> bool {
    if !sequencer_cli::input::x11::is_usable() {
        eprintln!("skip: no usable X11 session (DISPLAY unset or unreachable)");
        return false;
    }
    if let Some(pid) = live_real_manager() {
        panic!(
            "a sequencer manager is already running (PID {pid}); quit it first — \
             Ctrl+C in its terminal, or unapply its profiles — then re-run"
        );
    }
    true
}

/// The PID in the *real* config's lock file, if that process is alive.
fn live_real_manager() -> Option<u32> {
    let config = std::env::var_os("SEQUENCER_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME").map(|base| PathBuf::from(base).join("sequencer"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/sequencer"))
        })?;
    let pid: u32 = std::fs::read_to_string(config.join("manager.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Path::new(&format!("/proc/{pid}")).exists().then_some(pid)
}

/// Presses a chord the way a hand would — mods down, key down, all up in reverse —
/// through XTEST, which the manager's grabs hear exactly like a physical press.
fn tap_chord(keys: &[Key]) {
    let mut sink = XTestSink::open().expect("XTEST is usable (ready() said so)");
    let mut emit = |action: EmitAction| {
        sink.emit(&Emit {
            at: Timestamp::ZERO,
            action,
            level: 0,
        })
        .expect("inject");
        std::thread::sleep(Duration::from_millis(15));
    };
    for &key in keys {
        emit(EmitAction::KeyDown(key));
    }
    for &key in keys.iter().rev() {
        emit(EmitAction::KeyUp(key));
    }
}

/// Polls until the log's injection count exceeds `above`, up to `secs`.
fn injections_grow(log: &Path, above: usize, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if harness::inject_count(log) > above {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A manager child on the temp config, `-vv` logs and stdout merged into one file.
/// Dropping it kills anything still running — a leaked manager holds real grabs.
struct Manager {
    child: std::process::Child,
    log: PathBuf,
}

impl Manager {
    fn start(config: &TempConfig, profiles: &[&Path]) -> Self {
        let log = config.dir().join("manager.log");
        let out = std::fs::File::create(&log).expect("log file");
        let err = out.try_clone().expect("log file handle");
        let child = Command::new(harness::bin())
            .args(["profile-apply", "-vv"])
            .args(profiles)
            .env("SEQUENCER_CONFIG_DIR", config.dir())
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("manager spawns");
        let manager = Self { child, log };
        assert!(
            harness::await_line(&manager.log, "managing", 10),
            "manager never said 'managing': {}",
            harness::read(&manager.log)
        );
        manager
    }

    fn log(&self) -> &Path {
        &self.log
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().expect("child state").is_none()
    }

    fn interrupt(&self) {
        let pid = i32::try_from(self.child.id()).expect("pid fits");
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGINT,
        )
        .expect("SIGINT lands");
    }

    /// Waits up to `secs` for the child to end on its own.
    fn ended_within(&mut self, secs: u64) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("child state") {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

const MIRROR: &str = "\
[defaults]
suppress = true
emergency_stop = \"ctrl shift f10\"
[binds.\"ctrl shift f9\"]
bind = \"pause\"
";

const LOOPER: &str = "\
[defaults]
suppress = true
emergency_stop = \"ctrl shift f11\"
[binds.\"ctrl shift f6\"]
seq = [\"PRESS pause\", \"WAIT 50ms\", \"RELEASE pause\", \"WAIT 200ms\"]
loop = \"inf\"
";

// The gated profile's trigger is deliberately never grabbed (its program never has
// focus), so its press falls through to whatever is focused — the terminal running
// the tests. Pause as the primary keeps that fall-through invisible: terminals have
// no byte sequence for it, where an F-key would leave `^[[24;6~` on the prompt.
const GATED: &str = "\
[defaults]
suppress = true
program = \"*zz-no-such-program-zz*\"
[binds.\"ctrl shift pause\"]
bind = \"f9\"
";

const CTRL_SHIFT: [Key; 2] = [Key::LeftCtrl, Key::LeftShift];

fn chord(primary: Key) -> Vec<Key> {
    let mut keys = CTRL_SHIFT.to_vec();
    keys.push(primary);
    keys
}

/// The whole multi-profile story in one sitting: announce with stop hints, stack a
/// second profile onto the live manager, fire a mirror and a loop, stop each profile
/// by its own chord (nothing is global), and watch the manager quit once its set
/// empties — leaving no link and no lock behind.
#[test]
#[ignore = "live: needs a real X session + keyboard injection; SUDO-TEST.sh runs it in its second pass"]
#[allow(clippy::too_many_lines, reason = "one story, told in order")]
fn the_manager_lifecycle_from_apply_to_empty_set_quit() {
    let _serial = harness::serial();
    if !ready() {
        return;
    }
    let config = TempConfig::new();
    let p1 = config.profile("p1.toml", MIRROR);
    let p2 = config.profile("p2.toml", LOOPER);

    let mut manager = Manager::start(&config, &[&p1]);
    let text = harness::read(manager.log());
    assert!(
        text.contains("applied: ") && text.contains("(1 binds)"),
        "{text}"
    );
    assert!(
        text.contains("to stop this script, press: ctrl shift F10"),
        "the stop hint must name p1's chord: {text}"
    );
    assert_eq!(
        text.matches("to stop this script").count(),
        1,
        "one window, one hint: the caller became the manager, so only the manager's \
         announcement carries it: {text}"
    );
    assert!(
        config.active().join("p1.toml").is_symlink(),
        "the link is the state"
    );
    assert!(
        !text.to_lowercase().contains("sudo"),
        "an X11 run never mentions sudo"
    );

    let p2_arg = p2.to_str().expect("utf-8 temp path");
    let (status, out) = harness::run(&config, &["profile-apply", p2_arg]);
    assert!(status.success(), "second apply failed: {out}");
    assert!(out.contains("adding to an existing manager (PID"), "{out}");
    assert!(
        out.contains("to stop this script, press: ctrl shift F11"),
        "the second terminal gets p2's own hint: {out}"
    );
    assert!(
        harness::await_line(manager.log(), "profile applied: p2.toml", 3),
        "the manager picks the new link up: {}",
        harness::read(manager.log())
    );

    let (status, out) = harness::run(&config, &["profile-unapply", "zzz-not-there"]);
    assert!(!status.success(), "unapplying a stranger must fail");
    assert!(
        out.contains("not applied") && out.contains("list of applied:"),
        "the miss lists what IS applied: {out}"
    );

    let before = harness::inject_count(manager.log());
    tap_chord(&chord(Key::F9));
    assert!(
        injections_grow(manager.log(), before, 3),
        "the mirror trigger fires (grab heard, tap injected)"
    );

    let before = harness::inject_count(manager.log());
    tap_chord(&chord(Key::F6));
    assert!(
        injections_grow(manager.log(), before + 4, 3),
        "the infinite loop is looping"
    );
    tap_chord(&chord(Key::F6)); // a re-press stops the loop
    std::thread::sleep(Duration::from_millis(800));
    let settled = harness::inject_count(manager.log());
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        harness::inject_count(manager.log()),
        settled,
        "re-pressing the trigger stops the loop"
    );

    tap_chord(&chord(Key::F10));
    assert!(
        harness::await_line(manager.log(), "emergency stop: p1.toml unapplied", 3),
        "p1's chord stops p1: {}",
        harness::read(manager.log())
    );
    assert!(
        !config.active().join("p1.toml").exists(),
        "p1's link is gone"
    );
    assert!(
        config.active().join("p2.toml").is_symlink(),
        "p2 survives p1's emergency — nothing is global among scripts"
    );
    assert!(
        manager.alive(),
        "the manager itself survives a per-profile stop"
    );

    tap_chord(&chord(Key::F11));
    assert!(
        harness::await_line(manager.log(), "emergency stop: p2.toml unapplied", 3),
        "p2's chord stops p2: {}",
        harness::read(manager.log())
    );
    let status = manager
        .ended_within(3)
        .expect("the manager quits once its set empties");
    assert!(
        status.success(),
        "an empty-set quit exits 0, got {status:?}"
    );
    let text = harness::read(manager.log());
    assert!(text.contains("stopped (no profiles left)"), "{text}");
    assert!(
        std::fs::read_dir(config.active()).map_or(true, |mut dir| dir.next().is_none()),
        "active/ is empty afterwards"
    );
    assert!(!config.lock_file().exists(), "the PID lock is gone");
}

/// Ctrl+C semantics plus focus gating: a program-gated profile stays dormant and
/// eats nothing; SIGINT stops everything, clears the set, and the process dies BY
/// the signal — which is what hands the user's shell its prompt back cleanly.
#[test]
#[ignore = "live: needs a real X session + keyboard injection; SUDO-TEST.sh runs it in its second pass"]
fn ctrl_c_stops_everything_and_dies_by_the_signal() {
    let _serial = harness::serial();
    if !ready() {
        return;
    }
    let config = TempConfig::new();
    let p3 = config.profile("p3.toml", GATED);

    let mut manager = Manager::start(&config, &[&p3]);
    std::thread::sleep(Duration::from_millis(500));
    let text = harness::read(manager.log());
    assert!(
        !text.contains("to stop this script"),
        "a profile without emergency_stop gets no stop hint: {text}"
    );
    assert!(
        !text.lines().any(|line| line.starts_with("active: ")),
        "a program-gated profile stays dormant without its program: {text}"
    );

    let before = harness::inject_count(manager.log());
    tap_chord(&chord(Key::Pause));
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        harness::inject_count(manager.log()),
        before,
        "a dormant profile eats no keys and injects nothing"
    );

    manager.interrupt();
    let status = manager
        .ended_within(3)
        .expect("Ctrl+C stops the manager promptly");
    assert_eq!(
        status.signal(),
        Some(nix::sys::signal::Signal::SIGINT as i32),
        "the process dies BY the signal (cooperative exit), got {status:?}"
    );
    let text = harness::read(manager.log());
    assert!(text.contains("stopped (interrupted)"), "{text}");
    assert!(
        std::fs::read_dir(config.active()).map_or(true, |mut dir| dir.next().is_none()),
        "an interrupted quit empties active/"
    );
}
