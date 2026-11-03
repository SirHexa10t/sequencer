//! What KDE lets X11 apps hear of input meant for other apps.
//!
//! In a Wayland session an X11 app — sequencer's profiles included, which listen through
//! Xwayland — hears a key pressed in a native Wayland app only if KWin passes it on. KWin
//! decides from two settings in `kwinrc`'s `[Xwayland]` group, set in System Settings →
//! Legacy X11 App Support:
//!
//! - `XwaylandEavesdrops`: `None`, `Modifiers`, `Combinations` (the default) or `All`.
//!   KConfig stores the choice by name, read without regard to case, falls back to its
//!   number, and removes the key when it holds the default.
//! - `XwaylandEavesdropsMouse`: whether mouse buttons pass too; off by default.
//!
//! What passes is a copy: the Wayland app still receives the key.

use std::path::PathBuf;

/// Which keys KWin passes to X11 apps while another app has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySharing {
    /// None ("Prohibited").
    None,
    /// The keys that type no character: F-keys, Escape, arrows, modifiers, media keys.
    /// The setting is labelled "Only the Meta, Control, Alt, and Shift keys", but KWin
    /// passes every non-character key under it.
    Modifiers,
    /// As [`Self::Modifiers`], plus any key pressed while Ctrl, Alt or Meta is held.
    /// KWin's default.
    Combinations,
    /// Every key ("Always allowed").
    All,
}

/// What KWin passes to X11 apps of input meant for other apps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XwaylandSharing {
    /// The keys.
    pub keys: KeySharing,
    /// Whether mouse buttons pass too.
    pub mouse: bool,
}

impl Default for XwaylandSharing {
    fn default() -> Self {
        Self {
            keys: KeySharing::Combinations,
            mouse: false,
        }
    }
}

/// KDE's current setting: the user's `kwinrc`, then the system's (`$XDG_CONFIG_DIRS`,
/// `/etc/xdg` by default), then KWin's defaults — the order KConfig itself reads them in.
#[must_use]
pub fn xwayland_sharing() -> XwaylandSharing {
    let mut files: Vec<PathBuf> = Vec::new();
    let user = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    files.extend(user.map(|dir| dir.join("kwinrc")));
    let system = std::env::var_os("XDG_CONFIG_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    files.extend(std::env::split_paths(&system).map(|dir| dir.join("kwinrc")));
    let texts: Vec<String> = files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .collect();
    from_kwinrc(&texts.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The setting from `kwinrc` texts, the most important first: the first that sets a key
/// decides it.
fn from_kwinrc(texts: &[&str]) -> XwaylandSharing {
    let mut sharing = XwaylandSharing::default();
    let keys = texts
        .iter()
        .find_map(|text| setting(text, "XwaylandEavesdrops"))
        .and_then(|value| key_sharing(&value));
    if let Some(keys) = keys {
        sharing.keys = keys;
    }
    let mouse = texts
        .iter()
        .find_map(|text| setting(text, "XwaylandEavesdropsMouse"))
        .and_then(|value| boolean(&value));
    if let Some(mouse) = mouse {
        sharing.mouse = mouse;
    }
    sharing
}

/// The value of `key` in the `[Xwayland]` group of one config file's text.
fn setting(text: &str, key: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_group = line == "[Xwayland]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        // `Key[$i]` marks a value the user may not change; it is still the value.
        let name = name.trim();
        let name = name.split_once('[').map_or(name, |(bare, _)| bare);
        if name == key {
            return Some(value.trim().to_owned());
        }
    }
    None
}

fn key_sharing(value: &str) -> Option<KeySharing> {
    match value.to_ascii_lowercase().as_str() {
        "none" | "0" => Some(KeySharing::None),
        "modifiers" | "1" => Some(KeySharing::Modifiers),
        "combinations" | "2" => Some(KeySharing::Combinations),
        "all" | "3" => Some(KeySharing::All),
        _ => None,
    }
}

/// A KConfig boolean: `true`, `on`, `yes` or `1`, and their opposites.
fn boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_nothing_set_kwins_defaults_apply() {
        assert_eq!(from_kwinrc(&[]), XwaylandSharing::default());
        assert_eq!(
            from_kwinrc(&["[Compositing]\nBackend=OpenGL\n"]),
            XwaylandSharing {
                keys: KeySharing::Combinations,
                mouse: false
            }
        );
    }

    /// By name in any case, or by number; only the `[Xwayland]` group counts.
    #[test]
    fn the_setting_is_read_as_kconfig_writes_it() {
        let text = "[Other]\nXwaylandEavesdrops=None\n\n[Xwayland]\nXwaylandEavesdrops=all\nXwaylandEavesdropsMouse=true\n";
        assert_eq!(
            from_kwinrc(&[text]),
            XwaylandSharing {
                keys: KeySharing::All,
                mouse: true
            }
        );
        assert_eq!(
            from_kwinrc(&["[Xwayland]\nXwaylandEavesdrops=1\n"]).keys,
            KeySharing::Modifiers
        );
        assert_eq!(
            from_kwinrc(&["[Xwayland]\nXwaylandEavesdrops[$i]=None\n"]).keys,
            KeySharing::None,
            "an immutable value is still the value"
        );
    }

    /// The user's file wins over the system's, key by key.
    #[test]
    fn the_users_file_overrides_the_systems() {
        let user = "[Xwayland]\nXwaylandEavesdropsMouse=true\n";
        let system = "[Xwayland]\nXwaylandEavesdrops=None\nXwaylandEavesdropsMouse=false\n";
        assert_eq!(
            from_kwinrc(&[user, system]),
            XwaylandSharing {
                keys: KeySharing::None,
                mouse: true
            }
        );
    }

    #[test]
    fn a_value_kwin_would_not_accept_leaves_the_default() {
        assert_eq!(
            from_kwinrc(&["[Xwayland]\nXwaylandEavesdrops=sometimes\n"]).keys,
            KeySharing::Combinations
        );
    }
}
