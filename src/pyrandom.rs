//! CPython's `random.Random`, bit for bit.
//!
//! The identity draws (fonts, voices, media devices, GPU) are seeded by the
//! identity, and the Python and TypeScript launchers draw them with CPython's
//! Mersenne Twister so that a pinned identity presents the same device from
//! either language. This is the same generator, with the methods those draws
//! use: `random()`, `getrandbits()`, `randrange()`, `randint()`, `choice()`,
//! `choices()` and `sample()`.

const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908_b0df;
const UPPER_MASK: u32 = 0x8000_0000;
const LOWER_MASK: u32 = 0x7fff_ffff;

#[derive(Clone)]
pub struct PyRandom {
    mt: [u32; N],
    index: usize,
}

impl std::fmt::Debug for PyRandom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyRandom").field("index", &self.index).finish()
    }
}

impl PyRandom {
    /// `random.Random(seed)` for a non-negative integer seed.
    pub fn seeded(seed: u128) -> Self {
        let mut key: Vec<u32> = Vec::new();
        let mut s = seed;
        while s > 0 {
            key.push((s & 0xffff_ffff) as u32);
            s >>= 32;
        }
        if key.is_empty() {
            key.push(0);
        }
        Self::from_key(&key)
    }

    /// `random.Random()`: seeded from the operating system.
    pub fn from_entropy() -> Self {
        let mut bytes = [0u8; N * 4];
        getrandom::getrandom(&mut bytes).expect("os entropy");
        let key: Vec<u32> = bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        Self::from_key(&key)
    }

    /// `random.Random(seed)` when `seed` is `None`: a fresh generator.
    pub fn new(seed: Option<u128>) -> Self {
        match seed {
            Some(s) => Self::seeded(s),
            None => Self::from_entropy(),
        }
    }

    fn init_genrand(&mut self, s: u32) {
        self.mt[0] = s;
        for i in 1..N {
            let prev = self.mt[i - 1];
            self.mt[i] = 1_812_433_253u32.wrapping_mul(prev ^ (prev >> 30)).wrapping_add(i as u32);
        }
        self.index = N;
    }

