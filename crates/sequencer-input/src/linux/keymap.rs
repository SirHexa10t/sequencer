//! Translating between this crate's keys and Linux evdev codes.
//!
//! Both directions come from one table, so a key that can be pressed can always be read
//! back and vice versa — a round-trip test over the whole table enforces it. Mouse
//! buttons live here too: at the evdev layer a button *is* a key code (`BTN_LEFT` is just
//! key `0x110`), which is why one table covers both.

use evdev::KeyCode;
use sequencer_core::emit::Holdable;
use sequencer_core::input::{Button, Key};

/// Every key this backend can press or observe.
///
/// Ordered to match the `Key` enum so a missing entry is easy to spot. Keys with no evdev
/// code simply do not appear, and are reported as unmappable rather than silently
/// dropped.
#[rustfmt::skip]
static KEYS: &[(Key, KeyCode)] = &[
    (Key::A, KeyCode::KEY_A), (Key::B, KeyCode::KEY_B), (Key::C, KeyCode::KEY_C),
    (Key::D, KeyCode::KEY_D), (Key::E, KeyCode::KEY_E), (Key::F, KeyCode::KEY_F),
    (Key::G, KeyCode::KEY_G), (Key::H, KeyCode::KEY_H), (Key::I, KeyCode::KEY_I),
    (Key::J, KeyCode::KEY_J), (Key::K, KeyCode::KEY_K), (Key::L, KeyCode::KEY_L),
    (Key::M, KeyCode::KEY_M), (Key::N, KeyCode::KEY_N), (Key::O, KeyCode::KEY_O),
    (Key::P, KeyCode::KEY_P), (Key::Q, KeyCode::KEY_Q), (Key::R, KeyCode::KEY_R),
    (Key::S, KeyCode::KEY_S), (Key::T, KeyCode::KEY_T), (Key::U, KeyCode::KEY_U),
    (Key::V, KeyCode::KEY_V), (Key::W, KeyCode::KEY_W), (Key::X, KeyCode::KEY_X),
    (Key::Y, KeyCode::KEY_Y), (Key::Z, KeyCode::KEY_Z),

    (Key::Num0, KeyCode::KEY_0), (Key::Num1, KeyCode::KEY_1), (Key::Num2, KeyCode::KEY_2),
    (Key::Num3, KeyCode::KEY_3), (Key::Num4, KeyCode::KEY_4), (Key::Num5, KeyCode::KEY_5),
    (Key::Num6, KeyCode::KEY_6), (Key::Num7, KeyCode::KEY_7), (Key::Num8, KeyCode::KEY_8),
    (Key::Num9, KeyCode::KEY_9),

    (Key::F1, KeyCode::KEY_F1), (Key::F2, KeyCode::KEY_F2), (Key::F3, KeyCode::KEY_F3),
    (Key::F4, KeyCode::KEY_F4), (Key::F5, KeyCode::KEY_F5), (Key::F6, KeyCode::KEY_F6),
    (Key::F7, KeyCode::KEY_F7), (Key::F8, KeyCode::KEY_F8), (Key::F9, KeyCode::KEY_F9),
    (Key::F10, KeyCode::KEY_F10), (Key::F11, KeyCode::KEY_F11), (Key::F12, KeyCode::KEY_F12),
    (Key::F13, KeyCode::KEY_F13), (Key::F14, KeyCode::KEY_F14), (Key::F15, KeyCode::KEY_F15),
    (Key::F16, KeyCode::KEY_F16), (Key::F17, KeyCode::KEY_F17), (Key::F18, KeyCode::KEY_F18),
    (Key::F19, KeyCode::KEY_F19), (Key::F20, KeyCode::KEY_F20), (Key::F21, KeyCode::KEY_F21),
    (Key::F22, KeyCode::KEY_F22), (Key::F23, KeyCode::KEY_F23), (Key::F24, KeyCode::KEY_F24),

    (Key::Escape, KeyCode::KEY_ESC), (Key::Tab, KeyCode::KEY_TAB),
    (Key::CapsLock, KeyCode::KEY_CAPSLOCK), (Key::Space, KeyCode::KEY_SPACE),
    (Key::Enter, KeyCode::KEY_ENTER), (Key::Backspace, KeyCode::KEY_BACKSPACE),
    (Key::Delete, KeyCode::KEY_DELETE), (Key::Insert, KeyCode::KEY_INSERT),
    (Key::Home, KeyCode::KEY_HOME), (Key::End, KeyCode::KEY_END),
    (Key::PageUp, KeyCode::KEY_PAGEUP), (Key::PageDown, KeyCode::KEY_PAGEDOWN),
    (Key::Up, KeyCode::KEY_UP), (Key::Down, KeyCode::KEY_DOWN),
    (Key::Left, KeyCode::KEY_LEFT), (Key::Right, KeyCode::KEY_RIGHT),

    (Key::LeftCtrl, KeyCode::KEY_LEFTCTRL), (Key::LeftShift, KeyCode::KEY_LEFTSHIFT),
    (Key::LeftAlt, KeyCode::KEY_LEFTALT), (Key::LeftMeta, KeyCode::KEY_LEFTMETA),
    (Key::RightCtrl, KeyCode::KEY_RIGHTCTRL), (Key::RightShift, KeyCode::KEY_RIGHTSHIFT),
    (Key::RightAlt, KeyCode::KEY_RIGHTALT), (Key::RightMeta, KeyCode::KEY_RIGHTMETA),

    (Key::Minus, KeyCode::KEY_MINUS), (Key::Equal, KeyCode::KEY_EQUAL),
    (Key::LeftBracket, KeyCode::KEY_LEFTBRACE), (Key::RightBracket, KeyCode::KEY_RIGHTBRACE),
    (Key::Backslash, KeyCode::KEY_BACKSLASH), (Key::Semicolon, KeyCode::KEY_SEMICOLON),
    (Key::Quote, KeyCode::KEY_APOSTROPHE), (Key::Grave, KeyCode::KEY_GRAVE),
    (Key::Comma, KeyCode::KEY_COMMA), (Key::Period, KeyCode::KEY_DOT),
    (Key::Slash, KeyCode::KEY_SLASH),

    (Key::PrintScreen, KeyCode::KEY_SYSRQ), (Key::ScrollLock, KeyCode::KEY_SCROLLLOCK),
    (Key::Pause, KeyCode::KEY_PAUSE),

    (Key::NumLock, KeyCode::KEY_NUMLOCK), (Key::KeypadDivide, KeyCode::KEY_KPSLASH),
    (Key::KeypadMultiply, KeyCode::KEY_KPASTERISK), (Key::KeypadMinus, KeyCode::KEY_KPMINUS),
    (Key::KeypadPlus, KeyCode::KEY_KPPLUS), (Key::KeypadEnter, KeyCode::KEY_KPENTER),
    (Key::KeypadDot, KeyCode::KEY_KPDOT),
    (Key::Keypad0, KeyCode::KEY_KP0), (Key::Keypad1, KeyCode::KEY_KP1),
    (Key::Keypad2, KeyCode::KEY_KP2), (Key::Keypad3, KeyCode::KEY_KP3),
    (Key::Keypad4, KeyCode::KEY_KP4), (Key::Keypad5, KeyCode::KEY_KP5),
    (Key::Keypad6, KeyCode::KEY_KP6), (Key::Keypad7, KeyCode::KEY_KP7),
    (Key::Keypad8, KeyCode::KEY_KP8), (Key::Keypad9, KeyCode::KEY_KP9),
];

