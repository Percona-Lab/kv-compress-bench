//! Random number generation and the key / value-size distributions.
//!
//! Everything is implemented here (no `rand` crate) so the hot path is a few
//! arithmetic instructions and the sequences are stable across crate versions.

/// splitmix64: used for seeding and for hashing (key, version) pairs.
#[inline]
pub fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// xoshiro256++
#[derive(Clone)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut s = [0u64; 4];
        for v in s.iter_mut() {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            *v = mix64(x);
        }
        Rng { s }
    }
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let r = (self.s[0].wrapping_add(self.s[3])).rotate_left(23).wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        r
    }
    /// Uniform in [0, 1).
    #[inline]
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
    /// Uniform in [0, n) without modulo bias worth caring about (Lemire).
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }
    /// Standard normal (Box-Muller, one value per call; simple and fast enough).
    #[inline]
    pub fn normal(&mut self) -> f64 {
        let u1 = 1.0 - self.f64(); // (0, 1]
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
    /// Exponential with mean 1.
    #[inline]
    pub fn exp1(&mut self) -> f64 {
        -(1.0 - self.f64()).ln()
    }
}

// ---------------------------------------------------------------------------
// Key distributions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum KeyDist {
    Uniform,
    /// Normal around `center` (fraction of the key space) with `stddev`
    /// (fraction of the key space). Values outside the key space are redrawn.
    Normal { center: f64, stddev: f64 },
    /// YCSB-style Zipfian, theta in (0, 1).
    Zipfian { theta: f64, alpha: f64, zetan: f64, eta: f64, half_pow_theta: f64 },
    /// `hot_ops` of the operations go to `hot_keys` (fraction) of the key
    /// space starting at `hot_start` (fraction); the rest go to the cold keys.
    Hotspot { hot_start: f64, hot_keys: f64, hot_ops: f64 },
}

/// Bijective permutation of [0, n): x -> (a*x + b) mod n with gcd(a, n) = 1.
/// Used to scatter hot keys over the key space ("scrambled" distributions).
#[derive(Clone, Debug)]
pub struct Scramble {
    a: u64,
    b: u64,
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

impl Scramble {
    pub fn new(n: u64) -> Self {
        if n <= 1 {
            return Scramble { a: 1, b: 0 };
        }
        let mut a = (0x9E37_79B9_7F4A_7C15u64 % n).max(1);
        while gcd(a, n) != 1 {
            a = (a + 1) % n;
            if a == 0 {
                a = 1;
            }
        }
        Scramble { a, b: 0x5851_F42D_4C95_7F2D % n }
    }
    #[inline]
    pub fn apply(&self, x: u64, n: u64) -> u64 {
        ((self.a as u128 * x as u128 + self.b as u128) % n as u128) as u64
    }
}

/// Generalized harmonic number H(n, theta) = sum_{i=1..n} 1/i^theta.
/// Exact for the first 1M terms, Euler-Maclaurin for the tail (error << 1e-9
/// relative), so it is instant even for billions of keys.
pub fn zeta(n: u64, theta: f64) -> f64 {
    let m = n.min(1_000_000);
    let mut s = 0.0f64;
    for i in 1..=m {
        s += 1.0 / (i as f64).powf(theta);
    }
    if n > m {
        let f = |x: f64| x.powf(1.0 - theta) / (1.0 - theta);
        // sum_{i=m+1..n} i^-t ~ int_{m}^{n} x^-t dx + (n^-t - m^-t)/2 - t/12 (n^-t-1 - m^-t-1)
        let (a, b) = (m as f64, n as f64);
        s += f(b) - f(a) + (b.powf(-theta) - a.powf(-theta)) / 2.0
            - theta / 12.0 * (b.powf(-theta - 1.0) - a.powf(-theta - 1.0));
    }
    s
}

impl KeyDist {
    pub fn zipfian(n: u64, theta: f64) -> Self {
        assert!(theta > 0.0 && theta < 1.0, "zipfian theta must be in (0,1)");
        let zetan = zeta(n, theta);
        let zeta2 = zeta(2.min(n), theta);
        let alpha = 1.0 / (1.0 - theta);
        let eta = (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan);
        KeyDist::Zipfian { theta, alpha, zetan, eta, half_pow_theta: 1.0 + 0.5f64.powf(theta) }
    }

    /// Draw a key rank in [0, n) (before optional scrambling).
    #[inline]
    pub fn sample(&self, rng: &mut Rng, n: u64) -> u64 {
        match *self {
            KeyDist::Uniform => rng.below(n),
            KeyDist::Normal { center, stddev } => {
                let nf = n as f64;
                for _ in 0..64 {
                    let x = (center + stddev * rng.normal()) * nf;
                    if x >= 0.0 && x < nf {
                        return (x as u64).min(n - 1);
                    }
                }
                ((center * nf) as u64).min(n - 1)
            }
            KeyDist::Zipfian { theta, alpha, zetan, eta, half_pow_theta } => {
                let u = rng.f64();
                let uz = u * zetan;
                if uz < 1.0 {
                    return 0;
                }
                if uz < half_pow_theta {
                    return 1.min(n - 1);
                }
                let _ = theta;
                let r = (n as f64 * (eta * u - eta + 1.0).powf(alpha)) as u64;
                r.min(n - 1)
            }
            KeyDist::Hotspot { hot_start, hot_keys, hot_ops } => {
                let hot_n = ((hot_keys * n as f64) as u64).clamp(1, n);
                let start = ((hot_start * n as f64) as u64).min(n - hot_n);
                if rng.f64() < hot_ops || hot_n == n {
                    start + rng.below(hot_n)
                } else {
                    // cold keys: [0, start) U [start+hot_n, n)
                    let c = rng.below(n - hot_n);
                    if c < start { c } else { c + hot_n }
                }
            }
        }
    }

