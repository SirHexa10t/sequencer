//! Diagnosing whether this machine can actually run the thing.
//!
//! The design idea worth naming: **remediation is data, not print statements.** A
//! [`Requirement`] knows how to check itself and how to explain its own fix, so `doctor`
//! is just every requirement rendered verbosely — and a startup failure renders the
//! *same* [`Remediation`] for the one requirement that failed. There is exactly one place
//! the advice lives, so the two cannot drift, and every startup error is automatically as
//! good as `doctor`'s output.
//!
//! For a tool whose dominant support burden is permissions, that is the single
//! highest-leverage thing in this crate.

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

/// Which display server the user is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// A plain X11 session.
    X11,
    /// An X11 client inside a Wayland compositor.
    ///
    /// Worth distinguishing: X11 capture here sees only what reaches X clients, so native
    /// Wayland applications are invisible. Without saying so, "it works in my terminal but
    /// not in my game" is an unanswerable bug report.
    XWayland,
    /// A native Wayland session with no X server reachable.
    Wayland,
    /// A Linux virtual terminal, with no display server.
    Tty,
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
    /// Could not tell.
    Unknown,
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::X11 => "X11",
            Self::XWayland => "XWayland (X11 client in a Wayland session)",
            Self::Wayland => "Wayland",
            Self::Tty => "Linux virtual terminal",
            Self::Windows => "Windows",
            Self::MacOs => "macOS",
            Self::Unknown => "unknown",
        };
        f.write_str(name)
    }
}

/// What was detected about the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// The display server, if any.
    pub session: Session,
    /// The value of `DISPLAY`, on Linux.
    pub display: Option<String>,
    /// The value of `WAYLAND_DISPLAY`, on Linux.
    pub wayland_display: Option<String>,
    /// The value of `XDG_SESSION_TYPE`, on Linux.
    pub session_type: Option<String>,
}

impl SessionInfo {
    /// Looks at the environment and works out where we are.
    #[must_use]
    pub fn detect() -> Self {
        let display = non_empty("DISPLAY");
        let wayland_display = non_empty("WAYLAND_DISPLAY");
        let session_type = non_empty("XDG_SESSION_TYPE");

        let session = if cfg!(target_os = "windows") {
            Session::Windows
        } else if cfg!(target_os = "macos") {
            Session::MacOs
        } else {
            match (display.is_some(), wayland_display.is_some()) {
                // Both set means an X server is reachable from inside a Wayland session.
                (true, true) => Session::XWayland,
                (true, false) => Session::X11,
                (false, true) => Session::Wayland,
                (false, false) => Session::Tty,
            }
        };

        Self {
            session,
            display,
            wayland_display,
            session_type,
        }
    }
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// The outcome of checking one requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// Satisfied.
    Pass,
    /// Not satisfied, with what was actually found.
    Fail(String),
    /// Could not be determined here — usually because it belongs to another platform.
    Unknown(&'static str),
}

impl CheckResult {
    /// Whether this counts as satisfied.
    #[must_use]
    pub const fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// One step towards fixing a requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A command to run.
    Shell(Cow<'static, str>),
    /// A file to create, with its contents.
    WriteFile {
        /// Where it goes.
        path: &'static str,
        /// What goes in it.
        body: &'static str,
    },
    /// Something the user has to do by hand.
    Manual(Cow<'static, str>),
}

/// How to satisfy a requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remediation {
    /// One-line summary.
    pub title: &'static str,
    /// Why this is needed, in the user's terms.
    pub why: &'static str,
    /// What to do about it, in order.
    pub steps: Vec<Step>,
    /// Anything the user should understand before doing it.
    pub caution: Option<&'static str>,
}

/// Something the environment has to provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Requirement {
    /// The `uinput` kernel module is loaded.
    UinputModuleLoaded,
    /// `/dev/uinput` is writable by this user.
    UinputNodeWritable,
    /// Every keyboard in `/dev/input` is readable by this user.
    EvdevReadable,
}

impl Requirement {
    /// Checks this requirement against the current environment.
    #[must_use]
    pub fn check(self) -> CheckResult {
        match self {
            Self::UinputModuleLoaded => check_path_exists("/dev/uinput"),
            Self::UinputNodeWritable => check_writable("/dev/uinput"),
            Self::EvdevReadable => check_evdev_readable(),
        }
    }

