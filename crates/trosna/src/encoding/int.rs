//! Integer encodings (SPEC §6.1, §6.2, §6.4), operating on `i64`.
//! Int32 columns are widened to `i64`; `width` selects the Plain layout.

use crate::bits::{BitReader, BitWriter, bit_width};
use crate::bytes::{ByteReader, PutBytes, ceil8};
use crate::error::{Error, Result};

/// Deltas are bit-packed in groups of this many values.
pub(crate) const GROUP: usize = 128;

pub(crate) fn plain_encode(values: &[i64], width: usize, out: &mut Vec<u8>) {
    out.reserve(values.len() * width);
    if width == 4 {
        for &v in values {
            out.extend_from_slice(&(v as i32).to_le_bytes());
        }
    } else {
        for &v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

pub(crate) fn plain_decode(data: &[u8], n: usize, width: usize) -> Result<Vec<i64>> {
    if n.checked_mul(width) != Some(data.len()) {
        return Err(Error::corrupted(format!(
            "plain segment has {} bytes for {n} values of {width} bytes",
            data.len()
        )));
    }
    Ok(if width == 4 {
        data.chunks_exact(4)
            .map(|c| i64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]])))
            .collect()
    } else {
        data.chunks_exact(8)
            .map(|c| i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
            .collect()
    })
}

pub(crate) fn delta_bitpack_encode(values: &[i64], out: &mut Vec<u8>) {
    let Some(&first) = values.first() else {
        return;
    };
    out.put_svarint(first);
    let deltas: Vec<i64> = values.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
    for group in deltas.chunks(GROUP) {
        let min = *group.iter().min().expect("groups are non-empty");
        let max_offset = group
            .iter()
            .map(|d| d.wrapping_sub(min) as u64)
            .max()
            .expect("groups are non-empty");
        let width = bit_width(max_offset);
        out.put_svarint(min);
        out.put_u8(width as u8);
        let mut bits = BitWriter::with_capacity_bits(group.len() * width as usize);
        for d in group {
            bits.write(d.wrapping_sub(min) as u64, width);
        }
        out.extend_from_slice(&bits.finish());
    }
}

pub(crate) fn delta_bitpack_decode(data: &[u8], n: usize) -> Result<Vec<i64>> {
    let mut r = ByteReader::new(data);
    let mut out = Vec::with_capacity(super::decode_capacity(n, data.len()));
    if n > 0 {
        let mut prev = r.svarint()?;
        out.push(prev);
        let mut remaining = n - 1;
        while remaining > 0 {
            let group = remaining.min(GROUP);
            let min = r.svarint()?;
            let width = u32::from(r.u8()?);
            if width > 64 {
                return Err(Error::corrupted(format!("bit width {width} exceeds 64")));
            }
            let packed = r.bytes(ceil8(group * width as usize))?;
            let mut bits = BitReader::new(packed);
            for _ in 0..group {
                let delta = min.wrapping_add(bits.read(width)? as i64);
                prev = prev.wrapping_add(delta);
                out.push(prev);
            }
            bits.finish()?;
            remaining -= group;
        }
    }
    r.finish("delta-bitpack values")?;
    Ok(out)
}

pub(crate) fn rle_encode(values: &[i64], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < values.len() {
        let v = values[i];
        let run = values[i..].iter().take_while(|&&x| x == v).count();
        out.put_uvarint(run as u64);
        out.put_svarint(v);
        i += run;
    }
}

