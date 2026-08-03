//! Measuring how fast synthetic input actually gets through.
//!
//! The engine's own accounting can only report what it *wrote*. That is not the same as
//! what arrived: the kernel, or whatever is reading the device, can coalesce or drop
//! events under load. So this writes through the same [`UinputSink`] the clicker uses,
//! and simultaneously **reads its own virtual device back**, counting what a real
//! consumer would have seen. The gap between the two numbers is the interesting part.
//!
//! It presses [`Key::F24`] rather than clicking. F24 exists on no physical keyboard and
//! essentially nothing binds to it, so a benchmark does not click on whatever happens to
//! be under the pointer — but it is still a real key press, so this is not something to
//! run with a text editor focused and unsaved work open.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{Device, EventSummary, KeyCode};
use sequencer_core::emit::{Emit, EmitAction, InputSink};
use sequencer_core::input::Key;
use sequencer_core::time::Timestamp;

use crate::linux::UinputSink;

/// What a benchmark measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchResult {
    /// Press/release pairs written to the device.
    pub emitted: u64,
    /// Press/release pairs read back off the device.
    pub delivered: u64,
    /// How long the measured loop ran.
    pub elapsed: Duration,
}

impl BenchResult {
    /// Pairs per second written.
    #[must_use]
    pub fn emitted_rate(&self) -> f64 {
        rate(self.emitted, self.elapsed)
    }

    /// Pairs per second that arrived.
    #[must_use]
    pub fn delivered_rate(&self) -> f64 {
        rate(self.delivered, self.elapsed)
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "counts this large would need years of clicking"
)]
fn rate(count: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds > 0.0 {
        count as f64 / seconds
    } else {
        0.0
    }
}

/// Runs a benchmark for `seconds`, at `cps` if given or flat out if not.
///
/// # Errors
///
/// If the virtual device cannot be created or its own event node cannot be found.
pub fn run(cps: Option<f64>, seconds: f64) -> Result<BenchResult, BenchError> {
    let mut sink = UinputSink::open().map_err(|source| BenchError::Device(Box::new(source)))?;

    // Read our own device back. Everything else in this crate deliberately excludes it;
    // here it is the whole point.
    let node = sink
        .dev_nodes()
        .map_err(BenchError::DevNodes)?
        .into_iter()
        .next()
        .ok_or(BenchError::NoDevNode)?;
    let device = Device::open(&node).map_err(BenchError::DevNodes)?;

    let delivered = Arc::new(AtomicU64::new(0));
    let reading = Arc::new(AtomicBool::new(true));
    spawn_counter(device, Arc::clone(&delivered), Arc::clone(&reading));

    // Let the reader get to its first blocking read before the writes start, so the
    // count is not short by whatever it missed while spawning.
    thread::sleep(Duration::from_millis(50));

    let emitted = drive(&mut sink, cps, Duration::from_secs_f64(seconds))?;
    let elapsed_at_stop = Instant::now();

    // Reading is asynchronous, so give the kernel a moment to hand over what is already
    // queued before the count is taken. Without this the delivered figure understates by
    // however much was in flight.
    thread::sleep(Duration::from_millis(200));
    reading.store(false, Ordering::SeqCst);

    Ok(BenchResult {
        emitted: emitted.count,
        delivered: delivered.load(Ordering::Relaxed),
        elapsed: elapsed_at_stop.duration_since(emitted.started),
    })
}

struct Emitted {
    count: u64,
    started: Instant,
}

/// Writes press/release pairs until the time is up.
fn drive(
    sink: &mut UinputSink,
    cps: Option<f64>,
    duration: Duration,
) -> Result<Emitted, BenchError> {
    let down = Emit {
        at: Timestamp::ZERO,
        action: EmitAction::KeyDown(Key::F24),
        level: 0,
    };
    let up = Emit {
        at: Timestamp::ZERO,
        action: EmitAction::KeyUp(Key::F24),
        level: 0,
    };

    let period = cps
        .filter(|rate| *rate > 0.0)
        .map(|rate| Duration::from_secs_f64(1.0 / rate));
    let sleeper = spin_sleep::SpinSleeper::default();

    let started = Instant::now();
    let deadline = started + duration;
    let mut count = 0_u64;

    while Instant::now() < deadline {
        sink.emit(&down)
            .map_err(|e| BenchError::Emit(Box::new(e)))?;
        sink.emit(&up).map_err(|e| BenchError::Emit(Box::new(e)))?;
        sink.flush().map_err(|e| BenchError::Emit(Box::new(e)))?;
        count += 1;

        if let Some(period) = period {
            // Absolute deadlines, so a slow iteration does not push every later one back
            // and quietly understate the rate the machine can hold.
            let next = started + period.saturating_mul(u32::try_from(count).unwrap_or(u32::MAX));
            let now = Instant::now();
            if next > now {
                sleeper.sleep(next - now);
            }
        }
    }

    Ok(Emitted { count, started })
}

/// Counts F24 presses arriving on the device, until told to stop.
fn spawn_counter(mut device: Device, delivered: Arc<AtomicU64>, reading: Arc<AtomicBool>) {
    let spawned = thread::Builder::new()
        .name("sequencer-bench-reader".into())
        .spawn(move || {
            while reading.load(Ordering::Relaxed) {
                let Ok(events) = device.fetch_events() else {
                    return;
                };
                for event in events {
                    // Presses only: counting both edges would just double everything.
                    if let EventSummary::Key(_, KeyCode::KEY_F24, 1) = event.destructure() {
                        delivered.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
    if let Err(err) = spawned {
        tracing::warn!(%err, "could not start the bench reader; delivered will read zero");
    }
}

/// A benchmark could not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BenchError {
    /// The virtual device could not be created.
    #[error("{0}")]
    Device(#[source] Box<sequencer_core::SinkError>),
    /// Its event node could not be found or opened.
    #[error("cannot read back the virtual device: {0}")]
    DevNodes(#[source] io::Error),
    /// The device reported no event node at all.
    #[error("the virtual device reported no event node to read back from")]
    NoDevNode,
    /// A write failed part-way through.
    #[error("{0}")]
    Emit(#[source] Box<sequencer_core::SinkError>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_divide_by_the_measured_span() {
        let result = BenchResult {
            emitted: 1000,
            delivered: 900,
            elapsed: Duration::from_secs(2),
        };
        assert!((result.emitted_rate() - 500.0).abs() < 0.001);
        assert!((result.delivered_rate() - 450.0).abs() < 0.001);
    }

    #[test]
    fn a_zero_span_reports_zero_rather_than_infinity() {
        let result = BenchResult {
            emitted: 10,
            delivered: 10,
            elapsed: Duration::ZERO,
        };
        assert!((result.emitted_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_short_benchmark_reports_something_plausible() {
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_err()
        {
            eprintln!("skipping: /dev/uinput is not writable here");
            return;
        }
        let result = run(Some(200.0), 0.3).expect("bench should run");
        assert!(result.emitted > 0, "nothing was emitted");
        assert!(
            result.delivered <= result.emitted,
            "more delivered ({}) than emitted ({})",
            result.delivered,
            result.emitted
        );
        assert!(result.elapsed >= Duration::from_millis(250));
    }
}
