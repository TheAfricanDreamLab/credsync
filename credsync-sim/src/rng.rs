//! The one source of chance in the simulator.
//!
//! Every packet delay, drop, duplication, reorder, process kill and clock skew in a run comes
//! from here, and this is seeded from a single integer. That is what makes a bug report one
//! number: `--seed 0x4f21a9c3` replays the run exactly, on any machine, forever.
//!
//! # Why this is written by hand rather than pulled from a crate
//!
//! A generator from the ecosystem could change its output between versions — a bug fix, a
//! different stream-selection rule, a faster mixing function — and every recorded seed in every
//! bug issue would silently start replaying a *different* run. The seeds in this project's issue
//! tracker are meant to be good indefinitely, so the algorithm is pinned here where a change to
//! it is a visible diff rather than a dependency bump.
//!
//! It also keeps `rand` and `getrandom` out of the workspace entirely, which the sans-IO source
//! check watches for in the core crates.
//!
//! # PCG-XSH-RR 64/32
//!
//! O'Neill's permuted congruential generator: a 64-bit LCG whose state is permuted down to 32
//! bits of output. Small, fast, and statistically far better than the raw LCG underneath it —
//! which matters, because a generator with visible structure would explore the fault space in
//! patterns rather than freely, and the simulator would be busy without being thorough.
//!
//! Not cryptographic, and nothing here needs it to be.

/// A seeded, reproducible source of chance.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
    /// The seed this generator was built from, carried so a trace can report it.
    seed: u64,
}

/// The LCG multiplier from the reference implementation.
const MULTIPLIER: u64 = 6_364_136_223_846_793_005;
/// An odd increment, which is what gives the LCG its full period.
const INCREMENT: u64 = 1_442_695_040_888_963_407;

impl Rng {
    /// Builds a generator from a seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        // One advance before use, so seed 0 does not start from a degenerate state.
        let state = seed.wrapping_add(INCREMENT);
        let mut rng = Self { state, seed };
        rng.state = rng.state.wrapping_mul(MULTIPLIER).wrapping_add(INCREMENT);
        rng
    }

    /// The seed this generator came from.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// The next 32 bits.
    pub const fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MULTIPLIER).wrapping_add(INCREMENT);

        // XSH-RR: xorshift the high bits down, then rotate by a count taken from the top 5 bits.
        // The data-dependent rotation is what removes the lattice structure a plain LCG has.
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// The next 64 bits.
    pub const fn next_u64(&mut self) -> u64 {
        let hi = self.next_u32() as u64;
        let lo = self.next_u32() as u64;
        (hi << 32) | lo
    }

    /// A number in `0..n`, or `0` when `n` is zero.
    ///
    /// Uses rejection sampling rather than a modulo. A plain `% n` biases towards the low end
    /// whenever `n` does not divide 2^32 — small enough to miss in a spot check, and exactly the
    /// kind of skew that would quietly make rare faults rarer still.
    pub const fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let threshold = n.wrapping_neg() % n;
        loop {
            let v = self.next_u32();
            if v >= threshold {
                return v % n;
            }
        }
    }

    /// A number in `low..=high`, inclusive. Returns `low` if the range is inverted.
    pub const fn range(&mut self, low: u32, high: u32) -> u32 {
        if high <= low {
            return low;
        }
        low + self.below(high - low + 1)
    }

    /// True with probability `percent`/100.
    pub const fn chance(&mut self, percent: u32) -> bool {
        if percent == 0 {
            return false;
        }
        if percent >= 100 {
            return true;
        }
        self.below(100) < percent
    }

    /// A signed offset in `-magnitude..=magnitude`.
    pub const fn signed(&mut self, magnitude: i64) -> i64 {
        if magnitude <= 0 {
            return 0;
        }
        // `magnitude` bounded so the cast is exact; simulator magnitudes are days of milliseconds.
        let span = (magnitude as u64).wrapping_mul(2).wrapping_add(1);
        let pick = (self.next_u64() % span) as i64;
        pick - magnitude
    }

    /// Picks an index into a slice of length `len`, or `None` when empty.
    pub const fn index(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        // Simulator collections are small; a u32 range is plenty and keeps the draw to one word.
        Some(self.below(len as u32) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same seed produces the same sequence. If this ever fails, every recorded seed in every
    /// bug issue has become meaningless.
    #[test]
    fn a_seed_reproduces_its_sequence() {
        let mut a = Rng::new(0x4f21_a9c3);
        let mut b = Rng::new(0x4f21_a9c3);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let differs = (0..64).any(|_| a.next_u64() != b.next_u64());
        assert!(differs, "two seeds produced identical output");
    }

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut rng = Rng::new(7);
        let mut seen = [false; 5];
        for _ in 0..500 {
            let v = rng.below(5);
            assert!(v < 5);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "some values were never produced");
    }

    #[test]
    fn signed_spans_both_directions() {
        let mut rng = Rng::new(99);
        let mut negative = false;
        let mut positive = false;
        for _ in 0..500 {
            let v = rng.signed(3 * 24 * 60 * 60 * 1000);
            assert!(v.abs() <= 3 * 24 * 60 * 60 * 1000);
            if v < 0 {
                negative = true;
            }
            if v > 0 {
                positive = true;
            }
        }
        assert!(negative && positive, "skew must go both ways");
    }

    #[test]
    fn chance_respects_its_bounds() {
        let mut rng = Rng::new(5);
        for _ in 0..100 {
            assert!(!rng.chance(0), "0% must never fire");
            assert!(rng.chance(100), "100% must always fire");
        }
    }
}
