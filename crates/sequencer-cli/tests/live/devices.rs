//! The manager on the device backend, below the desktop. The rig's keyboard types, and the
//! rig's watch reads back exactly what sequencer's virtual keyboard sends — grabbed, so
//! nothing reaches the session. Each manager holds the rig's keyboard alone
//! (`SEQUENCER_TEST_GRAB_ONLY`), so the user's keyboards are never touched.
//!
//! What was sent is judged by what it typed — each key that went down, with the modifiers
//! the virtual keyboard held as it did — and by everything coming back up. How a lifted tap
//! interleaves its lifts and restores is the executor's business, not the contract.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use sequencer_cli::core::input::{Key, Mods};

use crate::harness::{self, Manager, TempConfig};
use crate::rig::Rig;

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

/// How long the supervisor may take to let a silent manager's grab go: its own quiet
/// period (`supervise::QUIET`, two seconds), and a margin.
const QUIET_RELEASE_WITHIN: Duration = Duration::from_secs(5);

/// A quarter of the quiet period: long enough in that the supervisor has noticed the
/// silence, short enough that it mustn't have acted on it yet.
const SILENCE_BEGUN: Duration = Duration::from_millis(500);

/// How long a killed manager's grab may stay: the supervisor hears the link close at once,
/// so this is only the margin a loaded machine needs.
const KILLED_RELEASE_WITHIN: Duration = Duration::from_secs(2);

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

/// Refuses outright while a sequencer virtual keyboard or test keyboard already exists: a
/// manager or rig from an earlier run is alive, and its devices would pass for the test's
/// own — a watch on its virtual keyboard hears nothing, while the test's output goes on
/// to the session (seen when a frozen run left one behind).
fn refuse_leftover_devices() {
    use sequencer_cli::input::linux::grab::KEYBOARD_NAME;
    use sequencer_cli::input::linux::rig::TEST_KEYBOARD_NAME;

    let leftovers: Vec<String> = sequencer_cli::input::devices::list()
        .into_iter()
        .filter(|device| device.name == KEYBOARD_NAME || device.name == TEST_KEYBOARD_NAME)
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
