//! Segments: one column of a `Data` frame (SPEC §5.3). A segment is an
//! optional validity bitmap followed by the encoded non-null values, the
//! whole compressed with a codec.

use crate::bytes::{ByteReader, PutBytes, ceil8};
use crate::codec::Codec;
use crate::column::{Bitmap, Column, Utf8Values, Values};
use crate::encoding::{self, Encoding, EncodingPolicy, boolean, float, int, string, time};
use crate::error::{Error, Result};
use crate::types::DataType;

/// Upper bound on the decompressed size of a segment (SPEC §5.3).
pub(crate) const MAX_RAW_SEGMENT: usize = 1 << 30;

const FLAG_VALIDITY: u8 = 1;
const FLAG_STATS: u8 = 2;

/// Minimum and maximum of the non-null values of a segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Statistics {
    /// For time, bool (`0`/`1`), int32 and int64 columns.
    Int {
        /// Smallest value.
        min: i64,
        /// Largest value.
        max: i64,
    },
    /// For float32 and float64 columns; NaN values are ignored.
    Float {
        /// Smallest value.
        min: f64,
        /// Largest value.
        max: f64,
    },
}

/// Header of a segment as stored in a `Data` frame.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SegmentHeader {
    pub(crate) encoding: Encoding,
    pub(crate) codec: Codec,
    pub(crate) has_validity: bool,
    pub(crate) null_count: u32,
    pub(crate) stored_len: u32,
    pub(crate) raw_len: u32,
    pub(crate) crc: u32,
    pub(crate) stats: Option<Statistics>,
}

impl SegmentHeader {
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.put_u8(self.encoding.code());
        out.put_u8(self.codec.code());
        let mut flags = 0;
        if self.has_validity {
            flags |= FLAG_VALIDITY;
        }
        if self.stats.is_some() {
            flags |= FLAG_STATS;
        }
        out.put_u8(flags);
        out.put_uvarint(u64::from(self.null_count));
        out.put_uvarint(u64::from(self.stored_len));
        out.put_uvarint(u64::from(self.raw_len));
        out.put_u32(self.crc);
        match self.stats {
            None => {}
            Some(Statistics::Int { min, max }) => {
                out.put_i64(min);
                out.put_i64(max);
            }
            Some(Statistics::Float { min, max }) => {
                out.put_u64(min.to_bits());
                out.put_u64(max.to_bits());
            }
        }
    }

    /// Parses a header; `data_type` is `None` for the time column.
    pub(crate) fn read(r: &mut ByteReader<'_>, data_type: Option<DataType>) -> Result<Self> {
        let encoding = Encoding::from_code(r.u8()?)?;
        encoding.check_allowed(data_type)?;
        let codec = Codec::from_code(r.u8()?)?;
        let flags = r.u8()?;
        if flags & !(FLAG_VALIDITY | FLAG_STATS) != 0 {
            return Err(Error::corrupted(format!(
                "unknown segment flags {flags:#x}"
            )));
        }
        let null_count = r.uvarint_u32()?;
        let stored_len = r.uvarint_u32()?;
        let raw_len = r.uvarint_u32()?;
        if raw_len as usize > MAX_RAW_SEGMENT {
            return Err(Error::corrupted(format!(
                "segment of {raw_len} bytes exceeds the limit"
            )));
        }
        let crc = r.u32()?;
        let stats = if flags & FLAG_STATS != 0 {
            Some(match data_type {
                Some(DataType::Float32 | DataType::Float64) => Statistics::Float {
                    min: f64::from_bits(r.u64()?),
                    max: f64::from_bits(r.u64()?),
                },
                Some(DataType::Utf8) => {
                    return Err(Error::corrupted("string segments have no statistics"));
                }
                _ => Statistics::Int {
                    min: r.i64()?,
                    max: r.i64()?,
                },
            })
        } else {
            None
        };
        let header = SegmentHeader {
            encoding,
            codec,
            has_validity: flags & FLAG_VALIDITY != 0,
            null_count,
            stored_len,
            raw_len,
            crc,
            stats,
        };
        if data_type.is_none() && (header.null_count != 0 || header.has_validity) {
            return Err(Error::corrupted("the time column cannot contain nulls"));
        }
        if header.has_validity != (header.null_count > 0) {
            return Err(Error::corrupted(
                "validity flag does not match the null count",
            ));
        }
        Ok(header)
    }
}