pub(crate) fn rle_decode(data: &[u8], n: usize) -> Result<Vec<i64>> {
    let mut r = ByteReader::new(data);
    let mut out = Vec::with_capacity(super::decode_capacity(n, data.len()));
    while out.len() < n {
        let run = r.len(n - out.len(), "run length")?;
        if run == 0 {
            return Err(Error::corrupted("zero-length run"));
        }
        let v = r.svarint()?;
        out.resize(out.len() + run, v);
    }
    r.finish("rle values")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn regular_counter_packs_to_zero_width() {
        let values: Vec<i64> = (0..1000).map(|i| 1_000 + 5 * i).collect();
        let mut out = Vec::new();
        delta_bitpack_encode(&values, &mut out);
        // first value + 8 groups of (min, width = 0, no packed bytes)
        assert!(out.len() < 30, "{} bytes", out.len());
        assert_eq!(delta_bitpack_decode(&out, values.len()).unwrap(), values);
    }

    #[test]
    fn extreme_values() {
        let values = vec![i64::MIN, i64::MAX, 0, -1, i64::MIN, i64::MAX];
        let mut out = Vec::new();
        delta_bitpack_encode(&values, &mut out);
        assert_eq!(delta_bitpack_decode(&out, values.len()).unwrap(), values);
        let mut out = Vec::new();
        rle_encode(&values, &mut out);
        assert_eq!(rle_decode(&out, values.len()).unwrap(), values);
    }

    #[test]
    fn rle_merges_runs() {
        let values = vec![7, 7, 7, 7, -2, -2, 7];
        let mut out = Vec::new();
        rle_encode(&values, &mut out);
        assert_eq!(out, vec![4, 14, 2, 3, 1, 14]);
        assert_eq!(rle_decode(&out, 7).unwrap(), values);
    }

    #[test]
    fn decoders_reject_wrong_counts() {
        let mut out = Vec::new();
        rle_encode(&[1, 1, 1], &mut out);
        assert!(rle_decode(&out, 2).is_err());
        assert!(rle_decode(&out, 4).is_err());
        // The count comes from the segment header; with non-zero bit widths a
        // wrong count leaves bytes over or runs out of them.
        let mut out = Vec::new();
        delta_bitpack_encode(&[1, 200, 3, 4000], &mut out);
        assert!(delta_bitpack_decode(&out, 2).is_err());
        assert!(delta_bitpack_decode(&out, 5).is_err());
        assert!(plain_decode(&[0; 7], 1, 8).is_err());
        assert!(rle_decode(&[0, 0], 1).is_err());
    }

    proptest! {
        #[test]
        fn round_trips(values in proptest::collection::vec(any::<i64>(), 0..400)) {
            let mut out = Vec::new();
            plain_encode(&values, 8, &mut out);
            prop_assert_eq!(&plain_decode(&out, values.len(), 8).unwrap(), &values);
            let mut out = Vec::new();
            delta_bitpack_encode(&values, &mut out);
            prop_assert_eq!(&delta_bitpack_decode(&out, values.len()).unwrap(), &values);
            let mut out = Vec::new();
            rle_encode(&values, &mut out);
            prop_assert_eq!(&rle_decode(&out, values.len()).unwrap(), &values);
        }

        #[test]
        fn i32_plain_round_trip(values in proptest::collection::vec(any::<i32>(), 0..100)) {
            let wide: Vec<i64> = values.iter().map(|&v| i64::from(v)).collect();
            let mut out = Vec::new();
            plain_encode(&wide, 4, &mut out);
            prop_assert_eq!(out.len(), values.len() * 4);
            prop_assert_eq!(plain_decode(&out, values.len(), 4).unwrap(), wide);
        }

        #[test]
        fn small_steps_round_trip(start in any::<i64>(), steps in proptest::collection::vec(-3i64..=3, 0..300)) {
            let mut values = vec![start];
            for s in steps {
                let last = *values.last().unwrap();
                values.push(last.wrapping_add(s));
            }
            let mut out = Vec::new();
            delta_bitpack_encode(&values, &mut out);
            prop_assert_eq!(delta_bitpack_decode(&out, values.len()).unwrap(), values);
        }

        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..64), n in 0usize..200) {
            let _ = delta_bitpack_decode(&data, n);
            let _ = rle_decode(&data, n);
            let _ = plain_decode(&data, n, 4);
        }
    }
}
