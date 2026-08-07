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
        EventKind::ButtonDown(button) => Some(crate::profile::run::pressable_name(
            sequencer_core::emit::Holdable::Button(button),
        )),
        // One notch, one line — the reserved names from the input map's mouse family.
        EventKind::Scroll { dy: 1.., .. } => Some("wheel-up".to_owned()),
        EventKind::Scroll { dy: ..=-1, .. } => Some("wheel-down".to_owned()),
        EventKind::Scroll { dx: 1.., .. } => Some("wheel-right".to_owned()),
        EventKind::Scroll { dx: ..=-1, .. } => Some("wheel-left".to_owned()),
        _ => None,
    }
}

/// What one decoded terminal input means.
#[derive(Debug, PartialEq, Eq)]
enum Decoded {
    /// A key with a bindable name.
    Named(&'static str),
    /// A key by way of a char the key table knows.
    Char(Key),
    /// Ctrl+C: end the run.
    Quit,
    /// Something the decoder cannot name (an unknown sequence, a non-ASCII char).
    Unknown,
}

/// Decodes one buffer of terminal bytes into presses.
///
/// Pure, so the entire escape-sequence zoo is unit-testable without a terminal. The
/// buffer holds whatever one read produced: a lone byte, an escape sequence, or several
/// of each if the user was quick.
fn decode(bytes: &[u8]) -> Vec<Decoded> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (one, used) = decode_one(rest);
        out.push(one);
        rest = &rest[used..];
    }
    out
}