/// Mouse buttons. Separate table, same idea.
#[rustfmt::skip]
static BUTTONS: &[(Button, KeyCode)] = &[
    (Button::Left, KeyCode::BTN_LEFT),
    (Button::Middle, KeyCode::BTN_MIDDLE),
    (Button::Right, KeyCode::BTN_RIGHT),
    (Button::Back, KeyCode::BTN_SIDE),
    (Button::Forward, KeyCode::BTN_EXTRA),
];

/// The evdev code for a key, if it has one.
///
/// [`Key::Hid`] passes its payload through as a raw code, which is the escape hatch for
/// anything the table does not name.
#[must_use]
pub fn key_to_code(key: Key) -> Option<KeyCode> {
    if let Key::Hid(raw) = key {
        return Some(KeyCode(raw));
    }
    KEYS.iter()
        .find(|(named, _)| *named == key)
        .map(|(_, code)| *code)
}

/// The key for an evdev code, if the table names one.
#[must_use]
pub fn code_to_key(code: KeyCode) -> Option<Key> {
    KEYS.iter()
        .find(|(_, mapped)| *mapped == code)
        .map(|(key, _)| *key)
}

/// The evdev code for a mouse button.
#[must_use]
pub fn button_to_code(button: Button) -> Option<KeyCode> {
    if let Button::Other(raw) = button {
        // Extra buttons continue upward from BTN_EXTRA.
        return Some(KeyCode(KeyCode::BTN_EXTRA.0.saturating_add(u16::from(raw))));
    }
    BUTTONS
        .iter()
        .find(|(named, _)| *named == button)
        .map(|(_, code)| *code)
}

