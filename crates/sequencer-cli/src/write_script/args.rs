//! `write-script`'s command-line surface.

use clap::Args;

use crate::args::GlobalArgs;

/// Everything `write-script` accepts — which is, for now, only what every command accepts.
///
/// Deliberately empty otherwise. Inventing flags before the feature exists would pin down
/// decisions this command has not earned yet, and a flag that parses but does nothing is
/// worse than one that isn't there.
#[derive(Args, Debug, Clone, PartialEq, Eq, Default)]
pub struct WriteScriptArgs {
    /// Shared options.
    #[command(flatten)]
    pub global: GlobalArgs,
}

impl WriteScriptArgs {
    /// The defaults, for a caller building this without going through clap.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            global: GlobalArgs::new(),
        }
    }
}
