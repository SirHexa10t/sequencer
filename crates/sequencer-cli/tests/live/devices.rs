//! The manager on the device backend, below the desktop. The rig's keyboard types, and the
//! rig's watch reads back exactly what sequencer's virtual keyboard sends — grabbed, so
//! nothing reaches the session; the rig's mouse clicks and moves, and a watch on its stand-in
//! reads back what passes on, grabbed too. Each manager holds the rig's devices alone
//! (`SEQUENCER_TEST_GRAB_ONLY`), so the user's keyboards and mice are never touched.
//!
//! What was sent is judged by what it typed — each key that went down, with the modifiers
//! the virtual keyboard held as it did — and by everything coming back up. How a lifted tap
//! interleaves its lifts and restores is the executor's business, not the contract.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use sequencer_cli::core::input::{Button, Key, Mods};

use crate::harness::{self, Manager, TempConfig};
use crate::rig::{Passed, Rig};

/// T2's profile: a plain letter bound to another.
pub(crate) const LETTER: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.j]
bind = \"x\"
";

/// T3's: a Ctrl chord, whose target must come out without the Ctrl.
pub(crate) const CTRL: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.\"ctrl j\"]
bind = \"x\"
";

/// T4's: the same with Shift.
pub(crate) const SHIFT: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.\"shift j\"]
bind = \"x\"
";

/// T5's: a sequence with a shifted character in it.
pub(crate) const SEQUENCE: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.F9]
seq = [\"h\", \"i\", \"shift 1\"]
";

/// T10's: Shift held well before and after its key.
pub(crate) const SLOW_SHIFT: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.F9]
seq = [\"PRESS shift\", \"WAIT 100ms\", \"1\", \"WAIT 100ms\", \"RELEASE shift\"]
";

/// T6's: a chord bound to nothing, which must reach nothing.
pub(crate) const SWALLOWED: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.\"ctrl k\"]
bind = \"NOP\"
";

/// T1's: a key that types no character, bound — and F8, the emergency stop.
pub(crate) const SPECIAL: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.F9]
bind = \"x\"
";

/// A chain's first link: j sends F9.
const CHAIN_FIRST: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.j]
bind = \"F9\"
";

/// Its second: F9 — the hand's, or the first link's — sends x.
const CHAIN_SECOND: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.F9]
bind = \"x\"
";

/// How far apart a sequence's `WAIT 100ms` must keep the keys around it, read back through
/// the rig: the wait, less the rig's own jitter.
const WAIT_KEEPS_APART: Duration = Duration::from_millis(80);