/// Decodes the first input in `bytes`, returning it and how many bytes it took.
fn decode_one(bytes: &[u8]) -> (Decoded, usize) {
    match bytes[0] {
        0x03 => (Decoded::Quit, 1),
        0x1b => decode_escape(bytes),
        0x0d | 0x0a => (Decoded::Named("enter"), 1),
        0x09 => (Decoded::Named("tab"), 1),
        0x7f | 0x08 => (Decoded::Named("backspace"), 1),
        b' ' => (Decoded::Named("space"), 1),
        // Printable ASCII: the key table itself maps the char to its key, shifted or
        // not — `{` lands on the `[` key the same way `A` lands on `a`.
        c @ 0x21..=0x7e => {
            let name = (c as char).to_string();
            match name.parse::<Key>() {
                Ok(key) => (Decoded::Char(key), 1),
                Err(_) => (Decoded::Unknown, 1),
            }
        }
        // A control char is ctrl+letter; the key is the letter. 0x03 was handled above.
        c @ 0x01..=0x1a => {
            let letter = ((c - 1 + b'a') as char).to_string();
            match letter.parse::<Key>() {
                Ok(key) => (Decoded::Char(key), 1),
                Err(_) => (Decoded::Unknown, 1),
            }
        }
        // Non-ASCII: a layout-specific char the table has no key for. Consume the whole
        // UTF-8 sequence so the follow-up bytes are not misread as keys.
        c => {
            let len = match c {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            (Decoded::Unknown, len.min(bytes.len()))
        }
    }
}

/// Decodes an input starting with ESC: a lone escape press, or a CSI/SS3 sequence.
///
/// Modifier suffixes (`;2` for shift and friends) are deliberately dropped: the report
/// names the key that was pressed, and shift-PageUp is still the pgup key.
fn decode_escape(bytes: &[u8]) -> (Decoded, usize) {
    match bytes.get(1) {
        None => (Decoded::Named("esc"), 1),
        // SS3: how F1-F4 arrive on most terminals — and, once the run has switched the
        // terminal to application-keypad mode, how the numpad's own keys arrive too,
        // which is what lets kpmultiply report as itself instead of as `*`.
        Some(b'O') => match bytes.get(2) {
            Some(b'P') => (Decoded::Named("f1"), 3),
            Some(b'Q') => (Decoded::Named("f2"), 3),
            Some(b'R') => (Decoded::Named("f3"), 3),
            Some(b'S') => (Decoded::Named("f4"), 3),
            Some(b'H') => (Decoded::Named("home"), 3),
            Some(b'F') => (Decoded::Named("end"), 3),
            Some(b'A') => (Decoded::Named("up"), 3),
            Some(b'B') => (Decoded::Named("down"), 3),
            Some(b'C') => (Decoded::Named("right"), 3),
            Some(b'D') => (Decoded::Named("left"), 3),
            Some(b'j') => (Decoded::Named("kpmultiply"), 3),
            Some(b'k') => (Decoded::Named("kpplus"), 3),
            Some(b'm') => (Decoded::Named("kpminus"), 3),
            Some(b'o') => (Decoded::Named("kpdivide"), 3),
            Some(b'n') => (Decoded::Named("kpdot"), 3),
            Some(b'M') => (Decoded::Named("kpenter"), 3),
            Some(digit @ b'p'..=b'y') => {
                const KP: [&str; 10] = [
                    "kp0", "kp1", "kp2", "kp3", "kp4", "kp5", "kp6", "kp7", "kp8", "kp9",
                ];
                (Decoded::Named(KP[usize::from(digit - b'p')]), 3)
            }
            Some(_) => (Decoded::Unknown, 3),
            None => (Decoded::Named("esc"), 1),
        },
        Some(b'[') => decode_csi(bytes),
        // ESC followed by an ordinary char is alt+char in most terminals; the key is
        // the char. Decode the tail and keep its length.
        Some(_) => {
            let (inner, used) = decode_one(&bytes[1..]);
            (inner, used + 1)
        }
    }
}

/// Decodes a CSI sequence: `ESC [ <params> <final>`.
fn decode_csi(bytes: &[u8]) -> (Decoded, usize) {
    // Collect parameter bytes (digits and `;`) up to the final byte.
    let mut i = 2;
    while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b';') {
        i += 1;
    }
    let Some(&fin) = bytes.get(i) else {
        // Sequence cut short; call the ESC an ESC and let the tail decode as chars.
        return (Decoded::Named("esc"), 1);
    };
    let used = i + 1;
    // The first parameter decides `~`-terminated keys; modifiers ride after a `;` and
    // are ignored on purpose.
    let first_param: u16 = bytes[2..i]
        .split(|&b| b == b';')
        .next()
        .and_then(|digits| std::str::from_utf8(digits).ok())
        .and_then(|text| text.parse().ok())
        .unwrap_or(1);
    let decoded = match fin {
        b'A' => Decoded::Named("up"),
        b'B' => Decoded::Named("down"),
        b'C' => Decoded::Named("right"),
        b'D' => Decoded::Named("left"),
        b'H' => Decoded::Named("home"),
        b'F' => Decoded::Named("end"),
        // Shift-tab: still the tab key.
        b'Z' => Decoded::Named("tab"),
        b'P' => Decoded::Named("f1"),
        b'Q' => Decoded::Named("f2"),
        b'R' => Decoded::Named("f3"),
        b'S' => Decoded::Named("f4"),
        b'~' => match first_param {
            1 | 7 => Decoded::Named("home"),
            2 => Decoded::Named("insert"),
            3 => Decoded::Named("delete"),
            4 | 8 => Decoded::Named("end"),
            5 => Decoded::Named("pgup"),
            6 => Decoded::Named("pgdn"),
            11 => Decoded::Named("f1"),
            12 => Decoded::Named("f2"),
            13 => Decoded::Named("f3"),
            14 => Decoded::Named("f4"),
            15 => Decoded::Named("f5"),
            17 => Decoded::Named("f6"),
            18 => Decoded::Named("f7"),
            19 => Decoded::Named("f8"),
            20 => Decoded::Named("f9"),
            21 => Decoded::Named("f10"),
            23 => Decoded::Named("f11"),
            24 => Decoded::Named("f12"),
            _ => Decoded::Unknown,
        },
        _ => Decoded::Unknown,
    };
    (decoded, used)
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

        let mut pump = crate::runtime::CapturePump::new(stream, &clock);
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

/// The terminal side: raw mode in, bytes decoded, names out, terminal restored.
#[cfg(target_os = "linux")]
mod tty {
    use super::{Decoded, decode, exit};
    use crate::{Error, Result};
    use nix::sys::termios::{self, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices};
    use std::io::Read as _;

    /// Puts the terminal into raw mode and guarantees the way back.
    ///
    /// Restoring in `Drop` is load-bearing: with `ISIG` off nothing else will — a raw
    /// terminal left behind eats the user's shell.
    struct RawGuard {
        original: termios::Termios,
    }

    impl RawGuard {
        fn enable() -> Result<Self> {
            let stdin = std::io::stdin();
            let original = termios::tcgetattr(&stdin).map_err(|err| {
                Error::NotImplemented(format!(
                    "detect-key reads the terminal, and standard input is not one ({err}). \
                     Run it directly in a terminal window."
                ))
            })?;
            let mut raw = original.clone();
            // No echo (the press must not appear), no line buffering (a press, not a
            // line, is the unit), no signals (Ctrl+C is handled as a byte so this guard
            // always runs). Output processing stays on so `\n` still starts at column 0.
            raw.local_flags &= !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG);
            raw.output_flags |= OutputFlags::OPOST;
            raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
            raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
            termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw)
                .map_err(|err| Error::NotImplemented(format!("raw mode failed: {err}")))?;
            Ok(Self { original })
        }

        /// Blocks for one byte, then drains whatever arrived with it (an escape
        /// sequence's tail, or more presses from fast typing).
        fn read_burst(buf: &mut Vec<u8>) -> std::io::Result<()> {
            buf.clear();
            let mut stdin = std::io::stdin();
            let mut byte = [0_u8; 1];
            if stdin.read(&mut byte)? == 0 {
                return Ok(()); // EOF: the terminal went away.
            }
            buf.push(byte[0]);
            // A brief VTIME window catches the rest of an escape sequence without
            // stalling a lone ESC press for long.
            let mut raw = termios::tcgetattr(std::io::stdin()).map_err(std::io::Error::other)?;
            raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
            raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 1; // 100 ms
            termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw)
                .map_err(std::io::Error::other)?;
            loop {
                let got = stdin.read(&mut byte)?;
                if got == 0 {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > 64 {
                    break; // Nothing legitimate is this long; stop hoarding.
                }
            }
            raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
            raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
            termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw)
                .map_err(std::io::Error::other)?;
            Ok(())
        }
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            // Numeric keypad back on, then the saved terminal state.
            let mut stdout = std::io::stdout();
            let _ = std::io::Write::write_all(&mut stdout, b"\x1b>");
            let _ = std::io::Write::flush(&mut stdout);
            let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
        }
    }

    /// Mutes the terminal while the *devices* are being read: no echo, no line
    /// buffering, no signals — and non-blocking, so the device loop can drain what the
    /// keyboard typed (consuming it) and spot Ctrl+C among it.
    ///
    /// Gated with its only caller: the tty module builds whenever the CLI does, but the
    /// device loop needs the evdev feature.
    #[cfg(feature = "evdev")]
    pub(super) struct Silencer {
        original: termios::Termios,
    }

    #[cfg(feature = "evdev")]
    impl Silencer {
        /// Best-effort: piped stdin has no echo to silence and no Ctrl+C byte to read,
        /// so `None` simply means the caller falls back to the SIGINT default.
        pub(super) fn enable() -> Option<Self> {
            let stdin = std::io::stdin();
            let original = termios::tcgetattr(&stdin).ok()?;
            let mut raw = original.clone();
            raw.local_flags &= !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG);
            raw.output_flags |= OutputFlags::OPOST;
            raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
            raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
            termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).ok()?;
            Some(Self { original })
        }

        /// Drains everything the keyboard typed into the terminal — consuming it is the
        /// point — and reports whether Ctrl+C was in there.
        ///
        /// Takes `&self` although nothing is read from it: holding a `Silencer` is the
        /// proof stdin is in the non-blocking state, without which this read would hang.
        #[allow(clippy::unused_self, reason = "the receiver is the capability")]
        pub(super) fn quit_requested(&self) -> bool {
            let mut stdin = std::io::stdin();
            let mut buf = [0_u8; 64];
            let mut quit = false;
            loop {
                match std::io::Read::read(&mut stdin, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => quit |= buf[..read].contains(&0x03),
                }
            }
            quit
        }
    }

    #[cfg(feature = "evdev")]
    impl Drop for Silencer {
        fn drop(&mut self) {
            let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
        }
    }

    /// Reads the terminal until Ctrl+C or EOF, printing each press by name.
    pub(super) fn run(out: &mut dyn std::io::Write) -> Result<u8> {
        let _guard = RawGuard::enable()?;
        let mut buf = Vec::with_capacity(16);
        loop {
            RawGuard::read_burst(&mut buf)?;
            if buf.is_empty() {
                return Ok(exit::OK); // EOF
            }
            for decoded in decode(&buf) {
                match decoded {
                    Decoded::Quit => return Ok(exit::OK),
                    Decoded::Named(name) => writeln!(out, "{name}")?,
                    Decoded::Char(key) => writeln!(out, "{key}")?,
                    Decoded::Unknown => {}
                }
            }
            out.flush()?;
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod tty {
    use crate::{Error, Result};

    pub(super) fn run(_out: &mut dyn std::io::Write) -> Result<u8> {
        Err(Error::NotImplemented(format!(
            "detect-key is not written for {}; only Linux is supported.",
            std::env::consts::OS
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(bytes: &[u8]) -> Decoded {
        let mut all = decode(bytes);
        assert_eq!(all.len(), 1, "{bytes:?} should be one input");
        all.remove(0)
    }

    /// The report names the key, never the character: `{` is the `[` key with shift
    /// held, and shift is not the terminal's business to report.
    #[test]
    fn a_shifted_char_reports_its_base_key() {
        assert_eq!(one(b"{"), Decoded::Char(Key::LeftBracket));
        assert_eq!(one(b"A"), Decoded::Char(Key::A));
        assert_eq!(one(b"!"), Decoded::Char(Key::Num1));
        assert_eq!(one(b"?"), Decoded::Char(Key::Slash));
    }

    #[test]
    fn charless_keys_get_their_words() {
        assert_eq!(one(b" "), Decoded::Named("space"));
        assert_eq!(one(b"\r"), Decoded::Named("enter"));
        assert_eq!(one(b"\t"), Decoded::Named("tab"));
        assert_eq!(one(&[0x7f]), Decoded::Named("backspace"));
        assert_eq!(one(&[0x1b]), Decoded::Named("esc"));
    }

    /// A control char is ctrl+letter, and the key is the letter — except Ctrl+C, which
    /// stays the way out and must never be reported as `c`.
    #[test]
    fn ctrl_c_quits_and_other_ctrl_chars_name_their_letter() {
        assert_eq!(one(&[0x03]), Decoded::Quit);
        assert_eq!(one(&[0x01]), Decoded::Char(Key::A));
        assert_eq!(one(&[0x1a]), Decoded::Char(Key::Z));
    }

    #[test]
    fn escape_sequences_cover_the_nav_cluster_and_function_keys() {
        assert_eq!(one(b"\x1b[A"), Decoded::Named("up"));
        assert_eq!(one(b"\x1b[D"), Decoded::Named("left"));
        assert_eq!(one(b"\x1b[H"), Decoded::Named("home"));
        assert_eq!(one(b"\x1b[2~"), Decoded::Named("insert"));
        assert_eq!(one(b"\x1b[3~"), Decoded::Named("delete"));
        assert_eq!(one(b"\x1b[5~"), Decoded::Named("pgup"));
        assert_eq!(one(b"\x1b[6~"), Decoded::Named("pgdn"));
        assert_eq!(one(b"\x1bOP"), Decoded::Named("f1"));
        assert_eq!(one(b"\x1b[15~"), Decoded::Named("f5"));
        assert_eq!(one(b"\x1b[24~"), Decoded::Named("f12"));
    }

    /// Modifier parameters are stripped: shift-PageUp is still the pgup key, and
    /// shift-tab is still the tab key. The key, not the chord, is the answer.
    #[test]
    fn modified_sequences_report_the_base_key() {
        assert_eq!(one(b"\x1b[5;2~"), Decoded::Named("pgup"));
        assert_eq!(one(b"\x1b[1;5A"), Decoded::Named("up"));
        assert_eq!(one(b"\x1b[Z"), Decoded::Named("tab"));
    }

    /// In application-keypad mode the numpad reports its own keys, which is the whole
    /// point of switching the mode on: kpmultiply is not the `*` it types.
    #[test]
    fn application_keypad_sequences_name_the_numpad_keys() {
        assert_eq!(one(b"\x1bOj"), Decoded::Named("kpmultiply"));
        assert_eq!(one(b"\x1bOk"), Decoded::Named("kpplus"));
        assert_eq!(one(b"\x1bOo"), Decoded::Named("kpdivide"));
        assert_eq!(one(b"\x1bOM"), Decoded::Named("kpenter"));
        assert_eq!(one(b"\x1bOp"), Decoded::Named("kp0"));
        assert_eq!(one(b"\x1bOy"), Decoded::Named("kp9"));
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

    /// Alt+char arrives as ESC then the char; the key is the char.
    #[test]
    fn alt_char_reports_the_char_key() {
        assert_eq!(one(b"\x1bx"), Decoded::Char(Key::X));
    }

    #[test]
    fn a_burst_of_presses_decodes_in_order() {
        assert_eq!(
            decode(b"ab\x1b[A "),
            vec![
                Decoded::Char(Key::A),
                Decoded::Char(Key::B),
                Decoded::Named("up"),
                Decoded::Named("space"),
            ]
        );
    }

    /// A layout char the table has no key for prints nothing, and its UTF-8 tail must
    /// not be misread as extra presses.
    #[test]
    fn non_ascii_chars_are_consumed_whole() {
        assert_eq!(decode("é".as_bytes()), vec![Decoded::Unknown]);
    }
}
