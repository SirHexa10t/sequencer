//! What can go wrong, and what exit code it means.

use sequencer_core::SinkError;

use crate::exit;

/// Anything that stops a subcommand finishing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The capture side failed.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[error(transparent)]
    Capture(#[from] sequencer_input::linux::CaptureError),

    /// A benchmark could not run.
    #[cfg(all(feature = "evdev", target_os = "linux"))]
    #[error(transparent)]
    Bench(#[from] sequencer_input::linux::BenchError),

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

    /// A binds profile file is invalid.
    #[error("{path}: {detail}")]
    Profile {
        /// The file that was refused.
        path: String,
        /// Why, in the words of the format.
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
            // A profile that cannot run is the user's input being wrong, which is a usage
            // error even when the file itself was read fine.
            Self::Profile { .. } => exit::USAGE,
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
        let err = Error::Profile {
            path: "binds.toml".into(),
            detail: "nope".into(),
        };
        assert_eq!(err.exit_code(), exit::USAGE);
        assert!(err.to_string().starts_with("binds.toml:"));
    }

    #[test]
    fn a_missing_backend_is_a_runtime_failure() {
        let err = Error::NotImplemented("not yet".into());
        assert_eq!(err.exit_code(), exit::FAILURE);
    }
}
