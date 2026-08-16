//! The detect-key command: press a key, read its bindable name.
//!
//! Two ways in, one printer:
//!
//! - **Devices** (the default): read `/dev/input`, which is the only *exact* answer —
//!   every key reports as itself, modifiers alone and media keys and mouse buttons
//!   included, and kpmultiply is never mistaken for the `*` it types. Needs read access,
//!   so it may borrow sudo for the run the same way the clicker does (and sheds it once
//!   the devices are open — this mode never writes anything). The terminal is silenced
//!   while it runs, so presses do not also type into the shell; Ctrl+C still quits,
//!   read as a byte off the silenced terminal.
//! - **Terminal** (`--no-sudo`): raw mode on standard input. No permission on X11,
//!   Wayland, SSH and the console alike, and the press is naturally consumed — but a
//!   terminal only receives characters, so it reports the key *behind* the char (`{`
//!   prints `[`, exactly as a binds file reads it) and keys that type nothing print
//!   nothing.
//!
//! In both modes raw terminal state is restored by a guard on every exit, panic
//! included, and Ctrl+C is handled as the byte `0x03` rather than as a signal — a signal
//! would kill the process without running the guard, leaving the shell eating its own
//! keystrokes.

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
    // a terminal or devices. A real run picks its source by the flag.
    if let Some(pump) = deps.pump.as_deref_mut() {
        return pump_loop(pump, deps.out);
    }
    if args.no_sudo {
        tty::run(deps.out)
    } else {
        platform::detect_devices(deps.out)
    }
}

/// The printing contract, fed from an injected pump: presses print once, by bindable
/// name; releases print nothing.
fn pump_loop(pump: &mut dyn EventPump, out: &mut dyn std::io::Write) -> Result<u8> {
    loop {
        match pump.wait_until(None) {
            Wake::Event(event) => {
                if let Some(name) = pressed_name(event.kind) {
                    writeln!(out, "{name}")?;
                    out.flush()?;
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
}

impl FocusPoll {
    fn new() -> Self {
        Self {
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            watcher: sequencer_input::FocusWatcher::open(),
            last: None,
        }
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

/// The device side: exact keys off `/dev/input`, with the terminal silenced meanwhile.
#[cfg(all(feature = "evdev", target_os = "linux"))]
mod platform {
    use super::{exit, pressed_name, tty};
    use crate::Result;
    use crate::runtime::{EventPump as _, Wake};
    use sequencer_input::{Epoch, EvdevCapture, SystemClock};

    /// How often the loop surfaces to check the silenced terminal for Ctrl+C.
    const QUIT_POLL_NANOS: u64 = 50_000_000;

    /// Reads every input device until Ctrl+C, printing each press by its exact name.
    pub(super) fn detect_devices(out: &mut dyn std::io::Write) -> Result<u8> {
        let epoch = Epoch::start();
        let clock = SystemClock::from_epoch(epoch.instant());
        let mut capture = EvdevCapture::new(epoch.clone());
        let stream = capture.start()?;
        // If sudo opened the devices, it has done its one job.
        crate::elevate::drop_root_after_open()?;
        // Silence the terminal: the devices are read *alongside* it, not instead of it,
        // so without this every press would also type into the shell. Absent a terminal
        // (piped stdin) there is nothing to silence and Ctrl+C falls back to the signal.
        let silencer = tty::Silencer::enable();
        let mut focus = super::FocusPoll::new();
        if let Err(err) = focus.report(out) {
            return Err(err.into());
        }

        let mut pump = crate::runtime::CapturePump::new(stream, &clock);
        // Focus is re-read every fourth quit-poll wake (~200ms): a human notices no lag
        // at that cadence, and the X round trips stay off the key-reporting path.
        let mut wakes: u32 = 0;
        let outcome = loop {
            use sequencer_core::time::Clock as _;
            let deadline = clock.now().saturating_add_nanos(QUIT_POLL_NANOS);
            match pump.wait_until(Some(deadline)) {
                Wake::Event(event) => {
                    if let Some(name) = pressed_name(event.kind)
                        && let Err(err) = writeln!(out, "{name}").and_then(|()| out.flush())
                    {
                        break Err(err.into());
                    }
                }
                Wake::Deadline => {
                    if silencer.as_ref().is_some_and(tty::Silencer::quit_requested) {
                        break Ok(exit::OK);
                    }
                    wakes = wakes.wrapping_add(1);
                    if wakes.is_multiple_of(4)
                        && let Err(err) = focus.report(out)
                    {
                        break Err(err.into());
                    }
                }
                Wake::Interrupted => break Ok(exit::OK),
            }
        };
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