/// Settings for encoding segments.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EncodeOptions {
    pub(crate) policy: EncodingPolicy,
    pub(crate) codec: Codec,
}

fn finish_segment(
    raw: Vec<u8>,
    encoding: Encoding,
    null_count: usize,
    stats: Option<Statistics>,
    options: EncodeOptions,
) -> Result<(SegmentHeader, Vec<u8>)> {
    if raw.len() > MAX_RAW_SEGMENT {
        return Err(Error::LimitExceeded(format!(
            "an encoded segment of {} bytes exceeds the 1 GiB limit; use smaller blocks",
            raw.len()
        )));
    }
    let (codec, stored) = match options.codec.compress(&raw) {
        Some(compressed) => (options.codec, compressed),
        None => (Codec::None, raw.clone()),
    };
    let header = SegmentHeader {
        encoding,
        codec,
        has_validity: null_count > 0,
        null_count: null_count as u32,
        stored_len: stored.len() as u32,
        raw_len: raw.len() as u32,
        crc: crc32c::crc32c(&stored),
        stats,
    };
    Ok((header, stored))
}

fn int_stats(values: &[i64]) -> Option<Statistics> {
    let min = *values.iter().min()?;
    let max = *values.iter().max()?;
    Some(Statistics::Int { min, max })
}

fn float_stats(values: impl Iterator<Item = f64>) -> Option<Statistics> {
    let mut acc: Option<(f64, f64)> = None;
    for v in values.filter(|v| !v.is_nan()) {
        acc = Some(match acc {
            None => (v, v),
            Some((lo, hi)) => (lo.min(v), hi.max(v)),
        });
    }
    acc.map(|(min, max)| Statistics::Float { min, max })
}

/// Encodes the time column of a block.
pub(crate) fn encode_time(
    times: &[i64],
    options: EncodeOptions,
) -> Result<(SegmentHeader, Vec<u8>)> {
    let (encoding, raw) = encoding::smallest(
        encoding::policy_candidates(options.policy, None),
        |enc, out| match enc {
            Encoding::DeltaOfDelta => time::delta_of_delta_encode(times, out),
            Encoding::DeltaBitPack => int::delta_bitpack_encode(times, out),
            _ => int::plain_encode(times, 8, out),
        },
    );
    finish_segment(raw, encoding, 0, int_stats(times), options)
}

/// Encodes a value column of a block.
pub(crate) fn encode_column(
    column: &Column,
    options: EncodeOptions,
) -> Result<(SegmentHeader, Vec<u8>)> {
    let data_type = column.data_type();
    let null_count = column.null_count();
    let validity = column.validity();
    let keep = |i: usize| validity.is_none_or(|v| v.get(i));
    let mut raw = Vec::new();
    if let Some(v) = validity {
        raw.extend_from_slice(v.as_bytes());
    }
    let candidates = if null_count == column.len() {
        &[Encoding::Plain][..]
    } else {
        encoding::policy_candidates(options.policy, Some(data_type))
    };
    let (encoding, encoded, stats) = match column.values() {
        Values::Bool(values) => {
            let dense: Bitmap = values
                .iter()
                .enumerate()
                .filter(|&(i, _)| keep(i))
                .map(|(_, b)| b)
                .collect();
            let stats = (!dense.is_empty()).then(|| {
                let ones = dense.count_ones();
                Statistics::Int {
                    min: i64::from(ones == dense.len()),
                    max: i64::from(ones > 0),
                }
            });
            let (enc, out) = encoding::smallest(candidates, |enc, out| match enc {
                Encoding::Rle => boolean::rle_encode(&dense, out),
                _ => boolean::plain_encode(&dense, out),
            });
            (enc, out, stats)
        }
        Values::Int32(values) => {
            let dense: Vec<i64> = values
                .iter()
                .enumerate()
                .filter(|&(i, _)| keep(i))
                .map(|(_, &v)| i64::from(v))
                .collect();
            let (enc, out) = encode_ints(candidates, &dense, 4);
            (enc, out, int_stats(&dense))
        }
        Values::Int64(values) => {
            let dense: Vec<i64> = if validity.is_none() {
                values.clone()
            } else {
                values
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| keep(i))
                    .map(|(_, &v)| v)
                    .collect()
            };
            let (enc, out) = encode_ints(candidates, &dense, 8);
            (enc, out, int_stats(&dense))
        }
        Values::Float32(values) => {
            let dense: Vec<f32> = values
                .iter()
                .enumerate()
                .filter(|&(i, _)| keep(i))
                .map(|(_, &v)| v)
                .collect();
            let bits: Vec<u64> = dense.iter().map(|v| u64::from(v.to_bits())).collect();
            let (enc, out) = encode_floats(candidates, &bits, 32);
            (enc, out, float_stats(dense.iter().map(|&v| f64::from(v))))
        }
        Values::Float64(values) => {
            let dense: Vec<f64> = values
                .iter()
                .enumerate()
                .filter(|&(i, _)| keep(i))
                .map(|(_, &v)| v)
                .collect();
            let bits: Vec<u64> = dense.iter().map(|v| v.to_bits()).collect();
            let (enc, out) = encode_floats(candidates, &bits, 64);
            (enc, out, float_stats(dense.iter().copied()))
        }
        Values::Utf8(values) => {
            let dense: Vec<&str> = values
                .iter()
                .enumerate()
                .filter(|&(i, _)| keep(i))
                .map(|(_, s)| s)
                .collect();
            let (enc, out) = encoding::smallest(candidates, |enc, out| match enc {
                Encoding::Dictionary => string::dictionary_encode(&dense, out),
                _ => string::plain_encode(dense.iter().copied(), out),
            });
            (enc, out, None)
        }
    };
    raw.extend_from_slice(&encoded);
    finish_segment(raw, encoding, null_count, stats, options)
}

