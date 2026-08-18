//! The manager against a real X session: grabs, injection, full-chord routing,
//! lifted taps, per-profile emergency stops, and signal handling. Keys are
//! synthesized through our own XTEST sink — grabs intercept synthetic input exactly
//! like physical input, so no external tool is needed. Every chord avoids `alt` (on
//! some setups a synthesized alt never reaches the modifier state, seen in the
//! field) and every injected *target* is Pause, which every desktop ignores.

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
    press_keys(keys);
    release_keys(keys);
}

/// Presses `keys` in order and leaves them down — the "hand still on the chord"
/// half a deferred tap waits out. Pair with [`release_keys`].
fn press_keys(keys: &[Key]) {
    inject(keys.iter().map(|&key| EmitAction::KeyDown(key)));
}

/// Releases `keys` in reverse order.
fn release_keys(keys: &[Key]) {
    inject(keys.iter().rev().map(|&key| EmitAction::KeyUp(key)));
}

fn inject(actions: impl Iterator<Item = EmitAction>) {
    let mut sink = XTestSink::open().expect("XTEST is usable (ready() said so)");
    for action in actions {
        sink.emit(&Emit {
            at: Timestamp::ZERO,
            action,
            level: 0,
        })
        .expect("inject");
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// The injection count once it has stopped moving — a snapshot taken mid-tap would
/// blame the tap's second half on whatever gets asserted next (seen in the field:
/// a "parked tap fired after the stop" false alarm that was the previous tap's
/// key-up landing late). Capped so a genuinely busy log fails loudly, not silently.
fn settled_injections(log: &Path) -> usize {
    let mut last = harness::inject_count(log);
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(400));
        let now = harness::inject_count(log);
        if now == last {
            return now;
        }
        last = now;
    }
    last
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
        // This runs on assertion failures too, so: graceful SIGINT first — the
        // manager's own teardown releases held keys, drops every grab and clears
        // its state, leaving nothing listening behind a red test — then SIGKILL
        // only if that is ignored. And never panic: this Drop runs during panics.
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGINT,
            );
        }
        for _ in 0..40 {
            if !matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The second bind shares its primary key with the profile's own emergency chord on
// purpose: pressing ctrl+F10 must run the bind, and only ctrl+shift+F10 may stop the
// profile — the field bug where any grab on a stop chord's primary stopped it. The
// bare F9 bind coexists with the ctrl+shift+F9 chord: exact masks, disjoint grabs.
const MIRROR: &str = "\
[defaults]
suppress = true
emergency_stop = \"ctrl shift f10\"
[binds.f9]
bind = \"pause\"
[binds.\"ctrl shift f9\"]
bind = \"pause\"
[binds.\"ctrl f10\"]
bind = \"pause\"
[binds.\"ctrl f8\"]
bind = \"ctrl shift pause\"
";

const LOOPER: &str = "\
[defaults]
suppress = true
emergency_stop = \"ctrl shift f11\"
[binds.\"ctrl shift f6\"]
seq = [\"PRESS pause\", \"WAIT 50ms\", \"RELEASE pause\", \"WAIT 200ms\"]
loop = \"inf\"
";

// Forms a circle with LOOPER across FILES: the looper PRESSes pause (this trigger),
// and this target is the looper's own trigger chord. Legal alone; refused together.
const CYCLER: &str = "\
[defaults]
suppress = true
[binds.pause]
bind = \"ctrl shift f6\"
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
        text.contains("applied: ") && text.contains("(4 binds)"),
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

    // A bare key and a chord over it are disjoint exact grabs now: bare F9 fires its
    // own bind here, while ctrl+shift+F9 (exercised below) routes to the chord's.
    let before = harness::inject_count(manager.log());
    tap_chord(&[Key::F9]);
    assert!(
        injections_grow(manager.log(), before, 3),
        "the bare F9 bind fires on a bare press"
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

    // Injections pass through grabs, so a feedback circle can span PROFILES: p4's
    // pause bind would be fed by p2's looper and would feed it back. Refused by the
    // apply command before anything links — the live set stays untouched.
    let p4 = config.profile("p4.toml", CYCLER);
    let p4_arg = p4.to_str().expect("utf-8 temp path");
    let (status, out) = harness::run(&config, &["profile-apply", p4_arg]);
    assert!(
        !status.success(),
        "a cross-profile circle must refuse: {out}"
    );
    assert!(out.contains("circle"), "{out}");
    assert!(
        out.contains("p2.toml::") && out.contains("p4.toml::"),
        "the message names both profiles: {out}"
    );
    assert!(
        !config.active().join("p4.toml").exists(),
        "nothing was linked"
    );

    let (status, out) = harness::run(&config, &["profile-unapply", "zzz-not-there"]);
    assert!(!status.success(), "unapplying a stranger must fail");
    assert!(
        out.contains("not applied") && out.contains("list of applied:"),
        "the miss lists what IS applied: {out}"
    );

    // The mirror's target (pause) does not name the trigger's ctrl+shift, so a plain
    // injection would recolour it. The tap therefore fires IMMEDIATELY between a
    // lift and a restore of the held modifiers — the live proof of the LiftedTap:
    // a misread keymap lifts the wrong keys, a missing restore strands the chord
    // (the second press would miss the grab), and a leftover park would fire again
    // on release. Two presses under one hold, then a quiet release.
    let before = harness::inject_count(manager.log());
    press_keys(&CTRL_SHIFT);
    tap_chord(&[Key::F9]);
    assert!(
        injections_grow(manager.log(), before, 3),
        "the tap fires at once, between lifted modifiers"
    );
    let after_first = settled_injections(manager.log());
    tap_chord(&[Key::F9]);
    assert!(
        injections_grow(manager.log(), after_first, 3),
        "a second press under the same hold fires too — the restore kept the grab alive"
    );
    let settled = settled_injections(manager.log());
    release_keys(&CTRL_SHIFT);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        harness::inject_count(manager.log()),
        settled,
        "releasing the modifiers fires nothing extra — no tap is parked anymore"
    );

    let before = settled_injections(manager.log());
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

    // Re-applying a live profile replaces its link and the manager reloads it —
    // edit-and-reapply, no unapply needed. The attach window says what happened,
    // repeats the stop hint, and the manager's own log shows the update.
    let p1_arg = p1.to_str().expect("utf-8 temp path");
    let (status, out) = harness::run(&config, &["profile-apply", p1_arg]);
    assert!(status.success(), "re-apply failed: {out}");
    assert!(out.contains("already applied:"), "{out}");
    assert!(
        out.contains("updating (re-applying) onto an existing manager (PID"),
        "{out}"
    );
    assert!(
        out.contains("to stop this script, press: ctrl shift F10"),
        "the re-apply repeats the stop hint: {out}"
    );
    assert!(
        harness::await_line(manager.log(), "profile updated: p1.toml (4 binds)", 3),
        "the manager reloads the replaced link: {}",
        harness::read(manager.log())
    );

    // The target names MORE than the trigger holds: shift is synthesized, but the
    // held ctrl is the hand's own — re-injecting it would end with a synthetic
    // release that strands the physical ctrl logically-up, and the SECOND press
    // here would miss the grab (the field bug: worked once, then went dead).
    let before = settled_injections(manager.log());
    press_keys(&[Key::LeftCtrl]);
    tap_chord(&[Key::F8]);
    assert!(
        injections_grow(manager.log(), before, 3),
        "ctrl+F8 fires its extra-modifier target"
    );
    let after_first = settled_injections(manager.log());
    tap_chord(&[Key::F8]);
    let second_fired = injections_grow(manager.log(), after_first, 3);
    release_keys(&[Key::LeftCtrl]);
    assert!(
        second_fired,
        "the second press under the same held ctrl must still fire — a re-injected \
         ctrl would have wiped the held state"
    );

    // ctrl+F10 shares the emergency's primary key but not its modifiers: it must run
    // its own bind — fired between a lift and restore of our injected ctrl — and
    // stop nothing.
    let before = settled_injections(manager.log());
    tap_chord(&[Key::LeftCtrl, Key::F10]);
    assert!(
        injections_grow(manager.log(), before, 3),
        "ctrl+F10 runs its bind rather than being mistaken for the stop chord"
    );
    assert!(
        config.active().join("p1.toml").is_symlink(),
        "a trigger sharing the stop chord's primary key must not stop the profile"
    );

    // A lifted tap must not confuse the stop path: with ctrl+shift held, F9 fires
    // its tap at once, and F10 — completing the exact emergency chord, since the
    // restore left ctrl+shift down — must stop p1. Nothing may fire after the stop.
    let before = settled_injections(manager.log());
    press_keys(&CTRL_SHIFT);
    tap_chord(&[Key::F9]);
    assert!(
        injections_grow(manager.log(), before, 3),
        "the lifted tap fires under the held chord"
    );
    let settled = settled_injections(manager.log());
    tap_chord(&[Key::F10]);
    release_keys(&CTRL_SHIFT);
    assert!(
        harness::await_line(manager.log(), "emergency stop: p1.toml unapplied", 3),
        "p1's chord stops p1 right after a lifted tap: {}",
        harness::read(manager.log())
    );
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        harness::inject_count(manager.log()),
        settled,
        "nothing fires after the stop"
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
