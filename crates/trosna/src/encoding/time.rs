//! Delta-of-delta encoding of time stamps (SPEC §6.3, formula (2) of the
//! review): `Δᵢ = tᵢ − tᵢ₋₁`, `Dᵢ = Δᵢ − Δᵢ₋₁`, with Gorilla-style prefix codes,
//! so a regularly sampled point costs a single bit.

use crate::bits::{BitReader, BitWriter};
use crate::bytes::{ByteReader, PutBytes, unzigzag, zigzag};
use crate::error::Result;

pub(crate) fn delta_of_delta_encode(values: &[i64], out: &mut Vec<u8>) {
    let Some(&first) = values.first() else {
        return;
    };
    out.put_svarint(first);
    if values.len() < 2 {
        return;
    }
    let mut prev_delta = values[1].wrapping_sub(values[0]);
    out.put_svarint(prev_delta);
    if values.len() < 3 {
        return;
    }
    let mut bits = BitWriter::with_capacity_bits(values.len());
    for w in values[1..].windows(2) {
        let delta = w[1].wrapping_sub(w[0]);
        let z = zigzag(delta.wrapping_sub(prev_delta));
        prev_delta = delta;
        // Prefix bits are written first to last, i.e. from the least significant bit.
        if z == 0 {
            bits.write_bit(false);
        } else if z < 1 << 8 {
            bits.write(0b01, 2);
            bits.write(z, 8);
        } else if z < 1 << 16 {
            bits.write(0b011, 3);
            bits.write(z, 16);
        } else if z < 1 << 32 {
            bits.write(0b0111, 4);
            bits.write(z, 32);
        } else {
            bits.write(0b1111, 4);
            bits.write(z, 64);
        }
    }
    out.extend_from_slice(&bits.finish());
}

pub(crate) fn delta_of_delta_decode(data: &[u8], n: usize) -> Result<Vec<i64>> {
    let mut r = ByteReader::new(data);
    let mut out = Vec::with_capacity(super::decode_capacity(n, data.len()));
    if n >= 1 {
        out.push(r.svarint()?);
    }
    if n >= 2 {
        let mut delta = r.svarint()?;
        let mut prev = out[0].wrapping_add(delta);
        out.push(prev);
        if n >= 3 {
            let rest = r.bytes(r.remaining())?;
            let mut bits = BitReader::new(rest);
            for _ in 2..n {
                let z = match bits.read_unary(4)? {
                    0 => 0,
                    1 => bits.read(8)?,
                    2 => bits.read(16)?,
                    3 => bits.read(32)?,
                    _ => bits.read(64)?,
                };
                delta = delta.wrapping_add(unzigzag(z));
                prev = prev.wrapping_add(delta);
                out.push(prev);
            }
            bits.finish()?;
        }
    }
    r.finish("delta-of-delta values")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn regular_sampling_costs_one_bit_per_point() {
        let values: Vec<i64> = (0..10_000).map(|i| 1_700_000_000_000 + 1_000 * i).collect();
        let mut out = Vec::new();
        delta_of_delta_encode(&values, &mut out);
        // 7 + 2 bytes of header, then one bit per point.
        assert!(out.len() <= 9 + 10_000 / 8 + 1, "{} bytes", out.len());
        assert_eq!(delta_of_delta_decode(&out, values.len()).unwrap(), values);
    }

    #[test]
    fn every_prefix_class() {
        let deltas = [10i64, 10, 11, 300, 70_000, 5_000_000_000, -5, 10];
        let mut values = vec![0i64];
        for d in deltas {
            values.push(values.last().unwrap() + d);
        }
        let mut out = Vec::new();
        delta_of_delta_encode(&values, &mut out);
        assert_eq!(delta_of_delta_decode(&out, values.len()).unwrap(), values);
    }

    #[test]
    fn short_inputs() {
        for values in [vec![], vec![42], vec![42, 43], vec![i64::MIN, i64::MAX]] {
            let mut out = Vec::new();
            delta_of_delta_encode(&values, &mut out);
            assert_eq!(delta_of_delta_decode(&out, values.len()).unwrap(), values);
        }
    }

    #[test]
    fn wrong_count_is_rejected() {
        let values: Vec<i64> = (0..100).collect();
        let mut out = Vec::new();
        delta_of_delta_encode(&values, &mut out);
        // 98 one-bit codes fill 13 bytes; 88 codes would leave two bytes over.
        assert!(delta_of_delta_decode(&out, 90).is_err());
        assert!(delta_of_delta_decode(&out, 120).is_err());
    }

    proptest! {
        #[test]
        fn round_trip(values in proptest::collection::vec(any::<i64>(), 0..300)) {
            let mut out = Vec::new();
            delta_of_delta_encode(&values, &mut out);
            prop_assert_eq!(delta_of_delta_decode(&out, values.len()).unwrap(), values);
        }

        #[test]
        fn jittered_round_trip(start in -1_000_000_000_000i64..1_000_000_000_000, jitter in proptest::collection::vec(-50i64..50, 0..500)) {
            let values: Vec<i64> = jitter.iter().enumerate()
                .map(|(i, j)| start + 1000 * i as i64 + j).collect();
            let mut out = Vec::new();
            delta_of_delta_encode(&values, &mut out);
            prop_assert_eq!(delta_of_delta_decode(&out, values.len()).unwrap(), values);
        }

        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..64), n in 0usize..300) {
            let _ = delta_of_delta_decode(&data, n);
        }
    }
}