fn encode_ints(candidates: &[Encoding], values: &[i64], width: usize) -> (Encoding, Vec<u8>) {
    encoding::smallest(candidates, |enc, out| match enc {
        Encoding::DeltaBitPack => int::delta_bitpack_encode(values, out),
        Encoding::Rle => int::rle_encode(values, out),
        _ => int::plain_encode(values, width, out),
    })
}

fn encode_floats(candidates: &[Encoding], bits: &[u64], width: u32) -> (Encoding, Vec<u8>) {
    encoding::smallest(candidates, |enc, out| match enc {
        Encoding::Xor => float::xor_encode(bits, width, out),
        _ => float::plain_encode(bits, width, out),
    })
}

/// Verifies the checksum and decompresses the stored bytes of a segment.
fn unpack(
    header: &SegmentHeader,
    stored: &[u8],
    rows: usize,
    data_type: Option<DataType>,
    verify: bool,
) -> Result<Vec<u8>> {
    if stored.len() != header.stored_len as usize {
        return Err(Error::corrupted("segment length does not match its header"));
    }
    if verify && crc32c::crc32c(stored) != header.crc {
        return Err(Error::corrupted("segment checksum mismatch"));
    }
    // Every value encoding of a non-string column needs at most 11 bytes per
    // value, which bounds the decompressed size and defeats decompression bombs.
    let raw_len = header.raw_len as usize;
    if !matches!(data_type, Some(DataType::Utf8)) && raw_len > ceil8(rows) + rows * 11 + 32 {
        return Err(Error::corrupted("implausible decompressed segment size"));
    }
    header.codec.decompress(stored, raw_len)
}

/// Decodes the time column of a block of `rows` rows.
pub(crate) fn decode_time(
    header: &SegmentHeader,
    stored: &[u8],
    rows: usize,
    verify: bool,
) -> Result<Vec<i64>> {
    let raw = unpack(header, stored, rows, None, verify)?;
    match header.encoding {
        Encoding::DeltaOfDelta => time::delta_of_delta_decode(&raw, rows),
        Encoding::DeltaBitPack => int::delta_bitpack_decode(&raw, rows),
        _ => int::plain_decode(&raw, rows, 8),
    }
}

