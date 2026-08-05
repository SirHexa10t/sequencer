//! What can go wrong, and what exit code it means.

use sequencer_core::SinkError;
use sequencer_core::validate::ConfigError;

use crate::exit;

/// Anything that stops a subcommand finishing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The settings do not describe a runnable profile.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The capture side failed.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[error(transparent)]
    Capture(#[from] sequencer_input::linux::CaptureError),

    /// A benchmark could not run.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[error(transparent)]
    Bench(#[from] sequencer_input::linux::BenchError),

    /// An X11 hotkey could not be grabbed.
    ///
    /// A usage error, not a runtime one: the fix is a different `--activate` or `--quit`
    /// key, which is the user's command line rather than their machine.
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    #[error(transparent)]
    Grab(#[from] sequencer_input::x11::GrabError),

    /// The injection side failed.
    #[error(transparent)]
    Sink(#[from] SinkError),

    /// A simulation script could not be read.
    #[error("{path}: {source}")]
    ScriptRead {
        /// The file that could not be read.
        path: String,
        /// Why.
        source: std::io::Error,
    },

    /// A simulation script line does not parse.
    #[error("line {line}: {detail}")]
    Script {
        /// Which line, counting from one.
        line: usize,
        /// What was wrong with it.
        detail: String,
    },

    /// Writing output failed.
    #[error("could not write output")]
    Io(#[from] std::io::Error),

    /// Session mode could not shed root after opening the devices.
    ///
    /// The password prompt promised the drop; running on as root would break that
    /// promise silently, so the run is refused instead.
    #[error("could not drop root after opening the devices: {0}")]
    Privilege(String),

    /// Something real but not built yet.
    ///
    /// Kept as an explicit variant rather than faked: a subcommand that pretends to work
    /// and quietly does nothing is worse than one that says what is missing.
    #[error("{0}")]
    NotImplemented(String),
}

impl Error {
    /// What the process should exit with.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            // Bad settings are the user's command line being wrong, which is a usage
            // error even when the parser accepted each flag on its own.
            Self::Config(_) | Self::Script { .. } => exit::USAGE,
            // The offending key came from the command line, and the fix is to change it.
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Self::Grab(_) => exit::USAGE,
            #[cfg(all(feature = "evdev", target_os = "linux"))]
            Self::Capture(_) | Self::Bench(_) => exit::FAILURE,
            Self::Sink(_)
            | Self::Io(_)
            | Self::ScriptRead { .. }
            | Self::Privilege(_)
            | Self::NotImplemented(_) => exit::FAILURE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_settings_are_a_usage_error() {
        let err = Error::Config(ConfigError::CpsOutOfRange(0.0));
        assert_eq!(err.exit_code(), exit::USAGE);

        let err = Error::Script {
            line: 3,
            detail: "nope".into(),
        };
        assert_eq!(err.exit_code(), exit::USAGE);
        assert!(err.to_string().starts_with("line 3:"));
    }

    #[test]
    fn a_missing_backend_is_a_runtime_failure() {
        let err = Error::NotImplemented("not yet".into());
        assert_eq!(err.exit_code(), exit::FAILURE);
    }
}
