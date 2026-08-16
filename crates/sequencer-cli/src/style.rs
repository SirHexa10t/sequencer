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

/// The decision itself, kept pure so its truth table is testable: colour needs a
/// terminal and the absence of a veto.
fn colours(no_color_set: bool, stdout_is_terminal: bool) -> bool {
    !no_color_set && stdout_is_terminal
}

/// [`colours`] asked about this run, resolved once: the answer cannot change mid-run,
/// and re-checking per word would mean an `isatty` syscall per banner field.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        colours(
            std::env::var_os("NO_COLOR").is_some(),
            std::io::stdout().is_terminal(),
        )
    })
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

    /// The gate itself: NO_COLOR always wins, a pipe or file never gets escape codes,
    /// and only an unvetoed terminal gets colour. (The ambient answer depends on where
    /// the test run's stdout points — libtest captures prints, not the fd — so tests
    /// assert on this table and compose expected strings through the helpers.)
    #[test]
    fn colour_needs_a_terminal_and_no_veto() {
        assert!(colours(false, true));
        assert!(!colours(true, true), "NO_COLOR wins even on a terminal");
        assert!(!colours(false, false), "a pipe gets plain text");
        assert!(!colours(true, false));
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