/// Decodes a value column of a block of `rows` rows.
pub(crate) fn decode_column(
    header: &SegmentHeader,
    stored: &[u8],
    rows: usize,
    data_type: DataType,
    verify: bool,
) -> Result<Column> {
    let raw = unpack(header, stored, rows, Some(data_type), verify)?;
    let null_count = header.null_count as usize;
    if null_count > rows {
        return Err(Error::corrupted("null count exceeds the number of rows"));
    }
    let (validity, data) = if header.has_validity {
        let n = ceil8(rows);
        if raw.len() < n {
            return Err(Error::corrupted(
                "segment too short for its validity bitmap",
            ));
        }
        let bitmap = boolean::plain_decode(&raw[..n], rows)?;
        if bitmap.count_ones() != rows - null_count {
            return Err(Error::corrupted(
                "validity bitmap does not match the null count",
            ));
        }
        (Some(bitmap), &raw[n..])
    } else {
        (None, &raw[..])
    };
    let n = rows - null_count;
    let values = match data_type {
        DataType::Bool => {
            let dense = match header.encoding {
                Encoding::Rle => boolean::rle_decode(data, n)?,
                _ => boolean::plain_decode(data, n)?,
            };
            Values::Bool(match &validity {
                None => dense,
                Some(v) => scatter_bits(&dense, v),
            })
        }
        DataType::Int32 => {
            let wide = decode_ints(header.encoding, data, n, 4)?;
            let dense = wide
                .into_iter()
                .map(|v| i32::try_from(v).map_err(|_| Error::corrupted("int32 value out of range")))
                .collect::<Result<Vec<i32>>>()?;
            Values::Int32(scatter(dense, validity.as_ref()))
        }
        DataType::Int64 => Values::Int64(scatter(
            decode_ints(header.encoding, data, n, 8)?,
            validity.as_ref(),
        )),
        DataType::Float32 => {
            let bits = decode_floats(header.encoding, data, n, 32)?;
            let dense = bits.into_iter().map(|b| f32::from_bits(b as u32)).collect();
            Values::Float32(scatter(dense, validity.as_ref()))
        }
        DataType::Float64 => {
            let bits = decode_floats(header.encoding, data, n, 64)?;
            let dense = bits.into_iter().map(f64::from_bits).collect();
            Values::Float64(scatter(dense, validity.as_ref()))
        }
        DataType::Utf8 => {
            let dense = match header.encoding {
                Encoding::Dictionary => string::dictionary_decode(data, n)?,
                _ => string::plain_decode(data, n)?,
            };
            Values::Utf8(match &validity {
                None => dense,
                Some(v) => scatter_strings(&dense, v)?,
            })
        }
    };
    Column::new(values, validity).map_err(|e| Error::corrupted(e.to_string()))
}

fn decode_ints(encoding: Encoding, data: &[u8], n: usize, width: usize) -> Result<Vec<i64>> {
    match encoding {
        Encoding::DeltaBitPack => int::delta_bitpack_decode(data, n),
        Encoding::Rle => int::rle_decode(data, n),
        _ => int::plain_decode(data, n, width),
    }
}

fn decode_floats(encoding: Encoding, data: &[u8], n: usize, width: u32) -> Result<Vec<u64>> {
    match encoding {
        Encoding::Xor => float::xor_decode(data, n, width),
        _ => float::plain_decode(data, n, width),
    }
}

/// Spreads dense values over the valid positions; null slots get the default value.
fn scatter<T: Copy + Default>(dense: Vec<T>, validity: Option<&Bitmap>) -> Vec<T> {
    let Some(validity) = validity else {
        return dense;
    };
    let mut values = dense.into_iter();
    validity
        .iter()
        .map(|valid| {
            if valid {
                values.next().unwrap_or_default()
            } else {
                T::default()
            }
        })
        .collect()
}

fn scatter_bits(dense: &Bitmap, validity: &Bitmap) -> Bitmap {
    let mut next = 0;
    validity
        .iter()
        .map(|valid| {
            if valid {
                next += 1;
                dense.get(next - 1)
            } else {
                false
            }
        })
        .collect()
}