    pub fn describe(&self) -> serde_json::Value {
        match *self {
            KeyDist::Uniform => serde_json::json!({"type": "uniform"}),
            KeyDist::Normal { center, stddev } => {
                serde_json::json!({"type": "normal", "center": center, "stddev": stddev})
            }
            KeyDist::Zipfian { theta, .. } => serde_json::json!({"type": "zipfian", "theta": theta}),
            KeyDist::Hotspot { hot_start, hot_keys, hot_ops } => serde_json::json!({
                "type": "hotspot", "hot_start": hot_start, "hot_keys": hot_keys, "hot_ops": hot_ops}),
        }
    }
}

// ---------------------------------------------------------------------------
// Value-size distributions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum SizeDist {
    Fixed(usize),
    Uniform { min: usize, max: usize },
    /// Normal with mean/stddev, clamped to [min, max].
    Normal { mean: f64, stddev: f64, min: usize, max: usize },
    /// Log-normal with the given median and sigma (of the underlying normal),
    /// clamped to [min, max].
    LogNormal { median: f64, sigma: f64, min: usize, max: usize },
}

impl SizeDist {
    #[inline]
    pub fn sample(&self, rng: &mut Rng) -> usize {
        match *self {
            SizeDist::Fixed(n) => n,
            SizeDist::Uniform { min, max } => min + rng.below((max - min + 1) as u64) as usize,
            SizeDist::Normal { mean, stddev, min, max } => {
                ((mean + stddev * rng.normal()).round().max(0.0) as usize).clamp(min, max)
            }
            SizeDist::LogNormal { median, sigma, min, max } => {
                ((median * (sigma * rng.normal()).exp()).round() as usize).clamp(min, max)
            }
        }
    }
    pub fn max(&self) -> usize {
        match *self {
            SizeDist::Fixed(n) => n,
            SizeDist::Uniform { max, .. } | SizeDist::Normal { max, .. } | SizeDist::LogNormal { max, .. } => max,
        }
    }
    pub fn describe(&self) -> serde_json::Value {
        match *self {
            SizeDist::Fixed(n) => serde_json::json!({"type": "fixed", "size": n}),
            SizeDist::Uniform { min, max } => serde_json::json!({"type": "uniform", "min": min, "max": max}),
            SizeDist::Normal { mean, stddev, min, max } => serde_json::json!({
                "type": "normal", "mean": mean, "stddev": stddev, "min": min, "max": max}),
            SizeDist::LogNormal { median, sigma, min, max } => serde_json::json!({
                "type": "lognormal", "median": median, "sigma": sigma, "min": min, "max": max}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeta_tail_matches_exact() {
        let n = 3_000_000u64;
        let exact: f64 = (1..=n).map(|i| 1.0 / (i as f64).powf(0.99)).sum();
        let approx = zeta(n, 0.99);
        assert!(((exact - approx) / exact).abs() < 1e-9, "{exact} vs {approx}");
    }

    #[test]
    fn normal_concentration() {
        // stddev 0.05: about 68% of draws within +-5% of the key space around the center
        let d = KeyDist::Normal { center: 0.5, stddev: 0.05 };
        let mut r = Rng::new(1);
        let n = 1_000_000;
        let inside = (0..200_000).filter(|_| {
            let k = d.sample(&mut r, n);
            (450_000..550_000).contains(&k)
        }).count();
        let f = inside as f64 / 200_000.0;
        assert!((f - 0.6827).abs() < 0.01, "{f}");
    }

    #[test]
    fn zipf_top_key_share() {
        // theta 0.99, n=1000: P(rank0) = 1/zeta(1000) ~ 0.134
        let n = 1000;
        let d = KeyDist::zipfian(n, 0.99);
        let mut r = Rng::new(7);
        let draws = 400_000;
        let zero = (0..draws).filter(|_| d.sample(&mut r, n) == 0).count() as f64 / draws as f64;
        let expect = 1.0 / zeta(n, 0.99);
        assert!((zero - expect).abs() < 0.01, "{zero} vs {expect}");
    }

    #[test]
    fn hotspot_share() {
        let d = KeyDist::Hotspot { hot_start: 0.0, hot_keys: 0.1, hot_ops: 0.9 };
        let mut r = Rng::new(3);
        let n = 100_000;
        let hot = (0..200_000).filter(|_| d.sample(&mut r, n) < 10_000).count() as f64 / 200_000.0;
        assert!((hot - 0.9).abs() < 0.01, "{hot}");
    }

    #[test]
    fn scramble_is_bijective() {
        for n in [1u64, 2, 10, 97, 1000, 4096] {
            let s = Scramble::new(n);
            let mut seen = vec![false; n as usize];
            for x in 0..n {
                let y = s.apply(x, n) as usize;
                assert!(!seen[y]);
                seen[y] = true;
            }
        }
    }

    #[test]
    fn size_bounds() {
        let mut r = Rng::new(5);
        let d = SizeDist::LogNormal { median: 1000.0, sigma: 1.0, min: 32, max: 8192 };
        for _ in 0..100_000 {
            let s = d.sample(&mut r);
            assert!((32..=8192).contains(&s));
        }
    }
}