/// The lifecycle's first profile: a letter bound, and a chord over its own stop chord's
/// primary key, which must run its bind and stop nothing.
const LIFE_MIRROR: &str = "\
[defaults]
emergency_stop = \"ctrl shift end\"
[binds.j]
bind = \"x\"
[binds.\"ctrl end\"]
bind = \"y\"
";

/// The second, stacked onto the running manager from another terminal: an endless loop its
/// trigger starts and stops, and a stop chord of its own.
const LIFE_LOOPER: &str = "\
[defaults]
emergency_stop = \"ctrl shift f11\"
[binds.F9]
loop = \"inf\"
gap = \"100ms\"
seq = [\"z\"]
";

/// With the looper, a circle across files: its trigger is the looper's output, and its
/// output the looper's trigger. Legal alone; refused together.
const LIFE_CYCLER: &str = "\
[binds.z]
bind = \"F9\"
";

/// How long the looper is left running: several of its 100 ms gaps.
const LOOP_RUNS_FOR: Duration = Duration::from_millis(600);

/// How long the supervisor may take to let a silent manager's grab go: its own quiet
/// period (`supervise::QUIET`, two seconds), and a margin.
const QUIET_RELEASE_WITHIN: Duration = Duration::from_secs(5);

/// A quarter of the quiet period: long enough in that the supervisor has noticed the
/// silence, short enough that it mustn't have acted on it yet.
const SILENCE_BEGUN: Duration = Duration::from_millis(500);

/// How long a keyboard plugged in may wait to be held: the manager's look for new devices,
/// about a second apart, and a margin.
const PLUGGED_IN_HELD_WITHIN: Duration = Duration::from_secs(5);

/// How long a killed manager's grab may stay: the supervisor hears the link close at once,
/// so this is only the margin a loaded machine needs.
const KILLED_RELEASE_WITHIN: Duration = Duration::from_secs(2);

/// A mouse trigger: the back (thumb) button, bound to a letter.
const BACK_BUTTON: &str = "\
[defaults]
emergency_stop = \"F8\"
[binds.mouse4]
bind = \"x\"
";

/// The same, stacked onto a running manager: without a stop chord, which would collide with
/// the first profile's.
const BACK_BUTTON_STACKED: &str = "\
[binds.mouse4]
bind = \"x\"
";

/// How long a manager runs before a mouse it should leave alone is checked: past its first
/// look for mice — at once, then about a second apart — with a margin.
const MICE_LOOKED_AT_BY: Duration = Duration::from_millis(1500);

/// The optional udev rule for stand-ins, as the README installs it.
const STAND_IN_RULE: &str = "69-sequencer-stand-in.rules";

/// Where udev reads rules from: the rule is installed when one of these has it.
const UDEV_RULE_DIRS: [&str; 3] = [
    "/etc/udev/rules.d",
    "/run/udev/rules.d",
    "/usr/lib/udev/rules.d",
];

/// How long udev may take to finish with a new stand-in, once it is there.
const UDEV_DONE_WITHIN: Duration = Duration::from_secs(5);

/// Whether the device tiers can run here, with the rig started when they can; refuses
/// outright while a real manager runs, as the X11 tier does.
pub(crate) fn ready() -> Option<Rig> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        eprintln!("skip: stdin is not a terminal, so session-mode sudo would refuse itself");
        return None;
    }
    if !wayland_session() {
        eprintln!(
            "skip: not a Wayland session, where alone `profile-apply` asks sudo for the devices"
        );
        return None;
    }
    if let Some(pid) = harness::live_real_manager() {
        panic!(
            "a sequencer manager is already running (PID {pid}); quit it first — Ctrl+C in \
             its terminal, or unapply its profiles — then re-run"
        );
    }
    refuse_leftover_devices();
    Rig::start()
}

/// Refuses outright while a sequencer virtual keyboard or a rig device — or a stand-in for
/// one — already exists: a manager or rig from an earlier run is alive, and its devices
/// would pass for the test's own — a watch on its virtual keyboard hears nothing, while the
/// test's output goes on to the session (seen when a frozen run left one behind).
fn refuse_leftover_devices() {
    use sequencer_cli::input::linux::grab::KEYBOARD_NAME;
    use sequencer_cli::input::linux::rig::TEST_DEVICES;

    let leftovers: Vec<String> = sequencer_cli::input::devices::list()
        .into_iter()
        .filter(|device| device.name == KEYBOARD_NAME || device.name.starts_with(TEST_DEVICES))
        .map(|device| format!("{} ({})", device.name, device.node))
        .collect();
    assert!(
        leftovers.is_empty(),
        "devices of an earlier sequencer run are still there — {}; end what keeps them, then \
         re-run: sudo pkill -KILL -f '[s]equencer-live-rig|[s]equencer profile-apply|--[s]equencer-supervisor'",
        leftovers.join(", ")
    );
}

/// Whether this is a Wayland session: the question `profile-apply` asks before it elevates
/// for the devices (the CLI's own `session::wayland_session`, which is not public), asked
/// of the same two facts — `$WAYLAND_DISPLAY`, or an X server that is Xwayland.
fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || sequencer_cli::input::x11::is_xwayland()
}

/// A manager on the devices running `profile`, its virtual keyboard watched by `rig`.
fn watched(config: &TempConfig, rig: &mut Rig, profile: &str) -> Manager {
    let manager = Manager::start_on_devices(config, &[&config.profile("profile.toml", profile)]);
    rig.watch();
    manager
}

