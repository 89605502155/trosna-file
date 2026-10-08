//! Column encodings (SPEC §6), the first stage of the two-stage compression
//! scheme. The second stage, general-purpose codecs, lives in [`crate::codec`].

pub(crate) mod boolean;
pub(crate) mod float;
pub(crate) mod int;
pub(crate) mod string;
pub(crate) mod time;

use std::fmt;

use crate::error::{Error, Result};
use crate::types::DataType;

/// Encoding of the values of a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Encoding {
    /// Fixed-width little-endian values; bitmaps for booleans.
    Plain,
    /// Deltas with frame-of-reference bit packing in groups of 128 (TS_2DIFF-like).
    DeltaBitPack,
    /// Gorilla-style prefix codes for the delta of deltas (time stamps).
    DeltaOfDelta,
    /// Run-length encoding.
    Rle,
    /// Gorilla XOR encoding of IEEE 754 bit patterns.
    Xor,
    /// Dictionary of distinct strings plus bit-packed indices.
    Dictionary,
}

/// Initial capacity for decoding `n` values from `data_len` bytes.
///
/// `n` comes from the file and may be corrupted, so only as much memory as the
/// input could plausibly hold is reserved up front; the vector grows further only
/// while values are really being decoded.
pub(crate) fn decode_capacity(n: usize, data_len: usize) -> usize {
    n.min(data_len.saturating_mul(8).saturating_add(64))
}

impl Encoding {
    pub(crate) fn code(self) -> u8 {
        match self {
            Encoding::Plain => 0,
            Encoding::DeltaBitPack => 1,
            Encoding::DeltaOfDelta => 2,
            Encoding::Rle => 3,
            Encoding::Xor => 4,
            Encoding::Dictionary => 5,
        }
    }

    pub(crate) fn from_code(code: u8) -> Result<Self> {
        Ok(match code {
            0 => Encoding::Plain,
            1 => Encoding::DeltaBitPack,
            2 => Encoding::DeltaOfDelta,
            3 => Encoding::Rle,
            4 => Encoding::Xor,
            5 => Encoding::Dictionary,
            _ => return Err(Error::Unsupported(format!("encoding code {code}"))),
        })
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Plain => "plain",
            Encoding::DeltaBitPack => "delta-bitpack",
            Encoding::DeltaOfDelta => "delta-of-delta",
            Encoding::Rle => "rle",
            Encoding::Xor => "xor",
            Encoding::Dictionary => "dictionary",
        }
    }

    /// Encodings applicable to a column of type `data_type`, or to the time
    /// column if `data_type` is `None`.
    pub fn candidates(data_type: Option<DataType>) -> &'static [Encoding] {
        match data_type {
            None => &[
                Encoding::DeltaOfDelta,
                Encoding::DeltaBitPack,
                Encoding::Plain,
            ],
            Some(DataType::Bool) => &[Encoding::Rle, Encoding::Plain],
            Some(DataType::Int32 | DataType::Int64) => {
                &[Encoding::DeltaBitPack, Encoding::Rle, Encoding::Plain]
            }
            Some(DataType::Float32 | DataType::Float64) => &[Encoding::Xor, Encoding::Plain],
            Some(DataType::Utf8) => &[Encoding::Dictionary, Encoding::Plain],
        }
    }

    /// The classic encoding of the literature for each kind of column:
    /// delta-of-delta for time, Gorilla XOR for floats, TS_2DIFF-like bit
    /// packing for integers, RLE for booleans and dictionaries for strings.
    pub fn classic(data_type: Option<DataType>) -> Encoding {
        Self::candidates(data_type)[0]
    }

    pub(crate) fn check_allowed(self, data_type: Option<DataType>) -> Result<()> {
        if Self::candidates(data_type).contains(&self) {
            Ok(())
        } else {
            let what = data_type.map_or("the time column", DataType::name);
            Err(Error::corrupted(format!(
                "encoding {} is not allowed for {what}",
                self.name()
            )))
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How the writer chooses the encoding of each segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncodingPolicy {
    /// Encode every segment with every applicable encoding and keep the
    /// smallest result (the default).
    #[default]
    Adaptive,
    /// Always use [`Encoding::classic`].
    Classic,
    /// Always use [`Encoding::Plain`] (useful as a baseline in experiments).
    Plain,
}

/// Encodes with each candidate and returns the smallest output.
/// Ties are resolved in favour of the earlier candidate.
pub(crate) fn smallest<F>(candidates: &[Encoding], mut encode: F) -> (Encoding, Vec<u8>)
where
    F: FnMut(Encoding, &mut Vec<u8>),
{
    let mut best: Option<(Encoding, Vec<u8>)> = None;
    for &encoding in candidates {
        let mut out = Vec::new();
        encode(encoding, &mut out);
        if best.as_ref().is_none_or(|(_, b)| out.len() < b.len()) {
            best = Some((encoding, out));
        }
    }
    best.expect("there is at least one candidate encoding")
}

/// Chooses the candidate list for a policy.
pub(crate) fn policy_candidates(
    policy: EncodingPolicy,
    data_type: Option<DataType>,
) -> &'static [Encoding] {
    let all = Encoding::candidates(data_type);
    match policy {
        EncodingPolicy::Adaptive => all,
        EncodingPolicy::Classic => &all[..1],
        EncodingPolicy::Plain => &[Encoding::Plain],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for code in 0..6 {
            assert_eq!(Encoding::from_code(code).unwrap().code(), code);
        }
        assert!(Encoding::from_code(6).is_err());
    }

    #[test]
    fn allowed_encodings() {
        assert!(Encoding::Xor.check_allowed(Some(DataType::Float64)).is_ok());
        assert!(Encoding::Xor.check_allowed(Some(DataType::Int64)).is_err());
        assert!(Encoding::DeltaOfDelta.check_allowed(None).is_ok());
        assert!(
            Encoding::DeltaOfDelta
                .check_allowed(Some(DataType::Int64))
                .is_err()
        );
        for t in DataType::ALL {
            assert!(Encoding::Plain.check_allowed(Some(t)).is_ok());
        }
    }

    #[test]
    fn smallest_prefers_earlier_on_ties() {
        let (enc, out) = smallest(&[Encoding::Rle, Encoding::Plain], |_, out| out.push(1));
        assert_eq!(enc, Encoding::Rle);
        assert_eq!(out, vec![1]);
    }
}
