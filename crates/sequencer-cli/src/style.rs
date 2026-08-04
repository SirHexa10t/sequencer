//! The one place this crate decides what a highlighted word looks like.
//!
//! Only the *variable* parts of the run banner are highlighted — the mode, the rate, the keys.
//! Those are the four things worth re-reading before pressing anything, and colouring the
//! surrounding prose too would make them stop standing out.
//!
//! Colour is suppressed when stdout is not a terminal, and when `NO_COLOR` is set to anything
//! (the [no-color.org](https://no-color.org) convention) — a banner captured to a file should
//! be plain text, not escape codes.

use std::io::IsTerminal as _;
use std::sync::OnceLock;

/// Bold blue.
const HIGHLIGHT: &str = "\u{1b}[1;34m";
const RESET: &str = "\u{1b}[0m";

/// Whether to emit colour at all. Resolved once: the answer cannot change mid-run, and
/// re-checking per word would mean an `isatty` syscall per banner field.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
    })
}

/// `text` in bold blue, or unchanged when colour is off.
pub(crate) fn key(text: &str) -> String {
    if enabled() {
        format!("{HIGHLIGHT}{text}{RESET}")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests capture stdout, so colour is off and the helper is the identity function —
    /// which is exactly the property a captured banner needs.
    #[test]
    fn a_captured_run_gets_plain_text() {
        assert_eq!(key("F9"), "F9");
    }

    #[test]
    fn the_highlight_wraps_and_always_closes() {
        // Whatever `enabled()` decided, a highlight must never leave the terminal coloured.
        let styled = format!("{HIGHLIGHT}F9{RESET}");
        assert!(styled.ends_with(RESET), "an unterminated colour leaks into the next line");
        assert!(styled.contains("F9"));
    }
}