/// Each key `sent` pressed that isn't a modifier, with the modifiers down as it went down.
fn typed(sent: &[(Key, i32)]) -> Vec<(Key, BTreeSet<Key>)> {
    let mut held = BTreeSet::new();
    let mut typed = Vec::new();
    for &(key, value) in sent {
        match value {
            1 if Mods::of_key(key).is_some() => {
                held.insert(key);
            }
            1 => typed.push((key, held.clone())),
            0 => {
                held.remove(&key);
            }
            _ => {}
        }
    }
    typed
}

/// `count` presses of `key` with no modifier down.
fn clean(key: Key, count: usize) -> Vec<(Key, BTreeSet<Key>)> {
    vec![(key, BTreeSet::new()); count]
}

/// The keys `sent` pressed and never let up again.
fn left_down(sent: &[(Key, i32)]) -> BTreeSet<Key> {
    let mut down = BTreeSet::new();
    for &(key, value) in sent {
        match value {
            1 => {
                down.insert(key);
            }
            0 => {
                down.remove(&key);
            }
            _ => {}
        }
    }
    down
}

/// T2: a bound letter types its target, and the letter itself goes nowhere — while a key
/// no profile binds passes through untouched.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_bound_letter_types_its_target_and_never_itself() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let _manager = watched(&config, &mut rig, LETTER);
    for _ in 0..3 {
        rig.tap(Key::J);
    }
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::X, 3), "{sent:?}");
    assert!(left_down(&sent).is_empty(), "{sent:?}");
    rig.tap(Key::A);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::A, 1), "{sent:?}");
    assert!(left_down(&sent).is_empty(), "{sent:?}");
}

/// T3, T4: a chord's target comes out clean of the modifier the hand still holds, and the
/// chord's key goes nowhere.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_chord_types_its_target_clean_of_its_modifier() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    for (modifier, profile) in [(Key::LeftCtrl, CTRL), (Key::LeftShift, SHIFT)] {
        let config = TempConfig::new();
        let manager = watched(&config, &mut rig, profile);
        for _ in 0..3 {
            rig.chord(&[modifier, Key::J]);
        }
        let sent = rig.sent();
        assert_eq!(typed(&sent), clean(Key::X, 3), "{modifier} j: {sent:?}");
        assert!(left_down(&sent).is_empty(), "{modifier} j: {sent:?}");
        drop(manager);
    }
}

/// T9: a modifier held across several fires carries every one, each target still clean.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_held_modifier_carries_every_fire() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let _manager = watched(&config, &mut rig, CTRL);
    rig.press(Key::LeftCtrl);
    for _ in 0..3 {
        rig.tap(Key::J);
    }
    rig.release(Key::LeftCtrl);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::X, 3), "{sent:?}");
    assert!(left_down(&sent).is_empty(), "{sent:?}");
}

/// T5, T10: a sequence types in order, its shifted character with Shift down — whether the
/// Shift comes with the step or is held around it.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_sequence_types_in_order_with_its_shift() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let shifted = || BTreeSet::from([Key::LeftShift]);
    for (profile, expected) in [
        (
            SEQUENCE,
            vec![
                (Key::H, BTreeSet::new()),
                (Key::I, BTreeSet::new()),
                (Key::Num1, shifted()),
            ],
        ),
        (SLOW_SHIFT, vec![(Key::Num1, shifted())]),
    ] {
        let config = TempConfig::new();
        let manager = watched(&config, &mut rig, profile);
        rig.tap(Key::F9);
        let sent = rig.sent();
        assert_eq!(typed(&sent), expected, "{profile}{sent:?}");
        assert!(left_down(&sent).is_empty(), "{sent:?}");
        drop(manager);
    }
}

