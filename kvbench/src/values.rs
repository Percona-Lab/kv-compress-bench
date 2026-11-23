//! Value generation with controllable compressibility.
//!
//! A shared pool of bytes is built once. The pool is a sequence of 16-byte
//! groups; in each group the first few characters are random (from a
//! 64-character alphabet) and the rest are fixed filler text, so that overall
//! `random_pct` percent of the characters are random. This is the same idea as
//! `--data-random-pct` in Percona-Lab/sysbench-compressible.
//!
//! The value for (key, version) is a short text header `<key>:<ver>:` followed
//! by a slice of the pool at a pseudo-random offset; offset and length are
//! derived from a hash of (key, version), so any value can be regenerated and
//! checked on read.

use crate::dist::{mix64, Rng, SizeDist};

const GROUP: usize = 16;
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const FILLER: &[u8; GROUP] = b"-kv-compress-pad";

pub struct ValueGen {
    pool: Vec<u8>,
    pub size: SizeDist,
    pub random_pct: f64,
    max_len: usize,
}

#[inline]
pub fn fmt_u64(mut v: u64, out: &mut [u8; 20]) -> &[u8] {
    let mut i = 20;
    loop {
        i -= 1;
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &out[i..]
}

#[inline]
fn fmt_hex(mut v: u64, out: &mut [u8; 16]) -> &[u8] {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut i = 16;
    loop {
        i -= 1;
        out[i] = H[(v & 15) as usize];
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    &out[i..]
}

impl ValueGen {
    pub fn new(size: SizeDist, random_pct: f64, pool_bytes: usize, seed: u64) -> Self {
        let max_len = size.max();
        let pool_len = pool_bytes.max(max_len * 4 + 4096);
        let mut pool = Vec::with_capacity(pool_len);
        let mut rng = Rng::new(seed ^ 0xC0FF_EE00);
        // Bresenham-style: carry the fractional number of random chars per group.
        let per_group = GROUP as f64 * random_pct / 100.0;
        let mut acc = 0.0f64;
        while pool.len() < pool_len {
            acc += per_group;
            let r = (acc.floor() as usize).min(GROUP);
            acc -= r as f64;
            for i in 0..GROUP {
                if i < r {
                    pool.push(ALPHABET[(rng.next_u64() & 63) as usize]);
                } else {
                    pool.push(FILLER[i]);
                }
            }
        }
        pool.truncate(pool_len);
        ValueGen { pool, size, random_pct, max_len }
    }

    #[inline]
    fn plan(&self, key: u64, ver: u64) -> (usize, usize) {
        let mut rng = Rng::new(mix64(key ^ mix64(ver.wrapping_add(0x51ED))));
        let len = self.size.sample(&mut rng);
        let off = rng.below((self.pool.len() - self.max_len) as u64) as usize;
        (len, off)
    }

    /// Length of the value for (key, ver).
    #[inline]
    pub fn len(&self, key: u64, ver: u64) -> usize {
        self.plan(key, ver).0
    }

    /// Append the value for (key, ver) to `out`; returns its length.
    #[inline]
    pub fn write(&self, key: u64, ver: u64, out: &mut Vec<u8>) -> usize {
        let (len, off) = self.plan(key, ver);
        let mut kb = [0u8; 20];
        let mut vb = [0u8; 16];
        let k = fmt_u64(key, &mut kb);
        let v = fmt_hex(ver, &mut vb);
        let hlen = k.len() + v.len() + 2;
        if len <= hlen {
            let mut h = Vec::with_capacity(hlen);
            h.extend_from_slice(k);
            h.push(b':');
            h.extend_from_slice(v);
            h.push(b':');
            out.extend_from_slice(&h[..len]);
        } else {
            out.extend_from_slice(k);
            out.push(b':');
            out.extend_from_slice(v);
            out.push(b':');
            out.extend_from_slice(&self.pool[off..off + len - hlen]);
        }
        len
    }

    /// Check a value read back for `key`. The version is taken from the
    /// header, then the whole value is regenerated and compared.
    pub fn verify(&self, key: u64, got: &[u8]) -> bool {
        // header: <key>:<hexver>:
        let mut kb = [0u8; 20];
        let k = fmt_u64(key, &mut kb);
        if got.len() < k.len() + 1 {
            // tiny value: only a prefix of the header is stored
            return k.starts_with(got) || got.starts_with(k);
        }
        if &got[..k.len()] != k || got[k.len()] != b':' {
            return false;
        }
        let rest = &got[k.len() + 1..];
        let end = rest.iter().position(|&c| c == b':').unwrap_or(rest.len());
        let mut ver = 0u64;
        for &c in &rest[..end] {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => return false,
            };
            ver = (ver << 4) | d as u64;
        }
        let mut exp = Vec::with_capacity(got.len());
        self.write(key, ver, &mut exp);
        exp == got
    }

    /// Estimate how well the values compress the way zram/zswap see them:
    /// values packed back-to-back into 4 KiB pages, each page compressed alone.
    pub fn measure_ratio(&self, nkeys: u64, pages: usize) -> serde_json::Value {
        self.measure_ratio_ext(nkeys, pages, &[], None)
    }

    /// Like measure_ratio, plus zstd ratios at extra levels (negative = fast levels) and an optional
    /// dump of the packed pages to a file (to test kernel compressors with exactly this data).
    pub fn measure_ratio_ext(&self, nkeys: u64, pages: usize, zstd_levels: &[i32], dump: Option<&str>) -> serde_json::Value {
        let mut rng = Rng::new(42);
        let mut buf = Vec::with_capacity(pages * 4096 + self.max_len);
        let mut total_len = 0u64;
        let mut count = 0u64;
        while buf.len() < pages * 4096 {
            let k = rng.below(nkeys.max(1));
            total_len += self.write(k, 0, &mut buf) as u64;
            count += 1;
        }
        let (mut raw, mut lz4, mut zstd1, mut zstd3) = (0usize, 0usize, 0usize, 0usize);
        for p in buf.chunks_exact(4096).take(pages) {
            raw += p.len();
            lz4 += lz4_flex::block::compress(p).len().min(4096);
            zstd1 += zstd::bulk::compress(p, 1).map(|c| c.len()).unwrap_or(4096).min(4096);
            zstd3 += zstd::bulk::compress(p, 3).map(|c| c.len()).unwrap_or(4096).min(4096);
        }
        let mut extra = serde_json::Map::new();
        for &lv in zstd_levels {
            let c: usize = buf.chunks_exact(4096).take(pages)
                .map(|p| zstd::bulk::compress(p, lv).map(|c| c.len()).unwrap_or(4096).min(4096)).sum();
            extra.insert(format!("zstd{}_ratio", lv), serde_json::json!(raw as f64 / c as f64));
        }
        if let Some(path) = dump {
            std::fs::write(path, &buf[..pages * 4096]).expect("write dump");
        }
        serde_json::json!({
            "zstd_levels": extra,
            "page_bytes": 4096,
            "pages": pages,
            "mean_value_len": total_len as f64 / count as f64,
            "lz4_ratio": raw as f64 / lz4 as f64,
            "zstd1_ratio": raw as f64 / zstd1 as f64,
            "zstd3_ratio": raw as f64 / zstd3 as f64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_verify() {
        let g = ValueGen::new(SizeDist::Uniform { min: 1, max: 3000 }, 10.0, 1 << 20, 1);
        for key in [0u64, 7, 123_456_789] {
            for ver in [0u64, 1, 0xABCDEF, (5u64 << 48) | 99] {
                let mut v = Vec::new();
                let n = g.write(key, ver, &mut v);
                assert_eq!(n, v.len());
                assert_eq!(n, g.len(key, ver));
                assert!(g.verify(key, &v), "key {key} ver {ver} len {n}");
                if v.len() > 30 {
                    let mut bad = v.clone();
                    let l = bad.len();
                    bad[l - 1] ^= 1;
                    assert!(!g.verify(key, &bad));
                }
            }
        }
    }

    #[test]
    fn random_share() {
        let g = ValueGen::new(SizeDist::Fixed(1000), 10.0, 1 << 20, 1);
        let filler = g.pool.chunks(GROUP).map(|c| c.iter().zip(FILLER).filter(|(a, b)| a == b).count()).sum::<usize>();
        let share = 1.0 - filler as f64 / g.pool.len() as f64;
        // random chars can coincide with the filler 1/64 of the time
        assert!((share - 0.10 * 63.0 / 64.0).abs() < 0.005, "{share}");
    }

    #[test]
    fn more_random_compresses_worse() {
        let r10 = ValueGen::new(SizeDist::Fixed(1000), 10.0, 1 << 22, 1).measure_ratio(1000, 64)["lz4_ratio"].as_f64().unwrap();
        let r50 = ValueGen::new(SizeDist::Fixed(1000), 50.0, 1 << 22, 1).measure_ratio(1000, 64)["lz4_ratio"].as_f64().unwrap();
        let r100 = ValueGen::new(SizeDist::Fixed(1000), 100.0, 1 << 22, 1).measure_ratio(1000, 64)["lz4_ratio"].as_f64().unwrap();
        assert!(r10 > r50 && r50 > r100, "{r10} {r50} {r100}");
        assert!(r100 < 1.1);
    }
}
