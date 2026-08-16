//! A tiny deterministic generator, for jitter and chance rolls.
//!
//! Seeded by the runner, so a given seed replays a given run exactly. That property is
//! worth more here than statistical quality: jitter exists to break up a recognisable
//! cadence, and a profile's `RNG` steps exist to vary behaviour — neither is
//! cryptographic material, and both are worth being able to replay under a fixed seed.

/// xorshift64\*.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Seeds the generator. A zero seed is replaced, since xorshift is stuck at zero.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// The next raw 64-bit value.
    #[must_use]
    pub const fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..=max`.
    ///
    /// Uses the multiply-shift reduction, which has a negligible modulo bias for the
    /// small ranges jitter uses and needs no rejection loop.
    #[must_use]
    pub const fn at_most(&mut self, max: u64) -> u64 {
        if max == u64::MAX {
            return self.next_u64();
        }
        let span = max + 1;
        ((self.next_u64() as u128 * span as u128) >> 64) as u64
    }
}

impl Rng {
    /// A value in `[0, 1)`, from the top 53 bits — every f64 in the interval exactly
    /// representable, which is what makes `roll < chance` behave at both ends:
    /// a chance of 1.0 always passes and a chance of 0.0 never does.
    #[must_use]
    pub const fn unit(&mut self) -> f64 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "53 bits into an f64 mantissa is exact by construction, divisor included"
        )]
        {
            let top = (self.next_u64() >> 11) as f64;
            top / ((1_u64 << 53) as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_replays_the_same_sequence() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn zero_seed_is_not_stuck() {
        let mut r = Rng::new(0);
        let first = r.next_u64();
        assert_ne!(first, 0);
        assert_ne!(first, r.next_u64());
    }

    #[test]
    fn at_most_stays_in_range() {
        let mut r = Rng::new(7);
        for max in [0_u64, 1, 2, 1000, u64::MAX] {
            for _ in 0..1000 {
                assert!(r.at_most(max) <= max, "exceeded {max}");
            }
        }
    }

    #[test]
    fn unit_stays_in_the_half_open_interval() {
        let mut r = Rng::new(9);
        for _ in 0..10_000 {
            let roll = r.unit();
            assert!((0.0..1.0).contains(&roll), "{roll}");
        }
    }

    #[test]
    fn at_most_zero_is_always_zero() {
        let mut r = Rng::new(7);
        assert!((0..100).all(|_| r.at_most(0) == 0));
    }
}