    /// A short label for reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::UinputModuleLoaded => "uinput kernel module",
            Self::UinputNodeWritable => "/dev/uinput writable",
            Self::EvdevReadable => "/dev/input keyboards readable",
        }
    }

    /// How to satisfy it.
    ///
    /// One flat match, deliberately: keeping every requirement's advice in a single
    /// place is what stops `doctor` and the startup errors drifting apart.
    #[must_use]
    pub fn remediation(self) -> Remediation {
        match self {
            Self::UinputModuleLoaded => Remediation {
                title: "The uinput kernel module is not loaded",
                why: "Injecting input below the display server needs /dev/uinput, which \
                      the uinput module provides.",
                steps: vec![
                    Step::Shell("sudo modprobe uinput".into()),
                    Step::WriteFile {
                        path: "/etc/modules-load.d/uinput.conf",
                        body: "uinput\n",
                    },
                ],
                caution: None,
            },
            Self::UinputNodeWritable => Remediation {
                title: "/dev/uinput is not writable",
                why: "The evdev backend creates a virtual input device there.",
                steps: vec![
                    Step::WriteFile {
                        path: "/etc/udev/rules.d/99-sequencer-uinput.rules",
                        body: "KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\", \
                               OPTIONS+=\"static_node=uinput\"\n",
                    },
                    Step::Shell("sudo udevadm control --reload-rules".into()),
                    Step::Shell("sudo udevadm trigger".into()),
                    Step::Shell("sudo usermod -aG input \"$USER\"".into()),
                    Step::Manual(
                        "Log out and back in: group membership is only applied at login. \
                         To test in the current shell without logging out, run `newgrp input`."
                            .into(),
                    ),
                ],
                caution: Some(
                    "Membership of the 'input' group lets ANY program you run read EVERY \
                     input device on this machine. That is full keylogging capability, \
                     including passwords typed into any application. sequencer needs it \
                     because it reads and writes input devices directly, which is what \
                     makes it work the same on X11, Wayland and the console.",
                ),
            },
            Self::EvdevReadable => Remediation {
                title: "A keyboard in /dev/input is not readable",
                why: "The evdev backend reads input devices directly, which is what makes \
                      it work identically on X11, Wayland and the console.",
                steps: vec![
                    Step::Shell("sudo usermod -aG input \"$USER\"".into()),
                    Step::Manual("Log out and back in.".into()),
                ],
                caution: Some(
                    "See the warning about the 'input' group above: it grants keylogging \
                     capability to everything you run.",
                ),
            },
        }
    }
}

fn check_path_exists(path: &'static str) -> CheckResult {
    if Path::new(path).exists() {
        CheckResult::Pass
    } else {
        CheckResult::Fail(format!("{path} does not exist"))
    }
}

fn check_writable(path: &'static str) -> CheckResult {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => CheckResult::Pass,
        Err(err) => CheckResult::Fail(format!("{path}: {err}")),
    }
}

/// Whether a device reporting these key codes is a keyboard: it has every code from 1
/// (`KEY_ESC`) to 31 (`KEY_S`) — Escape, the digit row, Q to P, A and S.
///
/// The rule udev's `input_id` builtin uses for `ID_INPUT_KEYBOARD`. Power buttons, media
/// remotes, mice and game pads report key codes too, but never all of these, and a
/// keyboard always does.
pub(crate) fn is_keyboard(has_key: impl Fn(u16) -> bool) -> bool {
    (1..=31).all(has_key)
}

/// [`is_keyboard`], asked of a device's `capabilities/key` bitmap in sysfs.
///
/// The kernel prints the bitmap as hexadecimal words, most significant first, leaving out
/// leading zero words (`input_print_bitmap`), so the last word always holds codes 0-63 —
/// all [`is_keyboard`] looks at. A 32-bit kernel prints 32-bit words, which still covers
/// codes 1-31.
fn caps_describe_a_keyboard(caps: &str) -> bool {
    let Some(low) = caps
        .split_whitespace()
        .last()
        .and_then(|word| u64::from_str_radix(word, 16).ok())
    else {
        return false;
    };
    is_keyboard(|code| (low >> code) & 1 == 1)
}