/// The mouse button for an evdev code, if it is one.
#[must_use]
pub fn code_to_button(code: KeyCode) -> Option<Button> {
    BUTTONS
        .iter()
        .find(|(_, mapped)| *mapped == code)
        .map(|(button, _)| *button)
}

/// The evdev code for anything holdable.
#[must_use]
pub fn holdable_to_code(what: Holdable) -> Option<KeyCode> {
    match what {
        Holdable::Key(key) => key_to_code(key),
        Holdable::Button(button) => button_to_code(button),
    }
}

/// Every code the virtual device needs to declare, so the kernel and any listening
/// compositor accept the events it later emits.
///
/// A uinput device may only send codes it declared up front, so this has to be the union
/// of everything reachable — not just what the current profile happens to use.
pub fn all_codes() -> impl Iterator<Item = KeyCode> {
    KEYS.iter()
        .map(|(_, code)| *code)
        .chain(BUTTONS.iter().map(|(_, code)| *code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_key_round_trips() {
        for (key, code) in KEYS {
            assert_eq!(key_to_code(*key), Some(*code), "{key:?} lost its code");
            assert_eq!(code_to_key(*code), Some(*key), "{code:?} lost its key");
        }
    }

    #[test]
    fn every_button_round_trips() {
        for (button, code) in BUTTONS {
            assert_eq!(button_to_code(*button), Some(*code));
            assert_eq!(code_to_button(*code), Some(*button));
        }
    }

    #[test]
    fn no_code_is_claimed_twice() {
        // A duplicate would make the reverse lookup ambiguous, and the round-trip test
        // above would silently pass for whichever entry came first.
        let mut seen = HashSet::new();
        for (key, code) in KEYS {
            assert!(
                seen.insert(code.0),
                "{code:?} is mapped twice, second by {key:?}"
            );
        }
        for (button, code) in BUTTONS {
            assert!(
                seen.insert(code.0),
                "{code:?} is mapped twice, second by {button:?}"
            );
        }
    }

    #[test]
    fn the_keys_the_defaults_use_are_all_present() {
        // A missing F9 or left button would mean the shipped defaults do not work.
        for key in [Key::F9, Key::F8, Key::F, Key::A, Key::Space] {
            assert!(key_to_code(key).is_some(), "{key:?} is not mapped");
        }
        assert_eq!(button_to_code(Button::Left), Some(KeyCode::BTN_LEFT));
    }

    #[test]
    fn hid_and_other_pass_their_raw_code_through() {
        assert_eq!(key_to_code(Key::Hid(0x1234)), Some(KeyCode(0x1234)));
        assert_eq!(
            button_to_code(Button::Other(1)),
            Some(KeyCode(KeyCode::BTN_EXTRA.0 + 1))
        );
    }

    #[test]
    fn the_declared_set_covers_both_tables() {
        let declared: HashSet<u16> = all_codes().map(|code| code.0).collect();
        assert_eq!(declared.len(), KEYS.len() + BUTTONS.len());
        assert!(declared.contains(&KeyCode::BTN_LEFT.0));
        assert!(declared.contains(&KeyCode::KEY_F9.0));
    }
}
