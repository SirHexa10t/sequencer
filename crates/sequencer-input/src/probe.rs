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
    /// `/dev/input/event*` is readable by this user.
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
            Self::EvdevReadable => "/dev/input readable",
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
                title: "/dev/input/event* is not readable",
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
    if std::path::Path::new(path).exists() {
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

fn check_evdev_readable() -> CheckResult {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return CheckResult::Fail("/dev/input cannot be listed".into());
    };
    let mut seen = 0_u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"))
        {
            continue;
        }
        seen += 1;
        if std::fs::File::open(&path).is_ok() {
            return CheckResult::Pass;
        }
    }
    if seen == 0 {
        CheckResult::Fail("no /dev/input/event* devices found".into())
    } else {
        CheckResult::Fail(format!("none of the {seen} input devices could be opened"))
    }
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
