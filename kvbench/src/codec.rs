//! Client-side value compression (--client-compress): values are compressed before SET and decompressed after
//! GET, the way an application would do it, so it can be compared with compression in the kernel or the store.
//! Each value is compressed on its own. Formats: lz4 block with the original size prepended (lz4_flex),
//! zstd frame (content size included).

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Lz4,
    Zstd(i32),
}

pub fn parse(s: &str) -> Result<Option<Kind>, String> {
    match s {
        "" | "none" | "no" => Ok(None),
        "lz4" => Ok(Some(Kind::Lz4)),
        "zstd" => Ok(Some(Kind::Zstd(3))),
        _ => match s.strip_prefix("zstd:") {
            Some(l) => l.parse::<i32>().map(|l| Some(Kind::Zstd(l))).map_err(|e| format!("client-compress {s}: {e}")),
            None => Err(format!("unknown client-compress {s} (none, lz4, zstd, zstd:LEVEL)")),
        },
    }
}

pub fn describe(k: Option<Kind>) -> String {
    match k {
        None => "none".into(),
        Some(Kind::Lz4) => "lz4".into(),
        Some(Kind::Zstd(l)) => format!("zstd:{l}"),
    }
}

/// Per-thread compressor state (reused contexts and buffers).
pub struct Codec {
    kind: Kind,
    zc: Option<zstd::bulk::Compressor<'static>>,
    zd: Option<zstd::bulk::Decompressor<'static>>,
    pub raw: Vec<u8>,
    pub out: Vec<u8>,
    max_len: usize,
    /// bytes before / after compression, for SETs
    pub raw_bytes: u64,
    pub comp_bytes: u64,
}

impl Codec {
    pub fn new(kind: Kind, max_len: usize) -> Self {
        let (zc, zd) = match kind {
            Kind::Zstd(l) => (Some(zstd::bulk::Compressor::new(l).expect("zstd level")), Some(zstd::bulk::Decompressor::new().unwrap())),
            Kind::Lz4 => (None, None),
        };
        Codec { kind, zc, zd, raw: Vec::with_capacity(max_len + 64), out: Vec::with_capacity(max_len * 2 + 64), max_len, raw_bytes: 0, comp_bytes: 0 }
    }

    /// Compress self.raw into self.out.
    pub fn compress(&mut self) {
        self.out.clear();
        match self.kind {
            Kind::Lz4 => self.out.extend_from_slice(&lz4_flex::block::compress_prepend_size(&self.raw)),
            Kind::Zstd(_) => {
                let c = self.zc.as_mut().unwrap().compress(&self.raw).expect("zstd compress");
                self.out.extend_from_slice(&c);
            }
        }
        self.raw_bytes += self.raw.len() as u64;
        self.comp_bytes += self.out.len() as u64;
    }

    /// Decompress `data` into self.raw. Returns false if the data is not valid.
    pub fn decompress(&mut self, data: &[u8]) -> bool {
        self.raw.clear();
        match self.kind {
            Kind::Lz4 => match lz4_flex::block::decompress_size_prepended(data) {
                Ok(v) => { self.raw.extend_from_slice(&v); true }
                Err(_) => false,
            },
            Kind::Zstd(_) => match self.zd.as_mut().unwrap().decompress(data, self.max_len + 16) {
                Ok(v) => { self.raw.extend_from_slice(&v); true }
                Err(_) => false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        for k in [Kind::Lz4, Kind::Zstd(3), Kind::Zstd(-1)] {
            let mut c = Codec::new(k, 4096);
            for len in [0usize, 1, 17, 1000, 4096] {
                c.raw = (0..len).map(|i| b"abcabcabcXYZ"[i % 12]).collect();
                let orig = c.raw.clone();
                c.compress();
                let comp = c.out.clone();
                assert!(c.decompress(&comp), "{k:?} {len}");
                assert_eq!(c.raw, orig);
            }
        }
        assert_eq!(parse("zstd:-1").unwrap(), Some(Kind::Zstd(-1)));
        assert_eq!(parse("none").unwrap(), None);
    }
}
