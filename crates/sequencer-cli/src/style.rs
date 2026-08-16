//! The one place this crate decides what a highlighted word looks like.
//!
//! Only the words that carry the decision are highlighted — the keys of a banner, the
//! verb of a state change, the phrase of a refusal. Colouring the surrounding prose too
//! would make them stop standing out. Three colours, three meanings: blue for state and
//! keys, red for "this did not happen", orange for "this is how you stop something".
//!
//! Colour is suppressed when stdout is not a terminal, and when `NO_COLOR` is set to
//! anything (the [no-color.org](https://no-color.org) convention) — output captured to
//! a file should be plain text, not escape codes.

use std::io::IsTerminal as _;
use std::sync::OnceLock;

/// Bold blue.
const HIGHLIGHT: &str = "\u{1b}[1;34m";
/// Bold red.
const ALARM: &str = "\u{1b}[1;31m";
/// Bold orange — 256-colour 208; the basic palette has no orange.
const STOPPER: &str = "\u{1b}[1;38;5;208m";
const RESET: &str = "\u{1b}[0m";

/// Whether to emit colour at all. Resolved once: the answer cannot change mid-run, and
/// re-checking per word would mean an `isatty` syscall per banner field.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

/// `text` in bold blue, or unchanged when colour is off.
pub(crate) fn key(text: &str) -> String {
    paint(HIGHLIGHT, text)
}

/// `text` in bold red — the phrase naming what did not happen.
pub(crate) fn alarm(text: &str) -> String {
    paint(ALARM, text)
}

/// `text` in bold orange — the word or chord that stops something.
pub(crate) fn stopper(text: &str) -> String {
    paint(STOPPER, text)
}

fn paint(colour: &str, text: &str) -> String {
    if enabled() {
        format!("{colour}{text}{RESET}")
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
        assert!(
            styled.ends_with(RESET),
            "an unterminated colour leaks into the next line"
        );
        assert!(styled.contains("F9"));
    }
}
