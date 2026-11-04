//! The manager on the device backend, into real windows: the rig's keyboard types, and the
//! text lands in kdialog boxes — an X11 one through Xwayland and a Wayland one — as the
//! hand-run kit's device mode checks. KDE only, with a US layout active. A key is typed only
//! while KWin says the test's own box has focus, so a focus slip stops the test instead of
//! typing into another window.

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sequencer_cli::core::emit::{Emit, EmitAction, InputSink as _};
use sequencer_cli::core::input::{EventKind, Key};
use sequencer_cli::core::time::Timestamp;
use sequencer_cli::input::{CaptureStream, Epoch, GrabCapture, KWinFocus, KdeLayout, XTestSink};

use crate::devices::{CTRL, LETTER, SEQUENCE, SHIFT, SLOW_SHIFT, SPECIAL, SWALLOWED};
use crate::harness::{self, Manager, TempConfig};
use crate::rig::Rig;

/// How long a box may take to open and take focus.
const FOCUS_WITHIN: Duration = Duration::from_secs(10);

/// How long a box may take to close once Enter submits it.
const CLOSE_WITHIN: Duration = Duration::from_secs(5);

/// How long the manager's reaction to the last step may take to land in the box before
/// Enter submits it — Enter goes straight through, and would otherwise beat a sequence's
/// keys there. Longer than the slowest test profile takes (T10: two 100 ms waits, with its
/// taps and gaps).
const SETTLE: Duration = Duration::from_millis(500);

/// The two kinds of window, as Qt is told to make them: X11 (through Xwayland), Wayland.
const PLATFORMS: [&str; 2] = ["xcb", "wayland"];

/// One step a hand takes on the keyboard.
#[derive(Clone, Copy, Debug)]
enum Step {
    Tap(Key),
    Press(Key),
    Release(Key),
}

use Step::{Press, Release, Tap};

/// Whether the window tests can run here: the device tiers' needs, and KWin to say which
/// window has focus, kdialog to type into, and a US layout, whose letters the expected
/// texts are.
fn ready() -> Option<(Rig, KWinFocus)> {
    if Command::new("kdialog")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skip: no kdialog to type into");
        return None;
    }
    let layout = KdeLayout::open().and_then(|layout| layout.current());
    if layout.as_deref() != Some("us") {
        eprintln!(
            "skip: the window tests expect a US layout's letters, and the active layout is \
             {layout:?}"
        );
        return None;
    }
    let rig = crate::devices::ready()?;
    let Some(focus) = KWinFocus::open() else {
        eprintln!("skip: no KWin to say which window has focus");
        return None;
    };
    Some((rig, focus))
}

/// A kdialog input box of the test's own, open and focused.
struct InputBox {
    child: Child,
    /// KWin's id for it, which must have focus for anything to be typed.
    window: String,
    platform: &'static str,
}

