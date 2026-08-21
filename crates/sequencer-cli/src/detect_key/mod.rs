//! The detect-key command: press a key, read its bindable name.
//!
//! Two ways in, one printer:
//!
//! - **Devices** (the default): read `/dev/input`, which is the only *exact* answer —
//!   every key reports as itself, modifiers alone and media keys and mouse buttons
//!   included, and kpmultiply is never mistaken for the `*` it types. Needs read access,
//!   so it may borrow sudo for the run the same way the clicker does (and sheds it once
//!   the devices are open — this mode never writes anything). The terminal is silenced
//!   while it runs, so presses do not also type into the shell; Ctrl+C quits, recognised
//!   as a chord off the devices themselves, which is the one path nothing between this
//!   process and the keyboard can swallow.
//! - **Terminal** (`--no-sudo`): raw mode on standard input. No permission on X11,
//!   Wayland, SSH and the console alike, and the press is naturally consumed — but a
//!   terminal only receives characters, so it reports the key *behind* the char (`{`
//!   prints `[`, exactly as a binds file reads it) and keys that type nothing print
//!   nothing.
//!
//! In both modes raw terminal state is restored by a guard on every exit, panic
//! included, and Ctrl+C is never allowed to arrive as a signal — a signal would kill the
//! process without running the guard, leaving the shell eating its own keystrokes. The
//! terminal mode reads it as the byte `0x03`; the device mode reads it as a chord off the
//! devices (see [`report_until_quit`] for why the byte alone was not enough).

use crate::args::DetectKeyArgs;
use crate::runtime::{EventPump, Wake};
use crate::{Deps, Result, exit};
use sequencer_core::input::{Category, EventKind, INPUT_MAP, Key};