/// A keyboard device in `/dev/input`, and whether this process could open it.
#[derive(Debug)]
struct KeyboardNode {
    /// The device's own name, as `doctor` shows it.
    name: String,
    /// `event<N>`.
    node: String,
    /// Why opening it failed, if it did.
    refused: Option<String>,
}

/// Every keyboard has to be readable, not just some device: a command reading
/// `/dev/input` hears only what it can open, and one that can open a game pad but no
/// keyboard hears nothing the user types. (Seen in the field: a session that grants its
/// user the game pads passed the old any-device check, so `detect-key` never asked for
/// sudo and listened to the pads alone.)
fn check_evdev_readable() -> CheckResult {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return CheckResult::Fail("/dev/input cannot be listed".into());
    };
    let mut keyboards = Vec::new();
    for entry in entries.flatten() {
        let Some(node) = entry
            .file_name()
            .to_str()
            .filter(|name| name.starts_with("event"))
            .map(str::to_owned)
        else {
            continue;
        };
        // sysfs describes every device, readable or not, which is what lets a keyboard
        // this process cannot open still be named.
        let sysfs = Path::new("/sys/class/input").join(&node).join("device");
        let caps = std::fs::read_to_string(sysfs.join("capabilities/key")).unwrap_or_default();
        if !caps_describe_a_keyboard(&caps) {
            continue;
        }
        let name = std::fs::read_to_string(sysfs.join("name"))
            .map_or_else(|_| node.clone(), |name| name.trim().to_owned());
        let refused = std::fs::File::open(entry.path())
            .err()
            .map(|err| err.to_string());
        keyboards.push(KeyboardNode {
            name,
            node,
            refused,
        });
    }
    keyboards_verdict(keyboards)
}

