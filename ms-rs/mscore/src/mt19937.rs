//! MT19937 with a CPython `random.Random`-compatible API, ported from
//! `ms/core/mt19937.js` (which replicates CPython's `_randommodule.c` exactly:
//! int seeds go through `init_by_array` with little-endian 32-bit words, and
//! `getrandbits(k)` assembles little-endian words so `random()`, `choice()`
//! and `shuffle()` produce bit-identical streams).

const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908b0df;
const UPPER_MASK: u32 = 0x80000000;
const LOWER_MASK: u32 = 0x7fffffff;

const FLOAT_DIV: f64 = 1.0 / 9007199254740992.0; // 1 / 2**53

#[derive(Clone, Debug)]
pub struct Mt19937 {
    pub state: Vec<u32>,
    pub index: usize,
}

fn init_genrand(state: &mut [u32], s: u32) {
    state[0] = s;
    for i in 1..N {
        let prev = state[i - 1];
        let x = prev ^ (prev >> 30);
        state[i] = 1812433253u32.wrapping_mul(x).wrapping_add(i as u32);
    }
}

fn init_by_array(state: &mut [u32], key: &[u32]) {
    init_genrand(state, 19650218);
    let mut i = 1usize;
    let mut j = 0usize;
    let mut k = if N > key.len() { N } else { key.len() };
    while k > 0 {
        let prev = state[i - 1];
        let x = prev ^ (prev >> 30);
        let mut v = state[i] ^ 1664525u32.wrapping_mul(x);
        v = v.wrapping_add(key[j]).wrapping_add(j as u32);
        state[i] = v;
        i += 1;
        j += 1;
        if i >= N {
            state[0] = state[N - 1];
            i = 1;
        }
        if j >= key.len() {
            j = 0;
        }
        k -= 1;
    }
    k = N - 1;
    while k > 0 {
        let prev = state[i - 1];
        let x = prev ^ (prev >> 30);
        let mut v = state[i] ^ 1566083941u32.wrapping_mul(x);
        v = v.wrapping_sub(i as u32);
        state[i] = v;
        i += 1;
        if i >= N {
            state[0] = state[N - 1];
            i = 1;
        }
        k -= 1;
    }
    state[0] = 0x80000000;
}

impl Mt19937 {
    pub fn new() -> Self {
        let mut m = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        m.seed_u64(5489);
        m
    }

    pub fn from_state(state: &[u32], index: usize) -> Self {
        let mut s = state.to_vec();
        s.resize(N, 0);
        Mt19937 { state: s, index }
    }

    pub fn snapshot(&self) -> (Vec<u32>, usize) {
        (self.state.clone(), self.index)
    }

    pub fn restore(&mut self, state: &[u32], index: usize) {
        self.state = state.to_vec();
        self.state.resize(N, 0);
        self.index = index;
    }

    /// CPython int-seed path: key = little-endian 32-bit words of |seed|,
    /// keyused = ceil(bit_length(|seed|)/32) (min 1); init_by_array(key).
    /// `seed` is a non-negative integer (abs applied by caller for negatives).
    pub fn seed_u64(&mut self, seed: u64) {
        let bits = if seed == 0 { 0 } else { 64 - seed.leading_zeros() as usize };
        let keyused = if bits == 0 { 1 } else { (bits + 31) / 32 };
        let mut key = Vec::with_capacity(keyused);
        for i in 0..keyused {
            key.push(((seed >> (32 * i)) & 0xffff_ffff) as u32);
        }
        init_by_array(&mut self.state, &key);
        self.index = N;
    }

    /// CPython null-seed path (non-deterministic): key = 624 urandom words.
    /// The caller supplies the words; this applies CPython init_by_array.
    pub fn seed_from_words(&mut self, key: &[u32]) {
        init_by_array(&mut self.state, key);
        self.index = N;
    }

