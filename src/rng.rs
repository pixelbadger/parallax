//! Seeded randomness.
//!
//! Every random stream is xoshiro256++ seeded through SplitMix64, and range
//! sampling is done here rather than by a library, so a seed produces the same
//! draws on every platform and with every version of the dependencies.

use rand_xoshiro::Xoshiro256PlusPlus;
use rand_xoshiro::rand_core::{Rng as _, SeedableRng};

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
const FORK_SALT: u64 = 0x5350_4C5F_464F_524B; // "SPL_FORK"

/// SplitMix64's output function: a strong 64-bit mixer.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(GOLDEN);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Seed for universe `i` of a multiverse run under `base`. Deterministic per
/// (seed, index), so two multiverses of the same size see the same draws.
pub fn universe_seed(base: u64, i: u64) -> u64 {
    mix(mix(base) ^ i)
}

#[derive(Clone, Debug)]
pub struct Rng(Xoshiro256PlusPlus);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut bytes = [0u8; 32];
        for (k, chunk) in bytes.chunks_exact_mut(8).enumerate() {
            let word = mix(seed.wrapping_add((k as u64).wrapping_mul(GOLDEN)));
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        Rng(Xoshiro256PlusPlus::from_seed(bytes))
    }

    /// Seed for a fork's own stream: derived from the current state without
    /// advancing it, so forking never changes what this stream draws next.
    pub fn fork_seed(&self) -> u64 {
        self.0.state().chunks_exact(8).fold(FORK_SALT, |h, word| {
            mix(h ^ u64::from_le_bytes(word.try_into().expect("8-byte chunk")))
        })
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    /// Uniform in `0..s` (Lemire's nearly divisionless method, unbiased).
    fn below(&mut self, s: u64) -> u64 {
        debug_assert!(s > 0);
        let mut m = u128::from(self.next_u64()) * u128::from(s);
        if (m as u64) < s {
            let threshold = s.wrapping_neg() % s;
            while (m as u64) < threshold {
                m = u128::from(self.next_u64()) * u128::from(s);
            }
        }
        (m >> 64) as u64
    }

    /// Uniform in `lo..=hi`; the caller guarantees `lo <= hi`.
    pub fn int_in(&mut self, lo: i64, hi: i64) -> i64 {
        let span = hi.wrapping_sub(lo) as u64; // number of values minus one
        let offset = match span.checked_add(1) {
            Some(n) => self.below(n),
            None => self.next_u64(),
        };
        lo.wrapping_add(offset as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let (mut a, mut b) = (Rng::new(42), Rng::new(42));
        for _ in 0..100 {
            assert_eq!(a.int_in(0, 99), b.int_in(0, 99));
        }
    }

    #[test]
    fn int_in_respects_bounds() {
        let mut r = Rng::new(1);
        for _ in 0..10_000 {
            assert!((1..=6).contains(&r.int_in(1, 6)));
        }
        assert_eq!(r.int_in(5, 5), 5);
        r.int_in(i64::MIN, i64::MAX); // full range must not overflow
    }

    #[test]
    fn fork_seed_does_not_advance() {
        let mut r = Rng::new(7);
        let mut copy = r.clone();
        assert_eq!(r.fork_seed(), r.fork_seed());
        assert_eq!(r.next_u64(), copy.next_u64());
    }

    #[test]
    fn universes_differ() {
        assert_ne!(universe_seed(0, 0), universe_seed(0, 1));
        assert_ne!(universe_seed(0, 1), universe_seed(1, 0));
    }
}