/// A sequence's waits are kept in what it sends, not just its order: each event leaves as it
/// is sent, so Shift goes down a WAIT before the 1 and comes up a WAIT after it — where a
/// staged sink once let a whole pass out at its end, every key at once.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_sequences_waits_keep_its_keys_apart() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let _manager = watched(&config, &mut rig, SLOW_SHIFT);
    rig.tap(Key::F9);
    let sent = rig.sent_timed();
    let when = |key: Key, value: i32| {
        sent.iter()
            .find(|(_, sent_key, sent_value)| (*sent_key, *sent_value) == (key, value))
            .map_or_else(
                || panic!("{key} {value} never came: {sent:?}"),
                |(at, _, _)| *at,
            )
    };
    let shift_down = when(Key::LeftShift, 1);
    let one_down = when(Key::Num1, 1);
    let one_up = when(Key::Num1, 0);
    let shift_up = when(Key::LeftShift, 0);
    assert!(
        one_down.duration_since(shift_down) >= WAIT_KEEPS_APART,
        "the first WAIT was squeezed out: {sent:?}"
    );
    assert!(
        shift_up.duration_since(one_up) >= WAIT_KEEPS_APART,
        "the second WAIT was squeezed out: {sent:?}"
    );
}

/// One profile's output fires another's trigger, as on X11's grabs: j sends F9, F9 sends x,
/// and only the x reaches the session — the F9 is kept, as a hand's would be.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_profiles_output_fires_anothers_trigger() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let first = config.profile("first.toml", CHAIN_FIRST);
    let second = config.profile("second.toml", CHAIN_SECOND);
    let _manager = Manager::start_on_devices(&config, &[&first, &second]);
    rig.watch();
    rig.tap(Key::J);
    let sent = rig.sent();
    assert_eq!(
        typed(&sent),
        clean(Key::X, 1),
        "j → F9 → x, and nothing else: {sent:?}"
    );
    assert!(left_down(&sent).is_empty(), "{sent:?}");
}

/// T6: a chord bound to nothing reaches nothing; only the hand's own Ctrl passes.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_chord_bound_to_nothing_reaches_nothing() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let _manager = watched(&config, &mut rig, SWALLOWED);
    rig.chord(&[Key::LeftCtrl, Key::K]);
    let sent = rig.sent();
    // Exactly the hand's Ctrl, down and up: a watch that heard nothing at all fails here.
    assert_eq!(sent, [(Key::LeftCtrl, 1), (Key::LeftCtrl, 0)]);
}

/// A trigger held until the kernel repeats it fires once: the repeats are kept, and the
/// bind is the press's.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_trigger_held_down_fires_once() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let _manager = watched(&config, &mut rig, LETTER);
    rig.press(Key::J);
    for _ in 0..5 {
        rig.repeat(Key::J);
    }
    rig.release(Key::J);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::X, 1), "{sent:?}");
    assert!(left_down(&sent).is_empty(), "{sent:?}");
}

/// T1: the emergency stop is kept from the session like any trigger, ends the profile —
/// and with the set empty, the manager — and the keyboard is free again.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn the_emergency_stop_ends_the_manager_and_frees_the_keyboard() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let mut manager = watched(&config, &mut rig, SPECIAL);
    rig.tap(Key::F9);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::X, 1), "{sent:?}");
    assert!(rig.held(), "the manager holds the rig's keyboard");
    rig.tap(Key::F8);
    assert!(
        manager.ended_within(10).is_some(),
        "the set emptied, so the manager should have quit: {}",
        harness::read(manager.log())
    );
    let sent = rig.sent();
    assert!(
        sent.iter().all(|(key, _)| *key != Key::F8),
        "the stop key reached the session: {sent:?}"
    );
    assert!(!rig.held(), "the keyboard is the session's again");
}

