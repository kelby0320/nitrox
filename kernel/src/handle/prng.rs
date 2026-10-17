//! Tiny xorshift64 PRNG.
//!
//! Used by the handle table to shuffle each new segment's freelist, so the low
//! 20 bits of a fresh handle are unpredictable to anyone not issued an adjacent
//! handle: defence in depth atop the owner-PID check and 32-bit generation
//! (`docs/spec/handle-encoding.md` § "Validation algorithm",
//! `docs/rationale/why-capabilities.md`).
//!
//! Cryptographic strength is **not** a goal: the seed (drawn from the entropy
//! CSPRNG) affects only the distribution of freshly-allocated slot indices,
//! never a rights or owner check.

/// A 64-bit xorshift PRNG (Marsaglia; shifts `(13, 7, 17)`): full period
/// `2^64 - 1` for any non-zero seed.
pub(crate) struct Xorshift64(u64);

/// Substituted for a `0` seed, the one state the xorshift cycle excludes.
const FALLBACK_SEED: u64 = 0xDEAD_BEEF_CAFE_F00D;

impl Xorshift64 {
    /// Seeded from `seed`; `0` becomes [`FALLBACK_SEED`].
    pub(crate) const fn new(seed: u64) -> Self {
        Self(if seed == 0 { FALLBACK_SEED } else { seed })
    }

    /// Advance the state and return the next 64-bit output.
    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A uniform `u32` in `0..n` by rejection sampling, so unbiased. Panics if
    /// `n == 0`.
    pub(crate) fn gen_below(&mut self, n: u32) -> u32 {
        debug_assert!(n > 0, "gen_below requires n > 0");
        // Values at or above the largest multiple of `n` in `u32` are rejected.
        let bound = u32::MAX - (u32::MAX % n);
        loop {
            let v = self.next_u64() as u32;
            if v < bound {
                return v % n;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_seed_falls_back_and_still_produces_output() {
        let mut p = Xorshift64::new(0);
        let a = p.next_u64();
        let b = p.next_u64();
        assert_ne!(a, b);
    }

    #[test]
    fn same_seed_yields_same_sequence() {
        let mut a = Xorshift64::new(0xCAFE);
        let mut b = Xorshift64::new(0xCAFE);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Xorshift64::new(1);
        let mut b = Xorshift64::new(2);
        let mut differed = false;
        for _ in 0..8 {
            if a.next_u64() != b.next_u64() {
                differed = true;
                break;
            }
        }
        assert!(differed);
    }

    #[test]
    fn gen_below_respects_bound() {
        let mut p = Xorshift64::new(0x9E37_79B9);
        for _ in 0..1000 {
            assert!(p.gen_below(17) < 17);
            assert!(p.gen_below(4096) < 4096);
            assert_eq!(p.gen_below(1), 0);
        }
    }

    #[test]
    fn gen_below_covers_full_range_with_a_few_thousand_draws() {
        let mut p = Xorshift64::new(0x1234_5678);
        let mut seen = [false; 16];
        for _ in 0..1_000 {
            seen[p.gen_below(16) as usize] = true;
        }
        assert!(seen.iter().all(|&v| v), "every value in 0..16 should appear at least once in 1000 draws",);
    }
}
