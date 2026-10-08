//! Least-significant-bit-first bit streams (SPEC §1).

use crate::bytes::ceil8;
use crate::error::{Error, Result};

#[inline]
fn low_bits(value: u64, n: u32) -> u64 {
    if n >= 64 {
        value
    } else {
        value & ((1u64 << n) - 1)
    }
}

/// Writes bit fields into a byte vector, least significant bit first.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_capacity_bits(bits: usize) -> Self {
        BitWriter {
            out: Vec::with_capacity(ceil8(bits)),
            acc: 0,
            nbits: 0,
        }
    }

    #[inline]
    pub(crate) fn write_bit(&mut self, bit: bool) {
        self.write(u64::from(bit), 1);
    }

    /// Writes the `n` low bits of `value` (`n ≤ 64`).
    #[inline]
    pub(crate) fn write(&mut self, value: u64, n: u32) {
        debug_assert!(n <= 64);
        if n == 0 {
            return;
        }
        let value = low_bits(value, n);
        let space = 64 - self.nbits;
        if n < space {
            self.acc |= value << self.nbits;
            self.nbits += n;
        } else {
            self.acc |= value << self.nbits;
            self.out.extend_from_slice(&self.acc.to_le_bytes());
            // `space` bits of `value` went into the flushed word.
            self.acc = if space == 64 { 0 } else { value >> space };
            self.nbits = n - space;
        }
    }

    /// Pads the last byte with zeros and returns the bytes.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        let tail = ceil8(self.nbits as usize);
        self.out.extend_from_slice(&self.acc.to_le_bytes()[..tail]);
        self.out
    }
}

/// Reads bit fields written by [`BitWriter`].
#[derive(Debug)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    len_bits: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            len_bits: data.len() * 8,
        }
    }

    pub(crate) fn remaining_bits(&self) -> usize {
        self.len_bits - self.pos
    }

    /// Reads `n ≤ 64` bits.
    #[inline]
    pub(crate) fn read(&mut self, n: u32) -> Result<u64> {
        if n == 0 {
            return Ok(0);
        }
        if self.remaining_bits() < n as usize {
            return Err(Error::corrupted("unexpected end of bit stream"));
        }
        let byte = self.pos >> 3;
        let off = (self.pos & 7) as u32;
        let mut word = [0u8; 8];
        let available = (self.data.len() - byte).min(8);
        word[..available].copy_from_slice(&self.data[byte..byte + available]);
        let mut value = u64::from_le_bytes(word) >> off;
        if off + n > 64 {
            // The field extends into a ninth byte, which exists by the length check.
            value |= u64::from(self.data[byte + 8]) << (64 - off);
        }
        self.pos += n as usize;
        Ok(low_bits(value, n))
    }

    #[inline]
    pub(crate) fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read(1)? == 1)
    }

    /// Counts consecutive one bits, up to `max`, consuming them and the
    /// terminating zero bit (if fewer than `max` ones were found).
    #[inline]
    pub(crate) fn read_unary(&mut self, max: u32) -> Result<u32> {
        let mut ones = 0;
        while ones < max {
            if !self.read_bit()? {
                return Ok(ones);
            }
            ones += 1;
        }
        Ok(ones)
    }

    /// Checks that only zero padding bits of the final byte remain.
    pub(crate) fn finish(&self) -> Result<()> {
        if ceil8(self.pos) != self.data.len() {
            return Err(Error::corrupted("trailing bytes after bit stream"));
        }
        if !self.pos.is_multiple_of(8) {
            let last = self.data[self.data.len() - 1];
            if last >> (self.pos % 8) != 0 {
                return Err(Error::corrupted("non-zero padding bits in bit stream"));
            }
        }
        Ok(())
    }
}

/// Number of bits needed to represent `v` (0 for 0).
#[inline]
pub(crate) fn bit_width(v: u64) -> u32 {
    64 - v.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn lsb_first_layout() {
        let mut w = BitWriter::new();
        w.write_bit(true);
        w.write_bit(false);
        w.write(0b11, 2);
        assert_eq!(w.finish(), vec![0b0000_1101]);
    }

    #[test]
    fn wide_fields_cross_words() {
        let mut w = BitWriter::new();
        w.write(0b101, 3);
        w.write(u64::MAX, 64);
        w.write(0x1234_5678_9abc_def0, 64);
        w.write(1, 1);
        let bytes = w.finish();
        assert_eq!(bytes.len(), ceil8(3 + 64 + 64 + 1));
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read(64).unwrap(), u64::MAX);
        assert_eq!(r.read(64).unwrap(), 0x1234_5678_9abc_def0);
        assert_eq!(r.read(1).unwrap(), 1);
        r.finish().unwrap();
    }

    #[test]
    fn reading_past_the_end_fails() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(r.read(8).unwrap(), 0xff);
        assert!(r.read(1).is_err());
    }

    #[test]
    fn padding_must_be_zero() {
        let mut r = BitReader::new(&[0b1000_0001]);
        assert!(r.read_bit().unwrap());
        assert!(r.finish().is_err());
    }

    #[test]
    fn unary_codes() {
        let mut w = BitWriter::new();
        for ones in [0u32, 1, 3, 4] {
            for _ in 0..ones.min(4) {
                w.write_bit(true);
            }
            if ones < 4 {
                w.write_bit(false);
            }
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_unary(4).unwrap(), 0);
        assert_eq!(r.read_unary(4).unwrap(), 1);
        assert_eq!(r.read_unary(4).unwrap(), 3);
        assert_eq!(r.read_unary(4).unwrap(), 4);
    }

    proptest! {
        #[test]
        fn fields_round_trip(fields in proptest::collection::vec((any::<u64>(), 0u32..=64), 0..200)) {
            let mut w = BitWriter::new();
            for &(v, n) in &fields {
                w.write(v, n);
            }
            let bytes = w.finish();
            let total: usize = fields.iter().map(|&(_, n)| n as usize).sum();
            prop_assert_eq!(bytes.len(), ceil8(total));
            let mut r = BitReader::new(&bytes);
            for &(v, n) in &fields {
                prop_assert_eq!(r.read(n).unwrap(), low_bits(v, n));
            }
            r.finish().unwrap();
        }
    }
}