impl InputBox {
    /// Opens a box on `platform` and waits until it has focus.
    fn open(platform: &'static str, focus: &KWinFocus) -> Self {
        let before = focus.focused_window();
        // Owned from the start, so a box that never takes focus is closed by `Drop`.
        let mut opened = Self {
            child: Command::new("kdialog")
                .args([
                    "--title",
                    &format!("sequencer live test ({platform})"),
                    "--inputbox",
                    "the live suite types here",
                    "",
                ])
                .env("QT_QPA_PLATFORM", platform)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("kdialog starts"),
            window: String::new(),
            platform,
        };
        let deadline = Instant::now() + FOCUS_WITHIN;
        loop {
            let class = focus.focused_class().unwrap_or_default().to_lowercase();
            if let Some(window) = focus.focused_window()
                && class.contains("kdialog")
                && Some(&window) != before.as_ref()
            {
                opened.window = window;
                return opened;
            }
            assert!(
                Instant::now() < deadline,
                "the {platform} box never took focus"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Whether this box has focus, or gets it back within `within` — saying once, while it
    /// waits, what to do: a KDE prompt is what takes focus away here.
    fn focused_within(&self, focus: &KWinFocus, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut said = false;
        loop {
            if focus.focused_window().as_deref() == Some(self.window.as_str()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if !said {
                eprintln!(
                    "  the {} box lost focus: if KDE asks, click Allow — waiting for the box to have it again",
                    self.platform
                );
                said = true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Whether the box closes within `within`, on whatever Enter reached it.
    fn closes_within(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while matches!(self.child.try_wait(), Ok(None)) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    /// Takes `steps` on the rig's keyboard, each only while this box has focus.
    fn type_steps(&self, rig: &mut Rig, focus: &KWinFocus, steps: &[Step]) {
        for step in steps {
            assert_eq!(
                focus.focused_window().as_deref(),
                Some(self.window.as_str()),
                "focus left the {} box before {step:?}; nothing more was typed",
                self.platform
            );
            match *step {
                Tap(key) => rig.tap(key),
                Press(key) => rig.press(key),
                Release(key) => rig.release(key),
            }
        }
    }

    /// Submits the box with Enter, and says what was typed in it.
    fn submit(mut self, rig: &mut Rig, focus: &KWinFocus) -> String {
        self.type_steps(rig, focus, &[Tap(Key::Enter)]);
        let deadline = Instant::now() + CLOSE_WITHIN;
        while matches!(self.child.try_wait(), Ok(None)) {
            assert!(
                Instant::now() < deadline,
                "the {} box never closed on Enter",
                self.platform
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut typed = String::new();
        if let Some(mut out) = self.child.stdout.take() {
            let _ = out.read_to_string(&mut typed);
        }
        typed.trim_end_matches('\n').to_owned()
    }
}

impl Drop for InputBox {
    fn drop(&mut self) {
        // A box a failed test left open must not linger on the desktop.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Runs `profile` on the devices and takes `steps` into a box of each kind, each with a
/// manager of its own; each box must end up holding `expected`. With `ends_manager`, the
/// steps end the manager themselves.
fn types_into_both_windows(profile: &str, steps: &[Step], expected: &str, ends_manager: bool) {
    let _serial = harness::serial();
    let Some((mut rig, focus)) = ready() else {
        return;
    };
    for platform in PLATFORMS {
        let config = TempConfig::new();
        let mut manager =
            Manager::start_on_devices(&config, &[&config.profile("profile.toml", profile)]);
        let input = InputBox::open(platform, &focus);
        input.type_steps(&mut rig, &focus, steps);
        std::thread::sleep(SETTLE);
        if ends_manager {
            assert!(
                manager.ended_within(10).is_some(),
                "the steps should have stopped the manager: {}",
                harness::read(manager.log())
            );
        }
        let typed = input.submit(&mut rig, &focus);
        assert_eq!(typed, expected, "into the {platform} box");
        drop(manager);
    }
}

/// T1: a key that types no character fires its bind in either window; F8 stops the
/// manager, and F9 after it is the window's again.
#[test]
#[ignore = "live: needs KDE, kdialog, a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn t1_a_special_key_fires_and_the_stop_key_ends_it() {
    let steps = [
        Tap(Key::F9),
        Tap(Key::F9),
        Tap(Key::F9),
        Tap(Key::F8),
        Tap(Key::F9),
    ];
    types_into_both_windows(SPECIAL, &steps, "xxx", true);
}

/// T2: a bound letter types its target in either window.
#[test]
#[ignore = "live: needs KDE, kdialog, a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn t2_a_bound_letter_types_its_target() {
    types_into_both_windows(LETTER, &[Tap(Key::J); 3], "xxx", false);
}

/// T3, T4: a chord's target comes out as a plain letter, not as the chord.
#[test]
#[ignore = "live: needs KDE, kdialog, a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn t3_t4_a_chord_types_a_plain_letter() {
    for (modifier, profile) in [(Key::LeftCtrl, CTRL), (Key::LeftShift, SHIFT)] {
        let chord = [Press(modifier), Tap(Key::J), Release(modifier)];
        types_into_both_windows(profile, &chord.repeat(3), "xxx", false);
    }
}

/// T5, T10: a sequence types "hi!", and a slow Shift still shifts its key.
#[test]
#[ignore = "live: needs KDE, kdialog, a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn t5_t10_sequences_type_their_shifted_characters() {
    types_into_both_windows(SEQUENCE, &[Tap(Key::F9)], "hi!", false);
    types_into_both_windows(SLOW_SHIFT, &[Tap(Key::F9)], "!", false);
}

/// T6: Ctrl+K bound to nothing never reaches the box, so it can't delete the line.
#[test]
#[ignore = "live: needs KDE, kdialog, a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn t6_a_chord_bound_to_nothing_deletes_nothing() {
    let steps = [
        Tap(Key::A),
        Tap(Key::B),
        Tap(Key::C),
        Tap(Key::Home),
        Press(Key::LeftCtrl),
        Tap(Key::K),
        Release(Key::LeftCtrl),
    ];
    types_into_both_windows(SWALLOWED, &steps, "abc", false);
}

/// T9: Ctrl held across three fires types three plain letters.
#[test]
#[ignore = "live: needs KDE, kdialog, a Wayland session and a cached sudo ticket; SUDO-TEST.sh runs it in its second pass"]
fn t9_a_held_modifier_carries_every_fire() {
    let steps = [
        Press(Key::LeftCtrl),
        Tap(Key::J),
        Tap(Key::J),
        Tap(Key::J),
        Release(Key::LeftCtrl),
    ];
    types_into_both_windows(CTRL, &steps, "xxx", false);
}

/// How long the routing check waits for its box to have focus again — after a KDE prompt,
/// whose answer it waits for.
const FOCUS_BACK_WITHIN: Duration = Duration::from_secs(60);

/// How long an injected key may take to reach the X11 grab on it.
const CAUGHT_WITHIN: Duration = Duration::from_secs(3);

/// How long the warm-up leaves for whatever a first injection brings on to show.
const FIRST_USE_SETTLE: Duration = Duration::from_secs(1);

/// What the routing check injects: Pause, alone and under the modifiers the X11 tier's
/// chords use. Pause, because nothing claims it — KDE's own window views sit on
/// Ctrl+F7–F12, where a chord that went astray could open one — and it does nothing in a
/// box, or a terminal.
const PAUSE_CHORDS: [&[Key]; 3] = [
    &[Key::Pause],
    &[Key::LeftCtrl, Key::Pause],
    &[Key::LeftCtrl, Key::LeftShift, Key::Pause],
];

/// Sends `keys` through XTEST on `sink` as a hand plays a chord: each down in order, then
/// each up in reverse.
fn xtest_chord(sink: &mut XTestSink, keys: &[Key]) {
    let downs = keys.iter().map(|&key| EmitAction::KeyDown(key));
    let ups = keys.iter().rev().map(|&key| EmitAction::KeyUp(key));
    for action in downs.chain(ups) {
        sink.emit(&Emit {
            at: Timestamp::ZERO,
            action,
            level: 0,
        })
        .expect("XTEST takes the key");
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Whether `chord`, injected through XTEST, reaches an X11 grab on it — the grab a manager
/// on the X11 fallback takes — within [`CAUGHT_WITHIN`].
fn reaches_a_grab(sink: &mut XTestSink, chord: &[Key]) -> bool {
    let (queue, stream) = CaptureStream::channel(16);
    let grab = GrabCapture::start_into(&Epoch::start(), &[chord.to_vec()], queue)
        .unwrap_or_else(|err| panic!("the X11 grab could not be taken: {err}"));
    let primary = *chord.last().expect("a chord has a key");
    xtest_chord(sink, chord);
    let deadline = Instant::now() + CAUGHT_WITHIN;
    let caught = loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break false;
        };
        match stream.next_within(left) {
            Some(event) if event.kind == EventKind::KeyDown(primary) => break true,
            Some(_) => {}
            None => break false,
        }
    };
    drop(grab);
    caught
}

/// Takes what a first injection brings on, in a box of its own: KDE asking whether to let
/// XTEST through, once per client until it remembers, and — on a first use, seen once — the
/// pointer sent to the top-left corner, whose hot corner opens the overview. Escape, at most
/// twice and each with the box focused, closes an overview if one opened, then the box.
fn warm_up(sink: &mut XTestSink, focus: &KWinFocus) {
    let mut first = InputBox::open("wayland", focus);
    xtest_chord(sink, &[Key::Pause]);
    std::thread::sleep(FIRST_USE_SETTLE);
    for _ in 0..2 {
        if !first.focused_within(focus, FOCUS_BACK_WITHIN) {
            return;
        }
        xtest_chord(sink, &[Key::Escape]);
        if first.closes_within(Duration::from_millis(500)) {
            return;
        }
    }
}

/// Under KDE Wayland, keys an X client injects through XTEST reach an X11 grab on them —
/// with an X11 window focused, and a Wayland one — and reach the focused window too: the
/// path a manager on the X11 fallback types through, and the one the X11 tier drives (whose
/// lifecycle test, typing with the terminal focused, skips in a Wayland session for it).
/// Every key goes to a box of the check's own, focused — KWin asked before each — and is
/// sent once.
#[test]
#[ignore = "live: needs KDE, kdialog and a Wayland session; SUDO-TEST.sh runs it in its second pass"]
fn injected_keys_reach_an_x11_grab_and_the_focused_window() {
    let _serial = harness::serial();
    if !sequencer_cli::input::x11::is_xwayland() {
        eprintln!("skip: not a Wayland session with Xwayland");
        return;
    }
    if Command::new("kdialog")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skip: no kdialog for the keys to land in");
        return;
    }
    let Some(focus) = KWinFocus::open() else {
        eprintln!("skip: no KWin to say which window has focus");
        return;
    };
    let sharing = sequencer_cli::input::kde::xwayland_sharing().keys;
    let mut sink = XTestSink::open().expect("XTEST");
    warm_up(&mut sink, &focus);
    for platform in PLATFORMS {
        let mut input = InputBox::open(platform, &focus);
        for chord in PAUSE_CHORDS {
            let name = chord
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                input.focused_within(&focus, FOCUS_BACK_WITHIN),
                "the {platform} box never had focus again, so {name} was never sent"
            );
            assert!(
                reaches_a_grab(&mut sink, chord),
                "{name}, injected with the {platform} box focused, never reached the X11 grab on \
                 it (KDE shares {sharing:?} with X11 apps)"
            );
        }
        assert!(
            input.focused_within(&focus, FOCUS_BACK_WITHIN),
            "the {platform} box never had focus again, so Escape was never sent"
        );
        xtest_chord(&mut sink, &[Key::Escape]);
        assert!(
            input.closes_within(CLOSE_WITHIN),
            "Escape, injected with the {platform} box focused, never reached it"
        );
    }
}
