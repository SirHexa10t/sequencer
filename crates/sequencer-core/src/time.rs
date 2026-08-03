//! Time, as the engine sees it.
//!
//! Nothing here reads a clock. [`Timestamp`] is a plain number the runner hands in, which
//! is what lets every timing test run against a virtual clock in microseconds instead of
//! sleeping for real.

use core::num::NonZeroU64;

pub use core::time::Duration;

use crate::validate::ConfigError;

/// Monotonic nanoseconds since an opaque epoch chosen by the runner.
///
/// Deliberately not `std::time::Instant`: the engine must not know about the OS. Every
/// operation saturates, so a clock that jumps backwards or a duration that overflows
/// produces a clamped value rather than a panic.
///
/// There is intentionally no `Sub` implementation. Subtraction that can panic is exactly
/// the backwards-clock bug; use [`Timestamp::saturating_sub`], which is total.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The epoch itself.
    pub const ZERO: Self = Self(0);

    /// Builds a timestamp from nanoseconds since the runner's epoch.
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Builds a timestamp from milliseconds since the runner's epoch. Test ergonomics.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(1_000_000))
    }

    /// Nanoseconds since the runner's epoch.
    #[must_use]
    pub const fn nanos(self) -> u64 {
        self.0
    }

    /// This timestamp advanced by `d`, saturating at [`u64::MAX`] nanoseconds.
    #[must_use]
    pub fn saturating_add(self, d: Duration) -> Self {
        Self(self.0.saturating_add(clamp_nanos(d)))
    }

    /// This timestamp advanced by `nanos`, saturating at [`u64::MAX`].
    #[must_use]
    pub const fn saturating_add_nanos(self, nanos: u64) -> Self {
        Self(self.0.saturating_add(nanos))
    }

    /// Time elapsed from `earlier` to `self`, or [`Duration::ZERO`] if `self` is earlier.
    #[must_use]
    pub const fn saturating_sub(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

/// A [`Duration`] as whole nanoseconds, clamped into a `u64`.
///
/// `Duration` counts nanoseconds in a `u128` and so can describe spans no monotonic clock
/// will ever reach. Clamping keeps the engine's arithmetic in `u64` without a fallible
/// conversion at every call site.
pub(crate) fn clamp_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Rounds a non-negative `f64` to the nearest `u64`, saturating.
///
/// `f64::round` lives in `std`, not `core`, so a `no_std` crate has to spell it out. The
/// alternative is a `libm` dependency for one call site.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "saturating on overflow is the documented behaviour of this function"
)]
fn round_to_u64(x: f64) -> u64 {
    // `as` saturates on overflow rather than wrapping, so a value past u64::MAX clamps.
    let truncated = x as u64;
    if x - (truncated as f64) >= 0.5 {
        truncated.saturating_add(1)
    } else {
        truncated
    }
}

/// A strictly positive repetition period.
///
/// The [`NonZeroU64`] is load-bearing rather than decorative: it makes `cps = 0`
/// unrepresentable, so the catch-up arithmetic in the engine cannot divide by zero and
/// there is no "period of zero means infinite loop" case to defend against downstream.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Period(NonZeroU64);

impl Period {
    /// Converts a rate in clicks (or key presses) per second into a period.
    ///
    /// There is deliberately no upper limit on the rate. A request faster than the
    /// machine can deliver degrades gracefully — the engine drops the slots it cannot
    /// reach and reports them — so an artificial ceiling here would only cap the
    /// achievable rate below what the hardware allows. Rates faster than one per
    /// nanosecond clamp to a 1 ns period, the resolution of the timeline itself.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::CpsOutOfRange`] if `cps` is not finite and positive.
    pub fn from_cps(cps: f64) -> Result<Self, ConfigError> {
        if !cps.is_finite() || cps <= 0.0 {
            return Err(ConfigError::CpsOutOfRange(cps));
        }
        // `unwrap_or` rather than `expect`: the clamp already rules zero out, and leaving
        // no panic path at all is better than documenting one that cannot happen.
        let nanos = round_to_u64(1.0e9_f64 / cps);
        Ok(Self(NonZeroU64::new(nanos).unwrap_or(NonZeroU64::MIN)))
    }

    /// Builds a period directly from nanoseconds.
    #[must_use]
    pub const fn from_nanos(nanos: NonZeroU64) -> Self {
        Self(nanos)
    }

    /// The period in nanoseconds. Never zero.
    #[must_use]
    pub const fn nanos(self) -> u64 {
        self.0.get()
    }

    /// The period as a [`Duration`].
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        Duration::from_nanos(self.0.get())
    }
}

/// Injected time. The only capability the engine needs from the outside world to be driven.
pub trait Clock {
    /// The current time on this clock.
    fn now(&self) -> Timestamp;

    /// Blocks until at least `deadline`.
    ///
    /// Returning early is explicitly allowed, so every caller must re-read [`Clock::now`]
    /// afterwards rather than assuming the deadline has passed. That contract is what
    /// lets a virtual clock implement this as a plain assignment, and what lets a real
    /// one undershoot on purpose and spin out the remainder.
    fn sleep_until(&self, deadline: Timestamp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cps_twenty_is_exactly_fifty_milliseconds() {
        assert_eq!(Period::from_cps(20.0).unwrap().nanos(), 50_000_000);
    }

    #[test]
    fn fractional_cps_is_exact() {
        assert_eq!(Period::from_cps(0.5).unwrap().nanos(), 2_000_000_000);
        assert_eq!(Period::from_cps(3.0).unwrap().nanos(), 333_333_333);
    }

    #[test]
    fn cps_zero_and_friends_are_rejected() {
        for bad in [0.0, -5.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                matches!(Period::from_cps(bad), Err(ConfigError::CpsOutOfRange(_))),
                "expected {bad} to be rejected as out of range"
            );
        }
    }

    #[test]
    fn there_is_no_artificial_ceiling() {
        // The ceiling is the machine, not this type. Absurd rates clamp to the 1 ns
        // resolution of the timeline rather than being refused.
        assert_eq!(Period::from_cps(10_000.0).unwrap().nanos(), 100_000);
        assert_eq!(Period::from_cps(1_000_000.0).unwrap().nanos(), 1_000);
        assert_eq!(Period::from_cps(1e9).unwrap().nanos(), 1);
        assert_eq!(Period::from_cps(1e300).unwrap().nanos(), 1);
    }

    #[test]
    fn absurdly_slow_rates_clamp_instead_of_overflowing() {
        // 1e-12 cps needs 1e21 ns, well past u64::MAX. It must clamp, not alias to a
        // tiny period that would machine-gun the display server.
        let p = Period::from_cps(1e-12).unwrap();
        assert_eq!(p.nanos(), u64::MAX);
    }

    #[test]
    fn timestamp_subtraction_of_a_later_value_is_zero_not_a_panic() {
        let early = Timestamp::from_millis(100);
        let late = Timestamp::from_millis(900);
        assert_eq!(late.saturating_sub(early), Duration::from_millis(800));
        assert_eq!(early.saturating_sub(late), Duration::ZERO);
    }

    #[test]
    fn timestamp_addition_saturates() {
        let t = Timestamp::from_nanos(u64::MAX - 5);
        assert_eq!(t.saturating_add(Duration::from_secs(1)).nanos(), u64::MAX);
    }
}
