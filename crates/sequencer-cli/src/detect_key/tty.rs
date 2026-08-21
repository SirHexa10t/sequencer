//! The terminal side of detect-key: raw mode, the silencer, and the read loop.
//!
//! Everything termios lives here, behind guards that restore the terminal on every exit
//! — see the module doc in [`super`] for why Ctrl+C is a byte and not a signal.

#[cfg(target_os = "linux")]
use super::decode::{Decoded, decode};
#[cfg(target_os = "linux")]
use crate::exit;
#[cfg(target_os = "linux")]
use crate::{Error, Result};

#[cfg(target_os = "linux")]
use nix::sys::termios::{self, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices};

#[cfg(target_os = "linux")]
use std::io::Read as _;

#[cfg(target_os = "linux")]
/// Puts the terminal into raw mode and guarantees the way back.
///
/// Restoring in `Drop` is load-bearing: with `ISIG` off nothing else will — a raw
/// terminal left behind eats the user's shell.
struct RawGuard {
    original: termios::Termios,
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
impl Drop for RawGuard {
    fn drop(&mut self) {
        // Numeric keypad back on, then the saved terminal state.
        let mut stdout = std::io::stdout();
        let _ = std::io::Write::write_all(&mut stdout, b"\x1b>");
        let _ = std::io::Write::flush(&mut stdout);
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
#[cfg(feature = "evdev")]
impl Drop for Silencer {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
    }
}

#[cfg(target_os = "linux")]
/// Reads the terminal until Ctrl+C or EOF, printing each press by name.
pub(super) fn run(out: &mut dyn std::io::Write) -> Result<u8> {
    let _guard = RawGuard::enable()?;
    let mut focus = super::SessionPoll::new();
    focus.report(out)?;
    let mut buf = Vec::with_capacity(16);
    loop {
        // Wait for a key with a timeout rather than blocking outright, so focus changes
        // are noticed between presses. poll(2) also keeps EOF unambiguous — a timeout
        // and a hangup look identical to a bare read, and confusing them would either
        // spin forever on a dead terminal or exit on an idle one.
        if !wait_for_input()? {
            focus.report(out)?;
            continue;
        }
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

/// Polls stdin for up to 200ms. `true` means readable (a key, or EOF for the read to
/// discover); `false` means the wait timed out and the caller may do idle work.
#[cfg(target_os = "linux")]
fn wait_for_input() -> Result<bool> {
    use std::os::fd::AsFd as _;
    let stdin = std::io::stdin();
    let mut fds = [nix::poll::PollFd::new(
        stdin.as_fd(),
        nix::poll::PollFlags::POLLIN,
    )];
    let ready = nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(200_u16))
        .map_err(|err| Error::NotImplemented(format!("polling the terminal: {err}")))?;
    Ok(ready > 0)
}

#[cfg(not(target_os = "linux"))]
use crate::{Error, Result};

#[cfg(not(target_os = "linux"))]
pub(super) fn run(_out: &mut dyn std::io::Write) -> Result<u8> {
    Err(Error::NotImplemented(format!(
        "detect-key is not written for {}; only Linux is supported.",
        std::env::consts::OS
    )))
}
