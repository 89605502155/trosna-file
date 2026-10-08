//! General-purpose codecs (SPEC §7), the second stage of compression.
//! Both are pure-Rust implementations without `unsafe` code in safe mode.

use std::fmt;
use std::io::Read;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Upper bound of the LZ4 block expansion ratio (each extra length byte adds 255).
const LZ4_MAX_RATIO: usize = 255;

/// General-purpose compression applied to encoded segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Codec {
    /// No compression.
    None,
    /// LZ4 block compression (fast).
    Lz4,
    /// Zstandard (better ratio). The pure-Rust encoder currently implements
    /// the "fastest" level, roughly equivalent to zstd level 1.
    #[default]
    Zstd,
}

impl Codec {
    pub(crate) fn code(self) -> u8 {
        match self {
            Codec::None => 0,
            Codec::Lz4 => 1,
            Codec::Zstd => 2,
        }
    }

    pub(crate) fn from_code(code: u8) -> Result<Self> {
        Ok(match code {
            0 => Codec::None,
            1 => Codec::Lz4,
            2 => Codec::Zstd,
            _ => return Err(Error::Unsupported(format!("codec code {code}"))),
        })
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Codec::None => "none",
            Codec::Lz4 => "lz4",
            Codec::Zstd => "zstd",
        }
    }

    /// Compresses `raw`. Returns `None` if compression would not make it smaller.
    pub(crate) fn compress(self, raw: &[u8]) -> Option<Vec<u8>> {
        let out = match self {
            Codec::None => return None,
            Codec::Lz4 => lz4_flex::block::compress(raw),
            Codec::Zstd => {
                ruzstd::encoding::compress_to_vec(raw, ruzstd::encoding::CompressionLevel::Fastest)
            }
        };
        (out.len() < raw.len()).then_some(out)
    }

    /// Decompresses `stored`, which must expand to exactly `raw_len` bytes.
    pub(crate) fn decompress(self, stored: &[u8], raw_len: usize) -> Result<Vec<u8>> {
        let out = match self {
            Codec::None => stored.to_vec(),
            Codec::Lz4 => {
                // An LZ4 block cannot expand more than 255:1, so a larger claimed
                // size is corruption; refusing it avoids reserving that memory.
                if raw_len
                    > stored
                        .len()
                        .saturating_mul(LZ4_MAX_RATIO)
                        .saturating_add(16)
                {
                    return Err(Error::corrupted(format!(
                        "LZ4 block of {} bytes cannot expand to {raw_len} bytes",
                        stored.len()
                    )));
                }
                lz4_flex::block::decompress(stored, raw_len)
                    .map_err(|e| Error::corrupted(format!("LZ4: {e}")))?
            }
            Codec::Zstd => {
                let mut source = stored;
                let decoder = ruzstd::decoding::StreamingDecoder::new(&mut source)
                    .map_err(|e| Error::corrupted(format!("zstd: {e}")))?;
                // zstd frames can legitimately expand far more than LZ4 blocks, so
                // memory is not reserved from the claimed size: the output grows
                // only as data is really decompressed, and never beyond `raw_len`.
                let mut out = Vec::with_capacity(raw_len.min(stored.len().saturating_mul(4)));
                decoder
                    .take(raw_len as u64 + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| Error::corrupted(format!("zstd: {e}")))?;
                if !source.is_empty() {
                    return Err(Error::corrupted("trailing bytes after zstd frame"));
                }
                out
            }
        };
        if out.len() != raw_len {
            return Err(Error::corrupted(format!(
                "segment decompressed to {} bytes, expected {raw_len}",
                out.len()
            )));
        }
        Ok(out)
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Codec {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "none" | "uncompressed" => Codec::None,
            "lz4" => Codec::Lz4,
            "zstd" | "zstandard" => Codec::Zstd,
            _ => return Err(Error::InvalidArgument(format!("unknown codec '{s}'"))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample() -> Vec<u8> {
        (0..10_000u32)
            .flat_map(|i| (i % 17).to_le_bytes())
            .collect()
    }

    #[test]
    fn codecs_round_trip() {
        let raw = sample();
        for codec in [Codec::Lz4, Codec::Zstd] {
            let stored = codec.compress(&raw).expect("repetitive data compresses");
            assert!(stored.len() < raw.len() / 4, "{codec}: {}", stored.len());
            assert_eq!(codec.decompress(&stored, raw.len()).unwrap(), raw);
        }
        assert_eq!(Codec::None.compress(&raw), None);
        assert_eq!(Codec::None.decompress(&raw, raw.len()).unwrap(), raw);
    }

    #[test]
    fn wrong_length_is_rejected() {
        let raw = sample();
        for codec in [Codec::Lz4, Codec::Zstd] {
            let stored = codec.compress(&raw).unwrap();
            assert!(codec.decompress(&stored, raw.len() - 1).is_err());
            assert!(codec.decompress(&stored, raw.len() + 1).is_err());
        }
        assert!(Codec::None.decompress(&raw, 3).is_err());
    }

    #[test]
    fn maximally_compressible_data_stays_within_the_lz4_ratio_bound() {
        let raw = vec![0u8; 4 << 20];
        let stored = Codec::Lz4.compress(&raw).unwrap();
        assert!(
            raw.len() <= stored.len() * LZ4_MAX_RATIO + 16,
            "{}",
            stored.len()
        );
        assert_eq!(Codec::Lz4.decompress(&stored, raw.len()).unwrap(), raw);
    }

    #[test]
    fn impossible_claimed_sizes_are_refused_before_allocating() {
        let stored = Codec::Lz4.compress(&sample()).unwrap();
        let err = Codec::Lz4.decompress(&stored, 1 << 30).unwrap_err();
        assert!(err.to_string().contains("cannot expand"), "{err}");
        let stored = Codec::Zstd.compress(&sample()).unwrap();
        assert!(Codec::Zstd.decompress(&stored, 1 << 30).is_err());
    }

    #[test]
    fn incompressible_data_is_not_compressed() {
        assert_eq!(Codec::Zstd.compress(&[1, 2, 3]), None);
        assert_eq!(Codec::Lz4.compress(&[]), None);
    }

    proptest! {
        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..256), len in 0usize..4096) {
            let _ = Codec::Lz4.decompress(&data, len);
            let _ = Codec::Zstd.decompress(&data, len);
        }

        #[test]
        fn round_trip(data in proptest::collection::vec(0u8..4, 0..2000)) {
            for codec in [Codec::Lz4, Codec::Zstd] {
                if let Some(stored) = codec.compress(&data) {
                    prop_assert_eq!(codec.decompress(&stored, data.len()).unwrap(), data.clone());
                }
            }
        }
    }
}
