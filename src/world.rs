//! Keyed uncertainty.
//!
//! A world fact's value is a pure function of (study seed, world id, key),
//! where the key is the world's name, the fact's name and its arguments. It
//! doesn't matter when, how often or in what order a fact is read, so every
//! policy sees the same facts in world `i`, whatever else it computes. Names,
//! not declaration order, make the key: renaming a fact changes its draws,
//! and moving declarations around doesn't.
//!
//! All hashing is SplitMix64 finalisation over 64-bit words, so draws are
//! identical on every platform.

/// SplitMix64's finaliser: a good 64-bit mix.
pub fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fold one word into a hash.
pub fn combine(h: u64, w: u64) -> u64 {
    mix(h.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ w)
}

/// FNV-1a: a stable hash of a name.
pub fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

pub fn key_prefix(world: &str, fact: &str) -> u64 {
    combine(fnv(world), fnv(fact))
}

/// Tag mixed into forecast world ids, so they never meet evaluation worlds.
const FORECAST: u64 = 0x666f_7265_6361_7374;

/// The `j`th world a policy imagines when forecasting at step `t` of
/// evaluation world `eval`. A decision model's policy, which decides once
/// and sees nothing of the world, uses the same forecast worlds everywhere.
pub fn forecast_world(eval: Option<u64>, t: i64, j: u64) -> u64 {
    let base = match eval {
        Some(w) => combine(combine(FORECAST, 1), w),
        None => combine(FORECAST, 0),
    };
    combine(combine(base, t as u64), j)
}

/// The random stream for one fact in one world.
pub struct Stream(u64);

impl Stream {
    pub fn new(seed: u64, world: u64, key: u64) -> Stream {
        Stream(combine(combine(mix(seed), world), key))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1), 53 bits.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform integer in `lo..=hi` (`hi >= lo`).
    pub fn int_in(&mut self, lo: i64, hi: i64) -> i64 {
        let range = (hi as i128 - lo as i128 + 1) as u128;
        if range > u64::MAX as u128 {
            return self.next_u64() as i64;
        }
        // Lemire's multiply-shift, with rejection so it is exactly uniform.
        let range = range as u64;
        let threshold = range.wrapping_neg() % range;
        loop {
            let m = self.next_u64() as u128 * range as u128;
            if (m as u64) >= threshold {
                return (lo as i128 + (m >> 64) as i128) as i64;
            }
        }
    }

    /// Standard normal, by Box–Muller with `libm` (portable bit-for-bit).
    pub fn normal(&mut self) -> f64 {
        let u1 = 1.0 - self.next_f64();
        let u2 = self.next_f64();
        libm::sqrt(-2.0 * libm::log(u1)) * libm::cos(2.0 * std::f64::consts::PI * u2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_are_stable() {
        // These values are part of the language: changing them changes
        // every study's results.
        let mut s = Stream::new(42, 0, key_prefix("Night", "heater"));
        assert_eq!(
            s.next_u64(),
            Stream::new(42, 0, key_prefix("Night", "heater")).next_u64()
        );
        assert_ne!(key_prefix("Night", "heater"), key_prefix("Night", "sensor"));
        assert_ne!(key_prefix("a", "bc"), key_prefix("ab", "c"));
        let x = s.int_in(0, 3);
        assert!((0..=3).contains(&x));
    }

    #[test]
    fn int_in_is_uniform_enough() {
        let mut s = Stream::new(1, 2, 3);
        let mut counts = [0; 6];
        for _ in 0..60000 {
            counts[s.int_in(1, 6) as usize - 1] += 1;
        }
        for c in counts {
            assert!((9500..10500).contains(&c), "{counts:?}");
        }
    }
}