/// The manager's whole life on the devices, as the X11 tier tells it on X11's grabs: a
/// second profile stacked on from another terminal, a bind and an endless loop, a circle
/// across files refused, an edit re-applied, a chord over the stop chord's own key that
/// stops nothing, each profile stopped by its own chord — nothing global among them — and
/// the manager gone once its set is empty, leaving no link, no lock held, and the keyboard
/// the session's again.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
#[allow(clippy::too_many_lines, reason = "one story, told in order")]
fn the_manager_lifecycle_on_the_devices() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let p1 = config.profile("p1.toml", LIFE_MIRROR);
    let p2 = config.profile("p2.toml", LIFE_LOOPER);
    let mut manager = Manager::start_on_devices(&config, &[&p1]);
    rig.watch();
    let text = harness::read(manager.log());
    assert_eq!(
        text.matches("to stop this script, press: ctrl shift end")
            .count(),
        1,
        "the manager's announcement names p1's chord, once: {text}"
    );
    rig.tap(Key::J);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::X, 1), "p1's bind: {sent:?}");

    // Stacked on from another terminal, which only links and reports: the manager, on the
    // devices already, picks the link up and claims its triggers.
    let p2_arg = p2.to_str().expect("utf-8 temp path");
    let (status, out) = harness::run(&config, &["profile-apply", p2_arg]);
    assert!(status.success(), "the second apply failed: {out}");
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

    // The loop runs from one press of its trigger to the next, the trigger itself kept.
    rig.tap(Key::F9);
    std::thread::sleep(LOOP_RUNS_FOR);
    rig.tap(Key::F9);
    let sent = rig.sent();
    let loops = typed(&sent);
    assert!(
        loops.len() >= 3
            && loops
                .iter()
                .all(|typed| *typed == (Key::Z, BTreeSet::new())),
        "the loop typed z, clean, again and again: {sent:?}"
    );
    assert!(left_down(&sent).is_empty(), "{sent:?}");
    assert!(
        rig.sent().is_empty(),
        "the second press of its trigger stopped the loop"
    );

    // A circle across files is refused before anything links; the live set stays as it was.
    let p4 = config.profile("p4.toml", LIFE_CYCLER);
    let (status, out) = harness::run(
        &config,
        &["profile-apply", p4.to_str().expect("utf-8 temp path")],
    );
    assert!(!status.success() && out.contains("circle"), "{out}");
    assert!(
        out.contains("p2.toml::") && out.contains("p4.toml::"),
        "the message names both profiles: {out}"
    );
    assert!(
        !config.active().join("p4.toml").exists(),
        "nothing was linked"
    );

    let (status, out) = harness::run(&config, &["profile-unapply", "zzz-not-there"]);
    assert!(
        !status.success() && out.contains("not applied") && out.contains("list of applied:"),
        "unapplying a stranger fails, listing what is applied: {out}"
    );

    // An edit re-applied replaces the link; the manager reloads it and claims its triggers
    // again.
    let p1_arg = p1.to_str().expect("utf-8 temp path");
    let (status, out) = harness::run(&config, &["profile-apply", p1_arg]);
    assert!(status.success(), "the re-apply failed: {out}");
    assert!(
        out.contains("already applied:")
            && out.contains("updating (re-applying) onto an existing manager (PID"),
        "{out}"
    );
    assert!(
        harness::await_line(manager.log(), "profile updated: p1.toml (2 binds)", 3),
        "the manager reloads the replaced link: {}",
        harness::read(manager.log())
    );
    rig.tap(Key::J);
    let sent = rig.sent();
    assert_eq!(
        typed(&sent),
        clean(Key::X, 1),
        "p1 binds after its reload: {sent:?}"
    );

    // ctrl+End shares p1's stop chord's primary key but not its modifiers: it runs its own
    // bind, clean of the held Ctrl, and stops nothing.
    rig.chord(&[Key::LeftCtrl, Key::End]);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::Y, 1), "{sent:?}");
    assert!(left_down(&sent).is_empty(), "{sent:?}");
    assert!(
        config.active().join("p1.toml").is_symlink(),
        "a trigger sharing the stop chord's primary key must not stop the profile"
    );

    // p1's own chord stops p1 alone: p2 stays, and so does the manager, holding the keyboard.
    rig.chord(&[Key::LeftCtrl, Key::LeftShift, Key::End]);
    assert!(
        harness::await_line(manager.log(), "emergency stop: p1.toml unapplied", 3),
        "p1's chord stops p1: {}",
        harness::read(manager.log())
    );
    let sent = rig.sent();
    assert!(
        sent.iter().all(|(key, _)| *key != Key::End),
        "the stop chord's key reached the session: {sent:?}"
    );
    assert!(
        !config.active().join("p1.toml").exists(),
        "p1's link is gone"
    );
    assert!(
        config.active().join("p2.toml").is_symlink(),
        "p2 survives p1's stop — nothing is global among scripts"
    );
    assert!(manager.alive(), "the manager survives a per-profile stop");
    assert!(rig.held(), "and still holds the keyboard, for p2");

    // p2's chord empties the set: the manager quits on its own, cleanly, and leaves nothing.
    rig.chord(&[Key::LeftCtrl, Key::LeftShift, Key::F11]);
    let status = manager
        .ended_within(10)
        .expect("the manager quits once its set empties");
    assert!(
        status.success(),
        "an empty-set quit exits 0, got {status:?}"
    );
    let text = harness::read(manager.log());
    assert!(
        text.contains("emergency stop: p2.toml unapplied")
            && text.contains("stopped (no profiles left)"),
        "{text}"
    );
    assert!(
        std::fs::read_dir(config.active()).map_or(true, |mut dir| dir.next().is_none()),
        "active/ is empty afterwards"
    );
    assert!(
        harness::lock_is_free(&config),
        "the manager quit but something still holds its lock"
    );
    assert!(
        freed_within(&mut rig, KILLED_RELEASE_WITHIN),
        "the keyboard is the session's again"
    );
}

