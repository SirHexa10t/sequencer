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

use sequencer_cli::clap;

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

    // Parse here rather than in `run`: session mode may re-exec this command line under
    // sudo, and the decision belongs to the process that owns argv. A parse failure (or
    // `--help`) exits the way clap says, before sudo could ever be mentioned.
    let cli = match <sequencer_cli::Cli as clap::Parser>::try_parse_from(std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2));
        }
    };
    ExitCode::from(sequencer_cli::run_with_sudo_prompt(
        &cli,
        "sequencer doctor",
    ))
}
