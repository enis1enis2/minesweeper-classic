//! Bit-exact xorshift64 PRNG, 1:1 with `src/minesweeper.c xorshift()`,
//! `server/sim_engine.py`, and `ms/core/sim-engine.js`.

pub const ZERO_SEED_FALLBACK: u64 = 0x9e3779b97f4a7c15;

/// Normalise a raw seed to the u64 the board actually uses. A masked value of
/// zero would leave xorshift64 stuck at the all-zero fixed point forever, so it
/// is mapped onto `ZERO_SEED_FALLBACK` (identical constant across C, Python,
/// Node, Rust).
pub fn to_u64(seed: u64) -> u64 {
    if seed == 0 {
        ZERO_SEED_FALLBACK
    } else {
        seed
    }
}

#[derive(Clone, Debug)]
pub struct Rng64 {
    pub s: u64,
}

impl Rng64 {
    pub fn new(seed: u64) -> Self {
        Rng64 { s: to_u64(seed) }
    }

    /// Advance the generator once and return the next 64-bit value.
    ///
    /// Matches JS: `x ^= (x << 13n) & MASK64; x ^= x >> 7n; x ^= (x << 17n) & MASK64`.
    /// Rust u64 shifts wrap/truncate exactly like the BigInt masking.
    pub fn next(&mut self) -> u64 {
        let mut x = self.s;
        x ^= (x << 13) & u64::MAX;
        x ^= x >> 7;
        x ^= (x << 17) & u64::MAX;
        self.s = x;
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_seed_uses_fallback() {
        assert_eq!(to_u64(0), ZERO_SEED_FALLBACK);
        assert_eq!(to_u64(1), 1);
    }

    #[test]
    fn zero_seed_first_draw_is_nonzero() {
        let mut r = Rng64::new(0);
        assert_ne!(r.next(), 0);
    }

    #[test]
    fn known_stream() {
        // First four xorshift64 draws for seed 1 (verified against the C, Python
        // and Node ports with `node -e` against ms/core/sim-engine.js).
        let mut r = Rng64::new(1);
        let draws = [r.next(), r.next(), r.next(), r.next()];
        assert_eq!(
            draws,
            [
                1082269761,
                1152992998833853505,
                11177516664432764457,
                17678023832001937445
            ]
        );
    }

    #[test]
    fn seed0_draws_differ_from_seed1() {
        let mut a = Rng64::new(0);
        let mut b = Rng64::new(1);
        for _ in 0..8 {
            assert_ne!(a.next(), b.next());
        }
    }
}
