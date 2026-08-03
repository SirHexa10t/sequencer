//! The `sequencer` binary.
//!
//! Deliberately the only file in the project that reads `argv` or decides a process exit
//! code. Everything else is reachable as a library, which is what lets another program
//! embed this one without inheriting its opinions about global state.
//!
//! It lives in `src/bin/` rather than being `src/main.rs` so that it cannot reach past
//! the library boundary with a `mod` declaration — the compiler enforces what would
//! otherwise be a convention nobody notices breaking.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Global setup belongs here, never in the library: a second subscriber in one process
    // fails, and an embedder must be free to install their own. The flags are sniffed
    // rather than parsed because logging has to be up before clap reports anything.
    #[cfg(feature = "logging")]
    {
        let mut verbosity = 0_u8;
        let mut quiet = false;
        for arg in std::env::args() {
            match arg.as_str() {
                "-v" | "--verbose" => verbosity = verbosity.saturating_add(1),
                "-vv" => verbosity = verbosity.saturating_add(2),
                "-q" | "--quiet" => quiet = true,
                _ => {}
            }
        }
        if let Err(err) = sequencer_cli::init_logging(verbosity, quiet) {
            eprintln!("warning: {err}");
        }
    }

    ExitCode::from(sequencer_cli::run(std::env::args_os()))
}