/// A keyboard that comes back — plugged in again, or back from a suspend as a new device —
/// is held again within a moment, and its keys reach the profile as before. The supervisor
/// keeps no handle of the one that went, or it would keep one for every device that ever
/// comes and goes.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_keyboard_plugged_in_again_is_held_again() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let manager = watched(&config, &mut rig, LETTER);
    rig.replug();
    assert!(
        held_within(&mut rig, PLUGGED_IN_HELD_WITHIN),
        "the keyboard plugged in again was never held"
    );
    match gone_devices_supervised(manager.pid()) {
        Some(gone) => assert!(
            gone.is_empty(),
            "the supervisor still holds the keyboard that went: {gone:?}"
        ),
        None => eprintln!(
            "note: the supervisor's descriptors can't be read here, so its letting go of the \
             keyboard that went is unchecked"
        ),
    }
    rig.tap(Key::J);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::X, 1), "{sent:?}");
    assert!(left_down(&sent).is_empty(), "{sent:?}");
}

/// What the supervisor of the manager `manager` still holds open of input devices that have
/// gone — a removed node reads back as `(deleted)` — or `None` when its descriptors can't be
/// read from here.
fn gone_devices_supervised(manager: i32) -> Option<Vec<String>> {
    let supervisor = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            // `PID (COMMAND) STATE PARENT …`, where the command may hold spaces itself.
            let parent: i32 = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            let command = std::fs::read(entry.path().join("cmdline")).ok()?;
            let marked = command
                .split(|byte| *byte == 0)
                .any(|arg| arg == sequencer_cli::SUPERVISOR_MARKER.as_bytes());
            (parent == manager && marked).then_some(pid)
        })?;
    let gone = std::fs::read_dir(format!("/proc/{supervisor}/fd"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .map(|target| target.to_string_lossy().into_owned())
        .filter(|target| target.starts_with("/dev/input/") && target.ends_with(" (deleted)"))
        .collect();
    Some(gone)
}

/// Polls whether the rig's keyboard is held until it is, up to `within`.
fn held_within(rig: &mut Rig, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if rig.held() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Polls whether the rig's keyboard is held until it isn't, up to `within`.
fn freed_within(rig: &mut Rig, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !rig.held() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A manager that stops answering loses its grab to the supervisor once the quiet period
/// passes — and not before. The rig plays the manager: it has a real supervisor take the
/// grab on its keyboard, and then never beats. A real manager can't be frozen here: it runs
/// under sudo, which copies a stop onto the whole test run.
#[test]
#[ignore = "live: needs a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_silent_manager_loses_its_grab_to_the_supervisor() {
    let _serial = harness::serial();
    refuse_leftover_devices();
    let Some(mut rig) = Rig::start() else {
        return;
    };
    rig.supervise();
    assert!(rig.held(), "the supervisor took the grab");
    std::thread::sleep(SILENCE_BEGUN);
    assert!(rig.held(), "and keeps it while the quiet period lasts");
    assert!(
        freed_within(&mut rig, QUIET_RELEASE_WITHIN),
        "the supervisor never let a silent manager's grab go"
    );
}

/// A manager killed without its goodbye loses the keyboard at once: the supervisor sees
/// the link close, and lets the grab go.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_killed_manager_loses_the_keyboard_at_once() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    let config = TempConfig::new();
    let mut manager = Manager::start_on_devices(&config, &[&config.profile("letter.toml", LETTER)]);
    assert!(rig.held(), "the manager holds the rig's keyboard");
    manager.signal(nix::sys::signal::Signal::SIGKILL);
    assert!(
        freed_within(&mut rig, KILLED_RELEASE_WITHIN),
        "the supervisor never let a killed manager's grab go"
    );
    assert!(
        manager.ended_within(10).is_some(),
        "a killed manager is gone: {}",
        harness::read(manager.log())
    );
}

/// A mouse button a profile names is a trigger like any key: its click fires the bind and
/// never reaches the session — while the rest of the mouse, its other buttons and its
/// motion, passes on through its stand-in as it came, and the stand-in carries the mouse's
/// identity (the rig checks it as it starts watching).
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_named_mouse_button_fires_its_bind_and_the_rest_of_the_mouse_passes_on() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    rig.plug_mouse();
    let config = TempConfig::new();
    let manager = watched(&config, &mut rig, BACK_BUTTON);
    assert!(
        mouse_held_within(&mut rig, PLUGGED_IN_HELD_WITHIN),
        "the mouse whose button a profile names was never held: {}",
        harness::read(manager.log())
    );
    rig.watch_stand_in();

    rig.click(Button::Back);
    let sent = rig.sent();
    assert_eq!(typed(&sent), clean(Key::X, 1), "{sent:?}");
    assert!(left_down(&sent).is_empty(), "{sent:?}");
    let passed = rig.passed();
    assert!(
        passed.is_empty(),
        "the trigger reached the session too: {passed:?}"
    );

    rig.click(Button::Left);
    rig.move_by(5, -3);
    assert_eq!(
        rig.passed(),
        [
            Passed::Button(Button::Left, 1),
            Passed::Button(Button::Left, 0),
            Passed::Moved(5, -3),
        ],
        "the rest of the mouse passes on as it came"
    );
    let sent = rig.sent();
    assert!(
        sent.is_empty(),
        "the mouse's own input came out of the virtual keyboard: {sent:?}"
    );
}