    fn from_key(key: &[u32]) -> Self {
        let mut rng = PyRandom { mt: [0; N], index: N };
        rng.init_genrand(19_650_218);
        let mut i = 1usize;
        let mut j = 0usize;
        let klen = key.len();
        let mut k = if N > klen { N } else { klen };
        while k > 0 {
            let prev = rng.mt[i - 1];
            rng.mt[i] = (rng.mt[i] ^ ((prev ^ (prev >> 30)).wrapping_mul(1_664_525)))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                rng.mt[0] = rng.mt[N - 1];
                i = 1;
            }
            if j >= klen {
                j = 0;
            }
            k -= 1;
        }
        let mut k = N - 1;
        while k > 0 {
            let prev = rng.mt[i - 1];
            rng.mt[i] = (rng.mt[i] ^ ((prev ^ (prev >> 30)).wrapping_mul(1_566_083_941))).wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                rng.mt[0] = rng.mt[N - 1];
                i = 1;
            }
            k -= 1;
        }
        rng.mt[0] = 0x8000_0000;
        rng.index = N;
        rng
    }

    fn genrand_u32(&mut self) -> u32 {
        if self.index >= N {
            let mt = &mut self.mt;
            for kk in 0..N - M {
                let y = (mt[kk] & UPPER_MASK) | (mt[kk + 1] & LOWER_MASK);
                mt[kk] = mt[kk + M] ^ (y >> 1) ^ if y & 1 == 1 { MATRIX_A } else { 0 };
            }
            for kk in N - M..N - 1 {
                let y = (mt[kk] & UPPER_MASK) | (mt[kk + 1] & LOWER_MASK);
                mt[kk] = mt[kk + M - N] ^ (y >> 1) ^ if y & 1 == 1 { MATRIX_A } else { 0 };
            }
            let y = (mt[N - 1] & UPPER_MASK) | (mt[0] & LOWER_MASK);
            mt[N - 1] = mt[M - 1] ^ (y >> 1) ^ if y & 1 == 1 { MATRIX_A } else { 0 };
            self.index = 0;
        }
        let mut y = self.mt[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// `random.random()`: a float in [0, 1) with 53 bits of entropy.
    pub fn random(&mut self) -> f64 {
        let a = (self.genrand_u32() >> 5) as f64;
        let b = (self.genrand_u32() >> 6) as f64;
        (a * 67_108_864.0 + b) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `random.getrandbits(k)` for k <= 64.
    pub fn getrandbits(&mut self, k: u32) -> u64 {
        if k == 0 {
            return 0;
        }
        if k <= 32 {
            return (self.genrand_u32() >> (32 - k)) as u64;
        }
        assert!(k <= 64, "getrandbits: at most 64 bits");
        let lo = self.genrand_u32() as u64;
        let remaining = k - 32;
        let hi = (self.genrand_u32() >> (32 - remaining)) as u64;
        lo | (hi << 32)
    }

    /// `random._randbelow(n)`: an integer in [0, n).
    pub fn randbelow(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let k = 64 - n.leading_zeros();
        loop {
            let r = self.getrandbits(k);
            if r < n {
                return r;
            }
        }
    }

    /// `random.randrange(start, stop)`.
    pub fn randrange(&mut self, start: i64, stop: i64) -> i64 {
        let width = stop - start;
        assert!(width > 0, "empty range for randrange()");
        start + self.randbelow(width as u64) as i64
    }

    /// `random.randint(a, b)`: an integer in [a, b].
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        self.randrange(a, b + 1)
    }

    /// `random.choice(seq)`.
    pub fn choice<'a, T>(&mut self, seq: &'a [T]) -> &'a T {
        assert!(!seq.is_empty(), "choice() from an empty sequence");
        &seq[self.randbelow(seq.len() as u64) as usize]
    }

    /// `random.choices(population, weights)[0]`: the index drawn.
    pub fn choices_index(&mut self, weights: &[f64]) -> usize {
        let n = weights.len();
        assert!(n > 0, "choices() from an empty population");
        let mut cum = Vec::with_capacity(n);
        let mut acc = 0.0f64;
        for w in weights {
            acc += w;
            cum.push(acc);
        }
        let total = cum[n - 1] + 0.0;
        let hi = n - 1;
        let x = self.random() * total;
        // bisect.bisect_right(cum, x, 0, hi)
        let (mut lo, mut hi) = (0usize, hi);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if x < cum[mid] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    /// `random.sample(population, k)`: the indices drawn, in order.
    pub fn sample_indices(&mut self, n: usize, k: usize) -> Vec<usize> {
        assert!(k <= n, "sample larger than population");
        let mut result = Vec::with_capacity(k);
        let mut setsize: usize = 21;
        if k > 5 {
            let exp = ((k as f64 * 3.0).ln() / 4f64.ln()).ceil() as u32;
            setsize += 4usize.pow(exp);
        }
        if n <= setsize {
            let mut pool: Vec<usize> = (0..n).collect();
            for i in 0..k {
                let j = self.randbelow((n - i) as u64) as usize;
                result.push(pool[j]);
                pool[j] = pool[n - i - 1];
            }
        } else {
            let mut selected = std::collections::HashSet::new();
            for _ in 0..k {
                let mut j = self.randbelow(n as u64) as usize;
                while selected.contains(&j) {
                    j = self.randbelow(n as u64) as usize;
                }
                selected.insert(j);
                result.push(j);
            }
        }
        result
    }

    /// `random.sample(population, k)` over a slice.
    pub fn sample<T: Clone>(&mut self, population: &[T], k: usize) -> Vec<T> {
        self.sample_indices(population.len(), k).into_iter().map(|i| population[i].clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_cpython_random() {
        // python3 -c "import random; r=random.Random(12345); print([r.random() for _ in range(3)])"
        let mut r = PyRandom::seeded(12345);
        let got: Vec<f64> = (0..3).map(|_| r.random()).collect();
        assert_eq!(got, vec![0.41661987254534116, 0.010169169457068361, 0.8252065092537432]);
    }

    #[test]
    fn matches_cpython_randrange_and_sample() {
        // python3 -c "import random; r=random.Random(7); print(r.randrange(100), r.randint(1,6), r.sample(range(10),3))"
        let mut r = PyRandom::seeded(7);
        let a = r.randrange(0, 100);
        let b = r.randint(1, 6);
        let s = r.sample_indices(10, 3);
        assert_eq!((a, b), (41, 2));
        assert_eq!(s, vec![6, 0, 1]);
    }

    #[test]
    fn large_seed() {
        // random.Random(2**40 + 5).random()
        let mut r = PyRandom::seeded((1u128 << 40) + 5);
        assert_eq!(r.random(), 0.5043802970418443);
    }
}
