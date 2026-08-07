//! The terminal byte decoder: raw input bytes to key names, pure and table-driven.
//!
//! No I/O anywhere in this file — [`super::tty`] reads the bytes, this decides what they
//! meant. That split is what lets the entire escape-sequence zoo be tested without a
//! terminal.

use sequencer_core::input::Key;

/// What one decoded terminal input means.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decoded {
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
pub(super) fn decode(bytes: &[u8]) -> Vec<Decoded> {
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