/// With the optional udev rule installed, a USB mouse's stand-in is on the USB bus to udev,
/// so udev looks it up in its mouse database as it did the mouse — the rig's mouse is on
/// that bus. Without the rule this says so and passes: the rule is optional.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn with_the_udev_rule_a_usb_mouses_stand_in_is_on_its_bus() {
    let _serial = harness::serial();
    if !UDEV_RULE_DIRS
        .iter()
        .any(|dir| std::path::Path::new(dir).join(STAND_IN_RULE).exists())
    {
        eprintln!("skip: the stand-in udev rule ({STAND_IN_RULE}) isn't installed — README, Setup");
        return;
    }
    let Some(mut rig) = ready() else {
        return;
    };
    rig.plug_mouse();
    let config = TempConfig::new();
    let manager = watched(&config, &mut rig, BACK_BUTTON);
    assert!(
        stand_in_is_within(true, PLUGGED_IN_HELD_WITHIN),
        "the mouse a profile names got no stand-in: {}",
        harness::read(manager.log())
    );
    let node = stand_in().expect("the stand-in is there");
    assert!(
        udev_says_within(&node, "ID_BUS=usb", UDEV_DONE_WITHIN),
        "udev never put the stand-in ({node}) on the USB bus: is the rule as the README gives it, and reloaded?"
    );
}