    pub fn genrand_uint32(&mut self) -> u32 {
        if self.index >= N {
            for i in 0..N {
                let y = (self.state[i] & UPPER_MASK) | (self.state[(i + 1) % N] & LOWER_MASK);
                self.state[i] = self.state[(i + M) % N]
                    ^ (y >> 1)
                    ^ if y & 1 == 1 { MATRIX_A } else { 0 };
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c5680;
        y ^= (y << 15) & 0xefc60000;
        y ^= y >> 18;
        y
    }

    /// CPython random(): 53-bit double in [0, 1).
    pub fn random(&mut self) -> f64 {
        let a = (self.genrand_uint32() >> 5) as f64;
        let b = (self.genrand_uint32() >> 6) as f64;
        (a * 67108864.0 + b) * FLOAT_DIV
    }

    /// CPython getrandbits(k): returns u64 for k in 1..=64 (assembled from
    /// little-endian words like CPython).
    pub fn getrandbits(&mut self, k: u32) -> u64 {
        assert!(k >= 1 && k <= 64, "getrandbits k must be 1..=64");
        if k <= 32 {
            return (self.genrand_uint32() >> (32 - k)) as u64;
        }
        let words = (k + 31) / 32;
        let mut result: u64 = 0;
        let mut bits = k;
        for i in 0..words {
            let mut r = self.genrand_uint32();
            if bits < 32 {
                r >>= 32 - bits;
            }
            result |= (r as u64) << (32 * i);
            bits = bits.saturating_sub(32);
        }
        result
    }

    /// CPython _randbelow_with_getrandbits: u64 in [0, n).
    pub fn _randbelow(&mut self, n: u64) -> u64 {
        assert!(n > 0, "_randbelow requires n > 0");
        let k = 64 - n.leading_zeros();
        loop {
            let r = self.getrandbits(k);
            if r < n {
                return r;
            }
        }
    }

    pub fn choice<T: Clone>(&mut self, seq: &[T]) -> T {
        assert!(!seq.is_empty(), "Cannot choose from an empty sequence");
        let i = self._randbelow(seq.len() as u64) as usize;
        seq[i].clone()
    }

    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        let n = (b - a + 1) as u64;
        a + self._randbelow(n) as i64
    }

    /// `Random.randrange(a, b)` — u64 range, used by the sim server producer
    /// and batch RNG (`randrange(0n, 1n << 63n)`).
    pub fn randrange(&mut self, a: u64, b: u64) -> u64 {
        a + self._randbelow(b - a)
    }

    pub fn shuffle<T>(&mut self, seq: &mut [T]) {
        for i in (1..seq.len()).rev() {
            let j = self._randbelow(i as u64 + 1) as usize;
            seq.swap(i, j);
        }
    }
}

impl Default for Mt19937 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_seed_5489() {
        // Default seed is 5489; first getrandbits(32) matches CPython.
        let mut c = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        c.seed_u64(5489);
        assert_eq!(c.getrandbits(32), 3382763572);
    }

    #[test]
    fn golden_seed_4242_random() {
        let mut c = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        c.seed_u64(4242);
        let v = c.random();
        assert!((v - 0.8624508153567833).abs() < 1e-15);
    }

    #[test]
    fn large_seed_2p40_plus_7() {
        let mut c = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        c.seed_u64((1u64 << 40) + 7);
        assert_eq!(c.getrandbits(32), 2635837658);
    }

    #[test]
    fn golden_rng_stream_seed1() {
        // golden-rng.json seed "1": 10 random() floats.
        let mut c = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        c.seed_u64(1);
        let expected = [
            0.13436424411240122,
            0.8474337369372327,
            0.763774618976614,
            0.2550690257394217,
            0.49543508709194095,
            0.4494910647887381,
            0.651592972722763,
            0.7887233511355132,
            0.0938595867742349,
            0.02834747652200631,
        ];
        for e in expected {
            assert!((c.random() - e).abs() < 1e-15, "random mismatch");
        }
    }

    #[test]
    fn getrandbits64_matches_golden() {
        // golden-rng.json seed "42": first getrandbits(64) values.
        let mut c = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        c.seed_u64(42);
        let expected = [
            "2053695854357871005",
            "13679192365072849617",
            "4517457392071889495",
            "2574020394472462046",
            "1890702223848595625",
            "13662908291426823533",
            "10060236952204337488",
            "10892664235628797826",
            "586287033698423193",
            "1728372192399379054",
        ];
        for e in expected {
            assert_eq!(c.getrandbits(64).to_string(), *e);
        }
    }

    #[test]
    fn shuffle_matches_python() {
        let mut c = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        c.seed_u64(7);
        let mut arr = [0, 1, 2, 3, 4];
        c.shuffle(&mut arr);
        // Verified against ms/core/mt19937.js Random(7).shuffle() -> [4,0,3,1,2].
        assert_eq!(arr, [4, 0, 3, 1, 2]);
    }

    #[test]
    fn choice_matches_python() {
        let mut c = Mt19937 {
            state: vec![0; N],
            index: N,
        };
        c.seed_u64(7);
        // Verified against ms/core/mt19937.js Random(7).choice() -> "b".
        assert_eq!(c.choice(&["a", "b", "c"]), "b");
    }
}
