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

/// A snapshot taken while the loop is still running, so a caller can show progress rather
/// than a frozen terminal. Same two counters as [`BenchResult`], mid-flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchSample {
    /// Press/release pairs written so far.
    pub emitted: u64,
    /// Pairs read back off the device so far. Lags `emitted` slightly — the reader is
    /// asynchronous — which is why the final figure gets a settling pause and this one is
    /// explicitly a *live* reading, not a verdict.
    pub delivered: u64,
    /// Time since the first write.
    pub elapsed: Duration,
}

impl BenchSample {
    /// Pairs per second written, so far.
    #[must_use]
    pub fn emitted_rate(&self) -> f64 {
        rate(self.emitted, self.elapsed)
    }

    /// Pairs per second read back, so far.
    #[must_use]
    pub fn delivered_rate(&self) -> f64 {
        rate(self.delivered, self.elapsed)
    }
}

/// How often [`BenchObserver::sample`] fires. Fast enough to look live, slow enough that
/// the rendering cannot itself become the bottleneck being measured.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(250);

/// Hooks into a running benchmark. Both methods have defaults, so a caller that wants
/// neither passes a unit struct and reads only the final [`BenchResult`].
pub trait BenchObserver {
    /// Called once, after the virtual device is created and opened for read-back, before
    /// any measuring begins.
    ///
    /// This is the window a caller that elevated *only to open the devices* uses to shed
    /// that privilege — the descriptors are already open, so the measurement runs
    /// unprivileged. Returning an error aborts the benchmark before it writes anything.
    ///
    /// # Errors
    ///
    /// Whatever the caller could not do; the benchmark reports it and stops.
    fn devices_open(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    /// Called roughly every [`SAMPLE_EVERY`] while the loop runs.
    fn sample(&mut self, _sample: BenchSample) {}
}

/// An observer that does nothing — the plain "just measure it" case.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unobserved;

impl BenchObserver for Unobserved {}

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
pub fn run(
    cps: Option<f64>,
    seconds: f64,
    observer: &mut dyn BenchObserver,
) -> Result<BenchResult, BenchError> {
    let mut sink = UinputSink::open().map_err(|source| BenchError::Device(Box::new(source)))?;

    // Read our own device back. Everything else in this crate deliberately excludes it;
    // here it is the whole point.
    let node = sink
        .dev_nodes()
        .map_err(BenchError::DevNodes)?
        .into_iter()
        .next()
        .ok_or(BenchError::NoDevNode)?;
    let mut device = Device::open(&node).map_err(BenchError::DevNodes)?;
    // Grab OUR OWN virtual device exclusively. The observe-only rule protects the
    // user's real devices; this one is ours, and without the grab the desktop's
    // libinput receives the whole measurement storm as real input — thousands of
    // events a second that lag the pointer and flicker the session. Grabbed, only
    // the counter thread sees them; the device (and grab) dies with this process.
    device.grab().map_err(BenchError::DevNodes)?;

    let delivered = Arc::new(AtomicU64::new(0));
    // Everything that needs privilege has happened: the device exists and is open for
    // read-back. A caller elevated purely for that drops it here, before a single write.
    observer
        .devices_open()
        .map_err(|source| BenchError::Aborted { source })?;

    let reading = Arc::new(AtomicBool::new(true));
    spawn_counter(device, Arc::clone(&delivered), Arc::clone(&reading));

    // Let the reader get to its first blocking read before the writes start, so the
    // count is not short by whatever it missed while spawning.
    thread::sleep(Duration::from_millis(50));

    let emitted = drive(
        &mut sink,
        cps,
        Duration::from_secs_f64(seconds),
        &delivered,
        observer,
    )?;
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
    delivered: &AtomicU64,
    observer: &mut dyn BenchObserver,
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
    let mut next_sample = started + SAMPLE_EVERY;

    while Instant::now() < deadline {
        sink.emit(&down)
            .map_err(|e| BenchError::Emit(Box::new(e)))?;
        sink.emit(&up).map_err(|e| BenchError::Emit(Box::new(e)))?;
        sink.flush().map_err(|e| BenchError::Emit(Box::new(e)))?;
        count += 1;

        // Checked against a wall-clock deadline rather than every Nth write: at an
        // unbounded ceiling run the write count per second is exactly what is unknown,
        // so a count-based interval would sample wildly too often or too rarely.
        let now = Instant::now();
        if now >= next_sample {
            observer.sample(BenchSample {
                emitted: count,
                delivered: delivered.load(Ordering::Relaxed),
                elapsed: now.duration_since(started),
            });
            next_sample = now + SAMPLE_EVERY;
        }

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
    /// The observer refused to continue once the devices were open.
    #[error("benchmark stopped before measuring: {source}")]
    Aborted {
        /// Why the caller stopped it.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live sample must divide by *its own* elapsed span, not the whole run — a
    /// mid-flight reading that used the total duration would report a rate climbing from
    /// near-zero rather than the steady one actually being achieved.
    #[test]
    fn a_live_sample_reports_the_rate_so_far_not_a_fraction_of_the_target() {
        let half_way = BenchSample {
            emitted: 500,
            delivered: 480,
            elapsed: Duration::from_millis(500),
        };
        assert!((half_way.emitted_rate() - 1000.0).abs() < f64::EPSILON);
        assert!((half_way.delivered_rate() - 960.0).abs() < f64::EPSILON);
    }

    /// An observer that wants nothing must not have to say so twice: both hooks default,
    /// so `Unobserved` is a complete implementation.
    #[test]
    fn the_unobserved_case_needs_no_methods() {
        let mut observer = Unobserved;
        assert!(observer.devices_open().is_ok());
        observer.sample(BenchSample {
            emitted: 1,
            delivered: 1,
            elapsed: Duration::from_secs(1),
        });
    }

    /// A refusal at the devices-open hook stops the run before anything is written — the
    /// promise session mode relies on when the drop fails.
    #[test]
    fn a_refusal_once_devices_are_open_aborts_and_keeps_the_reason() {
        struct Refuses;
        impl BenchObserver for Refuses {
            fn devices_open(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                Err("could not drop root".into())
            }
        }
        let err = BenchError::Aborted {
            source: Box::new(io::Error::other("could not drop root")),
        };
        assert!(
            err.to_string().contains("stopped before measuring"),
            "{err}"
        );
        assert!(
            std::error::Error::source(&err).is_some(),
            "the cause must survive"
        );
        // The trait object is usable as written (compile-time half of the guarantee).
        let mut refuses = Refuses;
        assert!(
            (&mut refuses as &mut dyn BenchObserver)
                .devices_open()
                .is_err()
        );
    }

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
        /// Counts the progress callbacks: a live rate nobody ever receives is the same as
        /// no live rate at all.
        struct Counting {
            samples: u32,
        }
        impl BenchObserver for Counting {
            fn sample(&mut self, sample: BenchSample) {
                assert!(
                    sample.elapsed > Duration::ZERO,
                    "a sample must span some time"
                );
                self.samples += 1;
            }
        }

        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_err()
        {
            eprintln!("skipping: /dev/uinput is not writable here");
            return;
        }
        // At 250ms apart over 0.3s, at least one sample is due.
        let mut observer = Counting { samples: 0 };
        let result = run(Some(200.0), 0.3, &mut observer).expect("bench should run");
        assert!(
            observer.samples > 0,
            "the run reported no live progress at all"
        );
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