/// The main block, illustrated. Shown at startup so the names being printed have a
/// picture to land on.
const KEYBOARD_MAP: &str = r"
╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮
│ esc ││  F1 ││  F2 ││  F3 ││  F4 ││  F5 ││  F6 ││  F7 ││  F8 ││  F9 ││ F10 ││ F11 ││ F12 │  (up to F24)
╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯
╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭───────────╮
│  `  ││  1  ││  2  ││  3  ││  4  ││  5  ││  6  ││  7  ││  8  ││  9  ││  0  ││  -  ││  =  ││ backspace │
╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰───────────╯
╭───────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────────╮
│  tab  ││  q  ││  w  ││  e  ││  r  ││  t  ││  y  ││  u  ││  i  ││  o  ││  p  ││  [  ││  ]  ││    \    │
╰───────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────────╯
╭─────────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭──────────────╮
│capslock ││  a  ││  s  ││  d  ││  f  ││  g  ││  h  ││  j  ││  k  ││  l  ││  ;  ││  '  ││     Enter    │
╰─────────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰──────────────╯
╭────────────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭─────╮╭──────────────────╮
│   shift    ││  z  ││  x  ││  c  ││  v  ││  b  ││  n  ││  m  ││  ,  ││  .  ││  /  ││        rshift    │
╰────────────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰─────╯╰──────────────────╯
╭──────╮╭──────╮╭──────╮╭──────────────────────────────────────────────────╮╭────────────╮╭────────────╮
│ ctrl ││ meta ││ alt  ││                      space                       ││    ralt    ││    rctrl   │
╰──────╯╰──────╯╰──────╯╰──────────────────────────────────────────────────╯╰────────────╯╰────────────╯";

/// Everything after the main block: the sets a drawing doesn't cover, and the rules.
///
/// Rendered from [`INPUT_MAP`], never written by hand: the map is the same structure
/// the profile parser reads names *from*, so what this prints and what a binds file
/// accepts cannot drift apart. A key added to the tables shows up here on its own.
fn reference() -> String {
    /// The families listed, in display order. `Main` is the drawing's job.
    const LISTED: &[Category] = &[
        Category::Nav,
        Category::Arrow,
        Category::Numpad,
        Category::Modifier,
        Category::Media,
        Category::Mouse,
        Category::Pad,
    ];

    let mut text =
        String::from("Every name above is valid in a binds file. The rest of a full keyboard:\n\n");
    for &category in LISTED {
        let items: Vec<String> = INPUT_MAP
            .in_category(category)
            .map(|entry| {
                if entry.gloss.is_empty() {
                    entry.name.to_owned()
                } else {
                    format!("{} ({})", entry.name, entry.gloss)
                }
            })
            .collect();
        set_lines(&mut text, category.label(), &items);
        if category == Category::Modifier {
            text.push_str("               (a bare modifier is the LEFT one)\n");
        }
    }
    set_lines(
        &mut text,
        "unnamed",
        &["\"hid:<code>\" (decimal or 0x hex) is a raw USB HID usage code".to_owned()],
    );

    // Every accepted spelling, straight from the parser's own table — grouped as
    // canonical/alias, so nothing here can drift from what a binds file takes.
    text.push('\n');
    let aliases: Vec<String> = Key::named()
        .filter_map(|(key, canonical)| {
            let mut spellings = canonical.to_owned();
            let mut any = false;
            for alias in INPUT_MAP.aliases_of(key) {
                spellings.push('/');
                spellings.push_str(alias);
                any = true;
            }
            any.then_some(spellings)
        })
        .collect();
    set_lines(&mut text, "aliases", &aliases);

    text.push_str(
        "\nReports the KEY, not the character: press '{' and it prints '['. Same key, shift ignored.\
        \nThis is also how profile/binds file treats it; writing '{' is the same as writing '[', same for 'A' and 'a', '!' and '1'...\n",
    );
    text.push_str(
        "\nKeys that have no char to go by are named: backspace, enter, the modifiers (ctl, shift...), the arrows (up, down..), space (separator for chord members).\n",
    );
    text.push_str("\nOn X11, the focused program prints as `focus: <name>` whenever it changes.\n");
    text
}

/// Appends one labelled, comma-joined, wrapped set line to the reference.
fn set_lines(text: &mut String, label: &str, items: &[String]) {
    const WIDTH: usize = 88;
    const INDENT: &str = "               "; // continuation lines align under the items
    let mut line = format!("  {:<13}", format!("{label}:"));
    let mut first = true;
    for item in items {
        let sep = if first { "" } else { " , " };
        if !first && line.len() + sep.len() + item.len() > WIDTH {
            text.push_str(line.trim_end());
            text.push_str(" ,\n");
            line = format!("{INDENT}{item}");
        } else {
            line.push_str(sep);
            line.push_str(item);
        }
        first = false;
    }
    text.push_str(line.trim_end());
    text.push('\n');
}

/// `sequencer detect-key`.
///
/// # Errors
///
/// If standard input is not a terminal, or the terminal cannot be switched to raw mode.
pub(crate) fn detect_key(args: &DetectKeyArgs, deps: &mut Deps<'_>) -> Result<u8> {
    if !args.global.quiet {
        writeln!(deps.out, "{KEYBOARD_MAP}\n\n{}", reference())?;
        deps.out.flush()?;
    }
    // The injected pump is the test seam: the printing contract stays checkable without
    // a terminal or devices, and through the SAME loop a real run uses. There is no
    // terminal to drain and no session to report, so both of those are inert here.
    if let Some(pump) = deps.pump.as_deref_mut() {
        return report_until_quit(
            pump,
            deps.clock,
            &|| false,
            &mut FocusPoll::silent(),
            deps.out,
        );
    }
    if args.no_sudo {
        tty::run(deps.out)
    } else {
        platform::detect_devices(deps.out)
    }
}

/// How often the loop surfaces: to drain the silenced terminal, and to re-read focus.
const QUIT_POLL_NANOS: u64 = 50_000_000;

/// Prints presses until Ctrl+C, or until the devices close.
///
/// **Ctrl+C is recognised from the DEVICES, not from the terminal.** This loop
/// already reads every keyboard, so the quit chord is plain input here — and that is
/// the only path nothing in the way can swallow. The terminal byte is a second
/// opinion, and a fragile one: `detect-key` re-execs itself under `sudo`, modern
/// sudo defaults to running its command on a *pty* and relaying the real terminal
/// into it, and somewhere in that relay the interrupt byte never reached our read —
/// the run could only be killed from another shell (field bug, twice: the byte check
/// was also starved by any device with a steady trickle, since it lived in the
/// timed-out arm of the wait and motion events print nothing).
///
/// So: the chord ends the run, the byte still ends the run if it arrives, and the
/// byte check keeps running regardless because *draining* the silenced terminal is
/// its other job — anything typed during the run must not land in the shell after.
/// Both checks are driven by the clock, never by the event stream going quiet.
fn report_until_quit(
    pump: &mut dyn EventPump,
    clock: &dyn sequencer_core::time::Clock,
    quit_requested: &dyn Fn() -> bool,
    focus: &mut FocusPoll,
    out: &mut dyn std::io::Write,
) -> Result<u8> {
    let mut checks: u32 = 0;
    let mut next_check = clock.now();
    let mut ctrl_down = false;
    loop {
        if clock.now() >= next_check {
            if quit_requested() {
                return Ok(exit::OK);
            }
            checks = checks.wrapping_add(1);
            // Focus is re-read every fourth check (~200ms): a human notices no lag
            // at that cadence, and the X round trips stay off the key path.
            if checks.is_multiple_of(4) {
                focus.report(out)?;
            }
            next_check = clock.now().saturating_add_nanos(QUIT_POLL_NANOS);
        }
        match pump.wait_until(Some(next_check)) {
            Wake::Event(event) => {
                match event.kind {
                    EventKind::KeyDown(Key::LeftCtrl | Key::RightCtrl) => ctrl_down = true,
                    EventKind::KeyUp(Key::LeftCtrl | Key::RightCtrl) => ctrl_down = false,
                    _ => {}
                }
                // The press is reported before the run ends: naming what was pressed
                // is this command's whole job, and the quit chord is no exception.
                if let Some(name) = pressed_name(event.kind) {
                    writeln!(out, "{name}")?;
                    out.flush()?;
                }
                // Off the devices, so it fires wherever it is pressed — hence the focus
                // question: a Ctrl+C aimed at another window is not aimed at this run.
                if ctrl_down && matches!(event.kind, EventKind::KeyDown(Key::C)) && focus.at_home()
                {
                    return Ok(exit::OK);
                }
            }
            Wake::Deadline => {}
            Wake::Interrupted => return Ok(exit::OK),
        }
    }
}

/// The bindable name of a press, or `None` for anything that is not a press.
fn pressed_name(kind: EventKind) -> Option<String> {
    match kind {
        EventKind::KeyDown(key) => Some(key.to_string()),
        EventKind::ButtonDown(button) => {
            Some(INPUT_MAP.display_name(sequencer_core::emit::Holdable::Button(button)))
        }
        // One notch, one line — the reserved names from the input map's mouse family.
        EventKind::Scroll { dy: 1.., .. } => Some("wheel-up".to_owned()),
        EventKind::Scroll { dy: ..=-1, .. } => Some("wheel-down".to_owned()),
        EventKind::Scroll { dx: 1.., .. } => Some("wheel-right".to_owned()),
        EventKind::Scroll { dx: ..=-1, .. } => Some("wheel-left".to_owned()),
        _ => None,
    }
}

mod decode;
mod tty;

/// Prints the focused program's name whenever it changes — `focus: firefox` — the
/// identifier a per-program profile will match on later. Keys print constantly; focus
/// only on a switch, which is what makes both readable in one stream.
///
/// Best-effort by design: on Wayland or without the `xtest` feature there is nothing to
/// ask, and the run simply reports keys alone (the intro says focus is X11-only).
struct FocusPoll {
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    watcher: Option<sequencer_input::FocusWatcher>,
    last: Option<String>,
    /// The window that had focus when the run began — the terminal it was started
    /// from, since starting it is the last thing that happened there.
    home: Option<u32>,
}

impl FocusPoll {
    fn new() -> Self {
        #[cfg(all(feature = "xtest", target_os = "linux"))]
        let watcher = sequencer_input::FocusWatcher::open();
        Self {
            home: {
                #[cfg(all(feature = "xtest", target_os = "linux"))]
                {
                    watcher
                        .as_ref()
                        .and_then(sequencer_input::FocusWatcher::focused_window)
                }
                #[cfg(not(all(feature = "xtest", target_os = "linux")))]
                {
                    None
                }
            },
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            watcher,
            last: None,
        }
    }

    /// A poll that never asks. For injected runs: no session to report on, and no
    /// reason for a unit test to open an X connection.
    fn silent() -> Self {
        Self {
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            watcher: None,
            last: None,
            home: None,
        }
    }

    /// Whether the window with focus right now is the one this run started in.
    ///
    /// The quit chord is read off the *devices*, which know nothing about focus — so
    /// without this, a Ctrl+C meant for another window would end the run too. See
    /// [`still_home`] for why an unreadable answer counts as yes.
    fn at_home(&self) -> bool {
        still_home(self.home, self.focused_window())
    }

    #[cfg(all(feature = "xtest", target_os = "linux"))]
    fn focused_window(&self) -> Option<u32> {
        self.watcher
            .as_ref()
            .and_then(sequencer_input::FocusWatcher::focused_window)
    }

    #[cfg(not(all(feature = "xtest", target_os = "linux")))]
    #[allow(
        clippy::unused_self,
        reason = "the stub keeps both builds on one call shape"
    )]
    fn focused_window(&self) -> Option<u32> {
        None
    }

    /// Asks for the current focus and prints it if it changed.
    fn report(&mut self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        let Some(class) = self.current() else {
            // Unreadable focus keeps the last known name: a window flickering through
            // an unnamed state must not re-announce its neighbour afterwards.
            return Ok(());
        };
        if self.last.as_deref() != Some(class.as_str()) {
            writeln!(out, "focus: {class}")?;
            out.flush()?;
            self.last = Some(class);
        }
        Ok(())
    }

    #[cfg(all(feature = "xtest", target_os = "linux"))]
    fn current(&self) -> Option<String> {
        self.watcher.as_ref()?.focused_class()
    }

    #[cfg(not(all(feature = "xtest", target_os = "linux")))]
    #[allow(
        clippy::unused_self,
        reason = "the stub keeps both builds on one call shape"
    )]
    fn current(&self) -> Option<String> {
        None
    }
}

/// Whether focus is still where the run began, given the window it began in and the one
/// focused now.
///
/// Unknown counts as yes, in both directions: with no window to compare against (no
/// EWMH, no X, a launcher rather than a terminal) or no readable answer now, the quit
/// chord must still work. A quit that cannot be reached is a worse bug than one that can
/// be reached from the wrong window — which is how this started.
const fn still_home(home: Option<u32>, now: Option<u32>) -> bool {
    match (home, now) {
        (Some(home), Some(now)) => home == now,
        _ => true,
    }
}

/// The device side: exact keys off `/dev/input`, with the terminal silenced meanwhile.
#[cfg(all(feature = "evdev", target_os = "linux"))]
mod platform {
    use super::tty;
    use crate::Result;
    use sequencer_input::{Epoch, EvdevCapture, SystemClock};

    /// Opens every input device, silences the terminal, and hands the pair to the
    /// printer. The loop itself is [`super::report_until_quit`] — policy lives in the
    /// parent module, and this function is only the OS wiring around it.
    pub(super) fn detect_devices(out: &mut dyn std::io::Write) -> Result<u8> {
        let epoch = Epoch::start();
        let clock = SystemClock::from_epoch(epoch.instant());
        let mut capture = EvdevCapture::new(epoch.clone());
        let stream = capture.start()?;
        // If sudo opened the devices, it has done its one job.
        crate::elevate::drop_root_after_open()?;
        // Silence the terminal: the devices are read *alongside* it, not instead of it,
        // so without this every press would also type into the shell. Absent a terminal
        // (piped stdin) there is nothing to silence, and the quit chord off the devices
        // is the way out either way.
        let silencer = tty::Silencer::enable();
        tracing::debug!(
            silenced = silencer.is_some(),
            "detect-key: terminal state before reading devices"
        );
        let mut focus = super::FocusPoll::new();
        focus.report(out)?;

        let mut pump = crate::runtime::CapturePump::new(stream, &clock);
        let outcome = super::report_until_quit(
            &mut pump,
            &clock,
            &|| silencer.as_ref().is_some_and(tty::Silencer::quit_requested),
            &mut focus,
            out,
        );
        drop(silencer);
        capture.stop();
        outcome
    }
}

/// Exact detection without the device backend: refuse, and name the way that works.
#[cfg(not(all(feature = "evdev", target_os = "linux")))]
mod platform {
    use crate::{Error, Result};

    pub(super) fn detect_devices(_out: &mut dyn std::io::Write) -> Result<u8> {
        Err(Error::NotImplemented(
            "exact detection reads the input devices, and this build has no device \
             backend. Pass --no-sudo for the terminal reader, which works everywhere."
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::runtime::EventPump;
    use sequencer_core::input::{EventKind, InputEvent, Key};
    use sequencer_core::testutil::VirtualClock;
    use sequencer_core::time::{Clock as _, Timestamp};

    /// A device that always has an event ready, counting how often it was asked.
    /// `budget` bounds it so a starved loop fails an assertion instead of hanging.
    struct BusyPump {
        clock: VirtualClock,
        asked: std::cell::Cell<u32>,
        budget: u32,
        kind: EventKind,
    }

    impl BusyPump {
        fn new(kind: EventKind, budget: u32) -> Self {
            Self {
                clock: VirtualClock::new(),
                asked: std::cell::Cell::new(0),
                budget,
                kind,
            }
        }
    }

    impl EventPump for BusyPump {
        fn wait_until(&mut self, _deadline: Option<Timestamp>) -> Wake {
            let asked = self.asked.get() + 1;
            self.asked.set(asked);
            if asked > self.budget {
                return Wake::Interrupted;
            }
            Wake::Event(InputEvent::physical(self.clock.now(), self.kind))
        }
    }

    /// The regression: Ctrl+C must be noticed while the devices are busy. A pump
    /// with an event always ready used to keep the loop out of the arm that looked
    /// for the quit byte, so nothing could stop the run — and a moved mouse is
    /// exactly that pump, silently, because motion prints nothing.
    #[test]
    fn a_busy_device_cannot_starve_the_quit_check() {
        let clock = VirtualClock::new();
        let mut pump = BusyPump::new(EventKind::Motion { x: 4, y: 9 }, 1_000);
        let mut out = Vec::new();

        let code = report_until_quit(&mut pump, &clock, &|| true, &mut FocusPoll::new(), &mut out)
            .expect("the loop reports its own exit");

        assert_eq!(code, exit::OK);
        assert!(
            pump.asked.get() <= 1,
            "the quit check waited for the event stream to go quiet: pumped {} times",
            pump.asked.get()
        );
        assert!(out.is_empty(), "motion prints nothing");
    }

    /// A pump that plays a script of presses, then closes.
    struct ScriptPump {
        clock: VirtualClock,
        script: std::collections::VecDeque<EventKind>,
    }

    impl EventPump for ScriptPump {
        fn wait_until(&mut self, _deadline: Option<Timestamp>) -> Wake {
            match self.script.pop_front() {
                Some(kind) => Wake::Event(InputEvent::physical(self.clock.now(), kind)),
                None => Wake::Interrupted,
            }
        }
    }

    /// The chord ends the run only while focus is where it started. Unknown counts as
    /// home, in either direction: a quit nobody can reach is the worse bug.
    #[test]
    fn the_quit_chord_belongs_to_the_window_the_run_started_in() {
        assert!(
            still_home(Some(7), Some(7)),
            "same window: this run's Ctrl+C"
        );
        assert!(
            !still_home(Some(7), Some(9)),
            "another window has focus: not this run's Ctrl+C"
        );
        assert!(still_home(None, Some(9)), "nothing to compare against");
        assert!(still_home(Some(7), None), "no readable answer now");
        assert!(still_home(None, None));
    }

    /// Ctrl+C off the DEVICES ends the run — the path the terminal cannot swallow.
    /// Both keys are still named on the way out, and nothing after the chord runs.
    #[test]
    fn ctrl_c_from_the_devices_quits() {
        let clock = VirtualClock::new();
        let mut pump = ScriptPump {
            clock: VirtualClock::new(),
            script: [
                EventKind::KeyDown(Key::LeftCtrl),
                EventKind::KeyDown(Key::C),
                EventKind::KeyDown(Key::F9),
            ]
            .into_iter()
            .collect(),
        };
        let mut out = Vec::new();

        let code = report_until_quit(
            &mut pump,
            &clock,
            // The terminal never sees it: this is the device path alone.
            &|| false,
            &mut FocusPoll::new(),
            &mut out,
        )
        .expect("the chord ends the run cleanly");

        assert_eq!(code, exit::OK);
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "ctrl\nc\n",
            "both keys are named, and nothing past the chord is read"
        );
    }

    /// `c` alone is just a key: only ctrl HELD makes it the quit chord, and a
    /// released ctrl stops counting.
    #[test]
    fn c_without_ctrl_held_is_only_a_key() {
        let clock = VirtualClock::new();
        let mut pump = ScriptPump {
            clock: VirtualClock::new(),
            script: [
                EventKind::KeyDown(Key::C),
                EventKind::KeyDown(Key::LeftCtrl),
                EventKind::KeyUp(Key::LeftCtrl),
                EventKind::KeyDown(Key::C),
            ]
            .into_iter()
            .collect(),
        };
        let mut out = Vec::new();

        report_until_quit(
            &mut pump,
            &clock,
            &|| false,
            &mut FocusPoll::new(),
            &mut out,
        )
        .expect("the script runs out and the stream closes");

        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "c\nctrl\nc\n",
            "the whole script was read: no quit fired"
        );
    }

    /// And with nothing asking it to stop, presses still print by name until the
    /// devices close.
    #[test]
    fn presses_print_until_the_devices_close() {
        let clock = VirtualClock::new();
        let mut pump = BusyPump::new(EventKind::KeyDown(Key::F9), 3);
        let mut out = Vec::new();

        let code = report_until_quit(
            &mut pump,
            &clock,
            &|| false,
            &mut FocusPoll::new(),
            &mut out,
        )
        .expect("a closed stream is a clean exit");

        assert_eq!(code, exit::OK);
        assert_eq!(String::from_utf8(out).expect("utf-8"), "F9\nF9\nF9\n");
    }

    /// Every canonical key name must appear somewhere in what detect-key prints — the
    /// drawing or the rendered sets. This is the guarantee that adding a key to the
    /// table cannot silently leave the listing stale: forget the group, fail the build.
    #[test]
    fn every_named_key_appears_in_the_printed_reference() {
        let text = format!("{KEYBOARD_MAP}\n{}", reference());
        // Split on whitespace, box art and parens only: `,` and `/` are list separators
        // in the output but are ALSO key names, so they must survive as standalone
        // tokens (the drawing renders each in its own cell). Alias pairs like
        // `esc/escape` are then sub-split, keeping both spellings.
        let mut tokens = std::collections::HashSet::new();
        for raw in text
            .split(|c: char| c.is_whitespace() || "│╭╮╰╯─()".contains(c))
            .filter(|token| !token.is_empty())
        {
            tokens.insert(raw.to_ascii_lowercase());
            for part in raw.split(['/', ',']).filter(|part| !part.is_empty()) {
                tokens.insert(part.to_ascii_lowercase());
            }
        }
        let is_high_function_key = |name: &str| {
            // The drawing says "(up to F24)" instead of listing f13..f24 one by one.
            name.strip_prefix('f')
                .and_then(|n| n.parse::<u8>().ok())
                .is_some_and(|n| (13..=24).contains(&n))
        };
        for entry in INPUT_MAP.entries() {
            if is_high_function_key(entry.name) {
                continue;
            }
            assert!(
                tokens.contains(&entry.name.to_ascii_lowercase()),
                "`{}` is in the input map but printed nowhere by detect-key",
                entry.name
            );
        }
    }

    /// A wheel notch reports by its reserved name — the device path hears scrolls now.
    #[test]
    fn wheel_notches_report_their_names() {
        assert_eq!(
            pressed_name(EventKind::Scroll { dx: 0, dy: 1 }).as_deref(),
            Some("wheel-up")
        );
        assert_eq!(
            pressed_name(EventKind::Scroll { dx: 0, dy: -3 }).as_deref(),
            Some("wheel-down")
        );
        assert_eq!(
            pressed_name(EventKind::Scroll { dx: -1, dy: 0 }).as_deref(),
            Some("wheel-left")
        );
    }
}
