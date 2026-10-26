//! The stderr logger behind `-q`, `-v` and `-vv`, for a `main` that wants one.
//!
//! Every crate in the workspace logs through the `log` facade, and nothing prints until a
//! logger is installed — the library never installs one itself. Each line is the time
//! since logging began, the level, and the message, with any values the message reports
//! written after it as `name=value`.
//!
//! `log` rather than `tracing` (2026-09-30): nothing here opens a span or reads a logged
//! value back, and `tracing` with its subscriber brought nine crates into the build for
//! what this file does.

use std::fmt;
use std::io::Write as _;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Installs the stderr logger, at the level the command line asked for.
///
/// The library never calls this; `main` does. Doing it twice in one process fails, which
/// is exactly why it is not done for you.
///
/// # Errors
///
/// If a logger is already installed in this process.
pub fn init_logging(verbose: u8, quiet: bool) -> Result<(), LoggingAlreadyInitialized> {
    STDERR.started.get_or_init(Instant::now);
    log::set_logger(&STDERR).map_err(|_| LoggingAlreadyInitialized)?;
    log::set_max_level(level_for(verbose, quiet));
    Ok(())
}

/// Returned when a logger is already installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a logger is already installed in this process")]
pub struct LoggingAlreadyInitialized;

/// What `-q` and each `-v` let through: warnings by default, one level more per `-v`.
///
/// A plain level, not a filter-directive string. A directive parser (`env_logger`'s
/// `RUST_LOG`) would be a dependency for per-module filtering nobody is going to write
/// for a tool this size.
fn level_for(verbose: u8, quiet: bool) -> LevelFilter {
    if quiet {
        return LevelFilter::Error;
    }
    match verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

/// The one logger. `log` keeps a `&'static` to whatever is installed, hence a static.
static STDERR: StderrLogger = StderrLogger {
    started: OnceLock::new(),
};

/// Writes each record to stderr as one line.
#[derive(Debug)]
struct StderrLogger {
    /// When logging began: every line is stamped with the time since.
    started: OnceLock<Instant>,
}

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let elapsed = self.started.get().map_or(Duration::ZERO, Instant::elapsed);
        // A logger must never be what fails a run: with stderr gone, the line is lost.
        let _ = write_line(
            &mut std::io::stderr().lock(),
            elapsed,
            record.level(),
            record.args(),
        );
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// One record as one line: seconds since logging began, the level, the message.
///
/// The time and the level are padded to fixed widths, so every message starts in the same
/// column whatever its level.
fn write_line(
    out: &mut dyn std::io::Write,
    elapsed: Duration,
    level: Level,
    message: &fmt::Arguments<'_>,
) -> std::io::Result<()> {
    writeln!(out, "{:>10.6} {level:>5} {message}", elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(elapsed: Duration, level: Level, message: fmt::Arguments<'_>) -> String {
        let mut out = Vec::new();
        write_line(&mut out, elapsed, level, &message).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Each `-v` lets one more level through, and `-q` wins over any number of them.
    #[test]
    fn each_verbose_flag_lets_one_more_level_through() {
        assert_eq!(
            level_for(0, false),
            LevelFilter::Warn,
            "warnings by default"
        );
        assert_eq!(level_for(1, false), LevelFilter::Info);
        assert_eq!(level_for(2, false), LevelFilter::Debug);
        assert_eq!(level_for(3, false), LevelFilter::Trace);
        assert_eq!(
            level_for(u8::MAX, false),
            LevelFilter::Trace,
            "nothing past trace"
        );
        assert_eq!(
            level_for(0, true),
            LevelFilter::Error,
            "-q leaves only errors"
        );
        assert_eq!(level_for(2, true), LevelFilter::Error, "-q wins over -v");
    }

    /// A line says when, how severe and what, with the reported value after the message
    /// as `name=value`, the way the call sites write it.
    #[test]
    fn a_line_is_the_time_the_level_and_the_message() {
        let text = line(
            Duration::from_micros(4_213),
            Level::Warn,
            format_args!("dangling profile link ignored link={}", "p.toml"),
        );
        let words: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(
            words,
            [
                "0.004213",
                "WARN",
                "dangling",
                "profile",
                "link",
                "ignored",
                "link=p.toml"
            ]
        );
        assert!(text.ends_with('\n'), "one record, one line: {text:?}");
        assert_eq!(
            text.matches('\n').count(),
            1,
            "one record, one line: {text:?}"
        );
    }

    /// Layout, apart from content: the message starts in the same column at every level,
    /// so a log reads as one column of messages.
    #[test]
    fn messages_line_up_whatever_the_level() {
        let columns: Vec<usize> = [
            Level::Error,
            Level::Warn,
            Level::Info,
            Level::Debug,
            Level::Trace,
        ]
        .into_iter()
        .map(|level| {
            line(Duration::from_secs(1), level, format_args!("message"))
                .find("message")
                .unwrap()
        })
        .collect();
        assert!(
            columns.windows(2).all(|pair| pair[0] == pair[1]),
            "{columns:?}"
        );
    }

    /// The contract `main` relies on: the first install takes, and a second reports that a
    /// logger is already there instead of replacing it. Installed at `-q`, a level nothing
    /// in the workspace logs at, so the rest of this test binary stays quiet.
    #[test]
    fn a_second_install_is_refused() {
        assert_eq!(init_logging(0, true), Ok(()));
        assert_eq!(init_logging(2, false), Err(LoggingAlreadyInitialized));
        assert_eq!(
            log::max_level(),
            LevelFilter::Error,
            "the refused install changed nothing"
        );
    }
}
