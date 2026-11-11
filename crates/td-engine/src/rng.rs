//! The one PRNG the engine draws from: SplitMix64 (Steele, Lea and Flood, 2014).
//!
//! It is small, fast and well studied, has a 64-bit state, and uses only integer arithmetic, so a
//! seed gives the same stream on every platform. Its floats come from the top 53 bits of a draw,
//! an exact conversion. Everything seeded in the engine (sweep samples, the bootstrap, bar
//! permutations) draws from it, and every draw happens on one thread in a fixed order, so results
//! do not depend on the platform or the number of threads.

/// The golden-ratio increment SplitMix64 adds to its state on each draw.
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

pub(crate) struct SplitMix64(pub(crate) u64);

impl SplitMix64 {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`, by rejecting draws from the incomplete last block. `n` must be at
    /// least 1.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        let limit = u64::MAX - u64::MAX % n;
        loop {
            let x = self.next();
            if x < limit {
                return x % n;
            }
        }
    }

    /// Uniform in `[0, 1)`: the top 53 bits of a draw over 2^53, exact in an `f64`.
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

/// The seed of stream `k` derived from `seed`: SplitMix64's own output for state
/// `seed + k * GAMMA`, so it is the (k+1)-th draw of `SplitMix64(seed)`. Streams started from two
/// derived seeds are not shifted copies of each other, as `seed + k` would make them.
pub(crate) fn derive_seed(seed: u64, k: u64) -> u64 {
    SplitMix64(seed.wrapping_add(k.wrapping_mul(GAMMA))).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_matches_the_published_reference_values() {
        // The first outputs for seed 1234567 from the reference implementation
        // (https://prng.di.unimi.it/splitmix64.c), as quoted in the Rust `rand_xoshiro` tests.
        let mut r = SplitMix64(1_234_567);
        let expected = [
            6_457_827_717_110_365_317,
            3_203_168_211_198_807_973,
            9_817_491_932_198_370_423,
            4_593_380_528_125_082_431,
            16_408_922_859_458_223_821,
        ];
        for e in expected {
            assert_eq!(r.next(), e);
        }
    }

    #[test]
    fn derived_seeds_are_the_draws_of_the_parent_stream() {
        let mut r = SplitMix64(42);
        for k in 0..5 {
            assert_eq!(derive_seed(42, k), r.next());
        }
    }

    #[test]
    fn unit_draws_stay_in_the_half_open_interval() {
        let mut r = SplitMix64(7);
        for _ in 0..10_000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u), "{u}");
        }
        // The largest possible draw is just below 1.
        assert!(((u64::MAX >> 11) as f64 * (1.0 / (1u64 << 53) as f64)) < 1.0);
    }
}