/// The verdict on the keyboards found: a pass only when there is one and every one opened.
/// Refusals are grouped by device name and reason, since one physical keyboard often
/// shows up as several nodes.
fn keyboards_verdict(mut keyboards: Vec<KeyboardNode>) -> CheckResult {
    if keyboards.is_empty() {
        return CheckResult::Fail("no keyboard found among the input devices".into());
    }
    keyboards.sort_by_key(|keyboard| node_number(&keyboard.node));
    let total = keyboards.len();
    let mut refusals: Vec<(String, String, Vec<String>)> = Vec::new();
    for keyboard in keyboards {
        let Some(reason) = keyboard.refused else {
            continue;
        };
        match refusals
            .iter_mut()
            .find(|(name, why, _)| *name == keyboard.name && *why == reason)
        {
            Some((_, _, nodes)) => nodes.push(keyboard.node),
            None => refusals.push((keyboard.name, reason, vec![keyboard.node])),
        }
    }
    if refusals.is_empty() {
        return CheckResult::Pass;
    }
    let unreadable: usize = refusals.iter().map(|(_, _, nodes)| nodes.len()).sum();
    let named = refusals
        .iter()
        .map(|(name, why, nodes)| format!("{name} ({}): {why}", nodes.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    CheckResult::Fail(format!(
        "{unreadable} of {total} keyboard devices cannot be read — {named}"
    ))
}

/// `event<N>`'s number, so the report lists devices in the kernel's order rather than
/// the directory's (`event10` after `event9`, not after `event1`).
fn node_number(node: &str) -> u32 {
    node.trim_start_matches("event").parse().unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_requirement_explains_itself() {
        let all = [
            Requirement::UinputModuleLoaded,
            Requirement::UinputNodeWritable,
            Requirement::EvdevReadable,
        ];
        for requirement in all {
            let fix = requirement.remediation();
            assert!(!requirement.label().is_empty());
            assert!(!fix.title.is_empty(), "{requirement:?} has no title");
            assert!(!fix.why.is_empty(), "{requirement:?} does not say why");
            assert!(!fix.steps.is_empty(), "{requirement:?} offers no steps");
        }
    }

    #[test]
    fn the_privilege_warning_is_attached_to_the_privileged_requirements() {
        // Asking a user to join the `input` group without telling them it grants
        // keylogging capability would not be an honest trade.
        for requirement in [Requirement::UinputNodeWritable, Requirement::EvdevReadable] {
            assert!(
                requirement.remediation().caution.is_some(),
                "{requirement:?} must carry the input-group warning"
            );
        }
    }

    /// What the kernel prints for an AT keyboard, a mouse, a power button and a game pad:
    /// only the keyboard has every code from Escape to S.
    #[test]
    fn only_a_full_key_set_describes_a_keyboard() {
        assert!(caps_describe_a_keyboard(
            "402000000 3803078f800d001 feffffdfffefffff fffffffffffffffe\n"
        ));
        assert!(!caps_describe_a_keyboard("ff0000 0 0 0 0"), "a mouse");
        assert!(
            !caps_describe_a_keyboard("10000000000000 0"),
            "a power button"
        );
        assert!(
            !caps_describe_a_keyboard("7cdb000000000000 0 0 0 0"),
            "a game pad"
        );
        assert!(!caps_describe_a_keyboard("0"), "no keys at all");
        assert!(!caps_describe_a_keyboard(""), "an unreadable sysfs file");
        assert!(
            caps_describe_a_keyboard("ffffffff fffffffe"),
            "a 32-bit kernel's words still cover codes 1-31"
        );
        // One key short of the set is not a keyboard: KEY_S (31) missing.
        assert!(!caps_describe_a_keyboard("7ffffffe"));
    }

    fn keyboard(name: &str, node: &str, refused: Option<&str>) -> KeyboardNode {
        KeyboardNode {
            name: name.to_owned(),
            node: node.to_owned(),
            refused: refused.map(str::to_owned),
        }
    }

    /// The field case: every keyboard refused, so the check must fail and say which —
    /// one physical keyboard's several nodes on one line, in the kernel's order.
    #[test]
    fn an_unreadable_keyboard_fails_the_check_and_is_named() {
        let denied = "Permission denied (os error 13)";
        let verdict = keyboards_verdict(vec![
            keyboard("Corsair K95 Keyboard", "event10", Some(denied)),
            keyboard("Corsair K95 Keyboard", "event9", Some(denied)),
            keyboard("G502 Keyboard", "event5", Some(denied)),
        ]);
        let CheckResult::Fail(detail) = verdict else {
            panic!("unreadable keyboards must fail the check: {verdict:?}");
        };
        assert!(
            detail.starts_with("3 of 3 keyboard devices cannot be read"),
            "{detail}"
        );
        assert!(
            detail.contains("G502 Keyboard (event5)")
                && detail.contains("Corsair K95 Keyboard (event9, event10)"),
            "{detail}"
        );
        assert!(
            detail.find("G502").unwrap() < detail.find("Corsair").unwrap(),
            "kernel order: event5 comes first: {detail}"
        );
    }

    /// One readable keyboard does not excuse another: the command would still miss
    /// what is typed on the second.
    #[test]
    fn every_keyboard_has_to_be_readable() {
        let verdict = keyboards_verdict(vec![
            keyboard("Laptop keyboard", "event3", None),
            keyboard(
                "USB keyboard",
                "event8",
                Some("Permission denied (os error 13)"),
            ),
        ]);
        assert!(
            matches!(&verdict, CheckResult::Fail(detail) if detail.starts_with("1 of 2")),
            "{verdict:?}"
        );
        let all_open = keyboards_verdict(vec![
            keyboard("Laptop keyboard", "event3", None),
            keyboard("USB keyboard", "event8", None),
        ]);
        assert_eq!(all_open, CheckResult::Pass);
    }

    /// With no keyboard at all there is nothing for the commands to hear, so it is not a
    /// pass — whatever else is readable.
    #[test]
    fn no_keyboard_at_all_is_not_a_pass() {
        assert!(matches!(
            keyboards_verdict(Vec::new()),
            CheckResult::Fail(detail) if detail.contains("no keyboard")
        ));
    }

    #[test]
    fn checks_return_a_verdict_rather_than_panicking() {
        // Whatever this machine looks like -- container, CI runner, desktop -- every
        // check has to answer rather than blow up.
        for requirement in [
            Requirement::UinputModuleLoaded,
            Requirement::UinputNodeWritable,
            Requirement::EvdevReadable,
        ] {
            let _ = requirement.check();
        }
    }
}