/// A mouse is held only while some applied profile names one of its buttons: left alone
/// beside a profile of keys, held once a profile naming its button is stacked on, and the
/// session's again — its stand-in gone — once that profile is unapplied, while the keyboard
/// stays held for the profile left.
#[test]
#[ignore = "live: needs a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn a_mouse_is_held_only_while_a_profile_names_its_buttons() {
    let _serial = harness::serial();
    let Some(mut rig) = ready() else {
        return;
    };
    rig.plug_mouse();
    let config = TempConfig::new();
    let manager = watched(&config, &mut rig, LETTER);
    std::thread::sleep(MICE_LOOKED_AT_BY);
    assert!(!rig.mouse_held(), "a mouse no profile names was held");
    assert!(!has_stand_in(), "a mouse no profile names got a stand-in");

    let stacked = config.profile("mouse.toml", BACK_BUTTON_STACKED);
    let (status, out) = harness::run(
        &config,
        &["profile-apply", stacked.to_str().expect("utf-8 temp path")],
    );
    assert!(
        status.success(),
        "stacking the mouse's profile failed: {out}"
    );
    assert!(
        harness::await_line(manager.log(), "profile applied: mouse.toml", 3),
        "the manager picks the mouse's profile up: {}",
        harness::read(manager.log())
    );
    assert!(
        mouse_held_within(&mut rig, PLUGGED_IN_HELD_WITHIN),
        "named, the mouse is held: {}",
        harness::read(manager.log())
    );
    assert!(
        stand_in_is_within(true, PLUGGED_IN_HELD_WITHIN),
        "and stands in through a stand-in: {}",
        harness::read(manager.log())
    );

    let (status, out) = harness::run(&config, &["profile-unapply", "mouse.toml"]);
    assert!(
        status.success(),
        "unapplying the mouse's profile failed: {out}"
    );
    // The stand-in goes after the grab: once it is gone, the grab is.
    assert!(
        stand_in_is_within(false, PLUGGED_IN_HELD_WITHIN),
        "named no longer, the mouse keeps its stand-in: {}",
        harness::read(manager.log())
    );
    assert!(
        !rig.mouse_held(),
        "named no longer, the mouse is still held: {}",
        harness::read(manager.log())
    );
    assert!(rig.held(), "the keyboard stays held, for the profile left");
}

/// Polls whether the rig's mouse is held until it is, up to `within`.
fn mouse_held_within(rig: &mut Rig, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if rig.mouse_held() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// The node of the stand-in for the rig's mouse — `event<N>` — while there is one: asked of
/// sysfs, which needs no privilege.
fn stand_in() -> Option<String> {
    use sequencer_cli::input::linux::grab::STAND_IN_PHYS;
    use sequencer_cli::input::linux::rig::TEST_MOUSE_NAME;

    sequencer_cli::input::devices::list()
        .into_iter()
        .find(|device| {
            device.name == TEST_MOUSE_NAME
                && STAND_IN_PHYS.to_str().is_ok_and(|phys| device.phys == phys)
        })
        .map(|device| device.node)
}

/// Whether a stand-in for the rig's mouse is there now.
fn has_stand_in() -> bool {
    stand_in().is_some()
}

/// Polls whether udev's properties for `/dev/input/<node>` — as `udevadm info` gives them —
/// hold `property` (`NAME=value`), until they do, up to `within`.
fn udev_says_within(node: &str, property: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        let said = std::process::Command::new("udevadm")
            .args(["info", "--query=property"])
            .arg(format!("--name=/dev/input/{node}"))
            .output();
        if said.is_ok_and(|said| {
            String::from_utf8_lossy(&said.stdout)
                .lines()
                .any(|line| line == property)
        }) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Polls whether a stand-in for the rig's mouse is there until that is `there`, up to
/// `within`.
fn stand_in_is_within(there: bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if has_stand_in() == there {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
