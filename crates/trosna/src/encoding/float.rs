//! Floating-point encodings (SPEC §6.1, §6.5), operating on IEEE 754 bit
//! patterns: `u64` for float64 and zero-extended `u32` for float32.
//!
//! `Xor` is the Gorilla scheme (formula (3) of the review): consecutive values
//! are XOR-ed, and only the meaningful bits between the leading and trailing
//! zeros are stored, reusing the previous bit window when it fits.

use crate::bits::{BitReader, BitWriter};
use crate::error::{Error, Result};

fn field_width(width: u32) -> u32 {
    if width == 64 { 6 } else { 5 }
}

fn leading_zeros(x: u64, width: u32) -> u32 {
    x.leading_zeros() - (64 - width)
}

pub(crate) fn plain_encode(bits: &[u64], width: u32, out: &mut Vec<u8>) {
    out.reserve(bits.len() * width as usize / 8);
    if width == 32 {
        for &b in bits {
            out.extend_from_slice(&(b as u32).to_le_bytes());
        }
    } else {
        for &b in bits {
            out.extend_from_slice(&b.to_le_bytes());
        }
    }
}

pub(crate) fn plain_decode(data: &[u8], n: usize, width: u32) -> Result<Vec<u64>> {
    let size = width as usize / 8;
    if n.checked_mul(size) != Some(data.len()) {
        return Err(Error::corrupted(format!(
            "plain segment has {} bytes for {n} values of {size} bytes",
            data.len()
        )));
    }
    Ok(if width == 32 {
        data.chunks_exact(4)
            .map(|c| u64::from(u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
            .collect()
    } else {
        data.chunks_exact(8)
            .map(|c| u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
            .collect()
    })
}

pub(crate) fn xor_encode(bits: &[u64], width: u32, out: &mut Vec<u8>) {
    let Some(&first) = bits.first() else {
        return;
    };
    let fw = field_width(width);
    let mut w = BitWriter::with_capacity_bits(width as usize + bits.len() * 2);
    w.write(first, width);
    let mut prev = first;
    let mut window: Option<(u32, u32)> = None;
    for &v in &bits[1..] {
        let x = v ^ prev;
        prev = v;
        if x == 0 {
            w.write_bit(false);
            continue;
        }
        w.write_bit(true);
        let lead = leading_zeros(x, width);
        let trail = x.trailing_zeros();
        match window {
            Some((l, t)) if lead >= l && trail >= t => {
                w.write_bit(false);
                w.write(x >> t, width - l - t);
            }
            _ => {
                let len = width - lead - trail;
                w.write_bit(true);
                w.write(u64::from(lead), fw);
                w.write(u64::from(len - 1), fw);
                w.write(x >> trail, len);
                window = Some((lead, trail));
            }
        }
    }
    out.extend_from_slice(&w.finish());
}

pub(crate) fn xor_decode(data: &[u8], n: usize, width: u32) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(super::decode_capacity(n, data.len()));
    let mut r = BitReader::new(data);
    if n > 0 {
        let fw = field_width(width);
        let mut prev = r.read(width)?;
        out.push(prev);
        let mut window: Option<(u32, u32)> = None;
        for _ in 1..n {
            if r.read_bit()? {
                if r.read_bit()? {
                    let lead = r.read(fw)? as u32;
                    let len = r.read(fw)? as u32 + 1;
                    if lead + len > width {
                        return Err(Error::corrupted(format!(
                            "XOR window of {lead}+{len} bits exceeds {width} bits"
                        )));
                    }
                    let trail = width - lead - len;
                    prev ^= r.read(len)? << trail;
                    window = Some((lead, trail));
                } else {
                    let (l, t) = window.ok_or_else(|| {
                        Error::corrupted("XOR stream reuses a window before defining one")
                    })?;
                    prev ^= r.read(width - l - t)? << t;
                }
            }
            out.push(prev);
        }
    }
    r.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn bits64(values: &[f64]) -> Vec<u64> {
        values.iter().map(|v| v.to_bits()).collect()
    }

    #[test]
    fn slowly_changing_signal_compresses() {
        let values: Vec<f64> = (0..1000).map(|i| 20.0 + f64::from(i / 50) * 0.5).collect();
        let mut out = Vec::new();
        xor_encode(&bits64(&values), 64, &mut out);
        assert!(out.len() < 1000 * 8 / 10, "{} bytes", out.len());
        assert_eq!(xor_decode(&out, values.len(), 64).unwrap(), bits64(&values));
    }

    #[test]
    fn special_values_are_bit_exact() {
        let values = [
            0.0,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            f64::MAX,
            1e-300,
        ];
        let bits = bits64(&values);
        let mut out = Vec::new();
        xor_encode(&bits, 64, &mut out);
        assert_eq!(xor_decode(&out, bits.len(), 64).unwrap(), bits);
        let nan_payload = vec![0x7ff8_0000_dead_beef_u64, 0xfff0_0000_0000_0001];
        let mut out = Vec::new();
        xor_encode(&nan_payload, 64, &mut out);
        assert_eq!(xor_decode(&out, 2, 64).unwrap(), nan_payload);
    }

    #[test]
    fn float32_width() {
        let values: Vec<u64> = [1.5f32, 1.5, 2.25, -0.0, f32::NAN, f32::MAX]
            .iter()
            .map(|v| u64::from(v.to_bits()))
            .collect();
        let mut out = Vec::new();
        xor_encode(&values, 32, &mut out);
        assert_eq!(xor_decode(&out, values.len(), 32).unwrap(), values);
    }

    #[test]
    fn window_reuse_before_definition_is_rejected() {
        let mut w = BitWriter::new();
        w.write(0, 64);
        w.write(0b01, 2); // "1" then "0": reuse window
        let data = w.finish();
        assert!(xor_decode(&data, 2, 64).is_err());
    }

    proptest! {
        #[test]
        fn round_trip64(values in proptest::collection::vec(any::<u64>(), 0..300)) {
            let mut out = Vec::new();
            xor_encode(&values, 64, &mut out);
            prop_assert_eq!(&xor_decode(&out, values.len(), 64).unwrap(), &values);
            let mut out = Vec::new();
            plain_encode(&values, 64, &mut out);
            prop_assert_eq!(&plain_decode(&out, values.len(), 64).unwrap(), &values);
        }

        #[test]
        fn round_trip32(values in proptest::collection::vec(any::<u32>(), 0..300)) {
            let values: Vec<u64> = values.into_iter().map(u64::from).collect();
            let mut out = Vec::new();
            xor_encode(&values, 32, &mut out);
            prop_assert_eq!(&xor_decode(&out, values.len(), 32).unwrap(), &values);
            let mut out = Vec::new();
            plain_encode(&values, 32, &mut out);
            prop_assert_eq!(&plain_decode(&out, values.len(), 32).unwrap(), &values);
        }

        #[test]
        fn smooth_round_trip(start in -1e6f64..1e6, steps in proptest::collection::vec(-1.0f64..1.0, 0..300)) {
            let mut v = start;
            let values: Vec<u64> = steps.iter().map(|s| { v += (s * 100.0).round() / 100.0; v.to_bits() }).collect();
            let mut out = Vec::new();
            xor_encode(&values, 64, &mut out);
            prop_assert_eq!(xor_decode(&out, values.len(), 64).unwrap(), values);
        }

        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..64), n in 0usize..200) {
            let _ = xor_decode(&data, n, 64);
            let _ = xor_decode(&data, n, 32);
            let _ = plain_decode(&data, n, 32);
        }
    }
}