fn scatter_strings(dense: &Utf8Values, validity: &Bitmap) -> Result<Utf8Values> {
    let mut out = Utf8Values::with_capacity(validity.len(), dense.data().len());
    let mut next = 0;
    for valid in validity.iter() {
        if valid {
            out.push(dense.get(next))?;
            next += 1;
        } else {
            out.push("")?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADAPTIVE: EncodeOptions = EncodeOptions {
        policy: EncodingPolicy::Adaptive,
        codec: Codec::Zstd,
    };

    fn round_trip(column: &Column, options: EncodeOptions) -> Column {
        let (header, stored) = encode_column(column, options).unwrap();
        let mut bytes = Vec::new();
        header.write(&mut bytes);
        let parsed =
            SegmentHeader::read(&mut ByteReader::new(&bytes), Some(column.data_type())).unwrap();
        assert_eq!(parsed, header);
        decode_column(&parsed, &stored, column.len(), column.data_type(), true).unwrap()
    }

    fn sample_columns() -> Vec<Column> {
        vec![
            Column::from(vec![Some(true), None, Some(false), Some(true)]),
            Column::from(vec![1i32, -5, i32::MAX, i32::MIN]),
            Column::from(vec![Some(10i64), Some(11), None, Some(13)]),
            Column::from(vec![Some(1.5f32), None, Some(f32::NAN), Some(-0.0)]),
            Column::from(vec![1.0f64, 1.0, 1.25, f64::INFINITY]),
            Column::from(vec![Some("a"), None, Some("б"), Some("a")]),
            Column::from(vec![None::<f64>, None, None]),
            Column::from(Vec::<i64>::new()),
        ]
    }

    #[test]
    fn columns_round_trip_with_every_policy() {
        for policy in [
            EncodingPolicy::Adaptive,
            EncodingPolicy::Classic,
            EncodingPolicy::Plain,
        ] {
            for codec in [Codec::None, Codec::Lz4, Codec::Zstd] {
                for column in sample_columns() {
                    let options = EncodeOptions { policy, codec };
                    assert_eq!(round_trip(&column, options), column, "{policy:?} {codec:?}");
                }
            }
        }
    }

    #[test]
    fn statistics() {
        let (h, _) =
            encode_column(&Column::from(vec![Some(3i64), None, Some(-7)]), ADAPTIVE).unwrap();
        assert_eq!(h.stats, Some(Statistics::Int { min: -7, max: 3 }));
        let (h, _) = encode_column(&Column::from(vec![f64::NAN, 2.0, -1.0]), ADAPTIVE).unwrap();
        assert_eq!(
            h.stats,
            Some(Statistics::Float {
                min: -1.0,
                max: 2.0
            })
        );
        let (h, _) = encode_column(&Column::from(vec![f64::NAN]), ADAPTIVE).unwrap();
        assert_eq!(h.stats, None);
        let (h, _) = encode_column(&Column::from(vec![true, true]), ADAPTIVE).unwrap();
        assert_eq!(h.stats, Some(Statistics::Int { min: 1, max: 1 }));
    }

    #[test]
    fn adaptive_choice_depends_on_data() {
        let constant = Column::from(vec![5i64; 1000]);
        let (h, _) = encode_column(
            &constant,
            EncodeOptions {
                policy: EncodingPolicy::Adaptive,
                codec: Codec::None,
            },
        )
        .unwrap();
        assert!(matches!(h.encoding, Encoding::Rle | Encoding::DeltaBitPack));
        let times: Vec<i64> = (0..1000).map(|i| 1_000_000 + i * 1000).collect();
        let (h, stored) = encode_time(&times, ADAPTIVE).unwrap();
        assert!(stored.len() < 64, "{} bytes", stored.len());
        assert_eq!(decode_time(&h, &stored, times.len(), true).unwrap(), times);
    }

    #[test]
    fn checksum_and_bounds_are_enforced() {
        let column = Column::from((0..100).map(|i| i as f64).collect::<Vec<_>>());
        let (header, mut stored) = encode_column(&column, ADAPTIVE).unwrap();
        stored[0] ^= 1;
        assert!(decode_column(&header, &stored, 100, DataType::Float64, true).is_err());
        let (header, stored) = encode_column(&column, ADAPTIVE).unwrap();
        assert!(decode_column(&header, &stored, 99, DataType::Float64, true).is_err());
        let mut bomb = header.clone();
        bomb.raw_len = 1 << 29;
        assert!(decode_column(&bomb, &stored, 100, DataType::Float64, false).is_err());
    }

    #[test]
    fn header_validation() {
        let mut bytes = Vec::new();
        SegmentHeader {
            encoding: Encoding::Xor,
            codec: Codec::None,
            has_validity: false,
            null_count: 0,
            stored_len: 0,
            raw_len: 0,
            crc: 0,
            stats: None,
        }
        .write(&mut bytes);
        assert!(SegmentHeader::read(&mut ByteReader::new(&bytes), Some(DataType::Int64)).is_err());
        assert!(SegmentHeader::read(&mut ByteReader::new(&bytes), Some(DataType::Float64)).is_ok());
        let mut nulls = Vec::new();
        SegmentHeader {
            encoding: Encoding::Plain,
            codec: Codec::None,
            has_validity: true,
            null_count: 1,
            stored_len: 1,
            raw_len: 1,
            crc: 0,
            stats: None,
        }
        .write(&mut nulls);
        assert!(SegmentHeader::read(&mut ByteReader::new(&nulls), None).is_err());
    }
}
