//! Bounds-checked reading and writing of the primitive types of the format
//! (SPEC §1): fixed-width little-endian integers, LEB128 varints, strings and
//! key-value maps.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// Maps a signed integer to an unsigned one so that small magnitudes stay small.
#[inline]
pub(crate) fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

/// Inverse of [`zigzag`].
#[inline]
pub(crate) fn unzigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// A cursor over a byte slice whose every read is bounds-checked.
#[derive(Debug, Clone)]
pub(crate) struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        ByteReader { buf, pos: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Fails unless the whole input has been consumed.
    pub(crate) fn finish(&self, what: &str) -> Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(Error::corrupted(format!(
                "{} unexpected trailing bytes after {what}",
                self.remaining()
            )))
        }
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(Error::corrupted(format!(
                "unexpected end of data: need {n} bytes, {} left",
                self.remaining()
            )));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// Reads an unsigned LEB128 integer (at most 10 bytes, no overflow).
    pub(crate) fn uvarint(&mut self) -> Result<u64> {
        let mut value: u64 = 0;
        for i in 0..10 {
            let byte = self.u8()?;
            let part = u64::from(byte & 0x7f);
            if i == 9 && part > 1 {
                return Err(Error::corrupted("varint overflows 64 bits"));
            }
            value |= part << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::corrupted("varint longer than 10 bytes"))
    }

    pub(crate) fn svarint(&mut self) -> Result<i64> {
        Ok(unzigzag(self.uvarint()?))
    }

    /// Reads a varint that must fit into `u32`.
    pub(crate) fn uvarint_u32(&mut self) -> Result<u32> {
        let v = self.uvarint()?;
        u32::try_from(v)
            .map_err(|_| Error::corrupted(format!("value {v} does not fit into 32 bits")))
    }

    /// Reads a varint used as a length or count, rejecting values above `max`.
    pub(crate) fn len(&mut self, max: usize, what: &str) -> Result<usize> {
        let v = self.uvarint()?;
        match usize::try_from(v) {
            Ok(n) if n <= max => Ok(n),
            _ => Err(Error::corrupted(format!(
                "{what} {v} exceeds the limit {max}"
            ))),
        }
    }

    pub(crate) fn str(&mut self) -> Result<&'a str> {
        let n = self.len(self.remaining(), "string length")?;
        let raw = self.bytes(n)?;
        std::str::from_utf8(raw).map_err(|_| Error::corrupted("string is not valid UTF-8"))
    }

    pub(crate) fn string(&mut self) -> Result<String> {
        Ok(self.str()?.to_owned())
    }

    pub(crate) fn opt_string(&mut self) -> Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.string()?)),
            flag => Err(Error::corrupted(format!(
                "invalid optional-string flag {flag}"
            ))),
        }
    }

    pub(crate) fn kv_map(&mut self) -> Result<BTreeMap<String, String>> {
        // Every pair needs at least two bytes, which bounds the count.
        let count = self.len(self.remaining() / 2, "metadata entry count")?;
        let mut map = BTreeMap::new();
        for _ in 0..count {
            let key = self.string()?;
            let value = self.string()?;
            if map.insert(key, value).is_some() {
                return Err(Error::corrupted("duplicate metadata key"));
            }
        }
        Ok(map)
    }
}

/// Append-only writing of the primitive types of the format.
pub(crate) trait PutBytes {
    fn put_u8(&mut self, v: u8);
    fn put_u32(&mut self, v: u32);
    fn put_u64(&mut self, v: u64);
    fn put_i64(&mut self, v: i64);
    fn put_uvarint(&mut self, v: u64);
    fn put_svarint(&mut self, v: i64);
    fn put_str(&mut self, s: &str);
    fn put_opt_str(&mut self, s: Option<&str>);
    fn put_kv_map(&mut self, map: &BTreeMap<String, String>);
}

impl PutBytes for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }

    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn put_i64(&mut self, v: i64) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn put_uvarint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.push(v as u8);
    }

    fn put_svarint(&mut self, v: i64) {
        self.put_uvarint(zigzag(v));
    }

    fn put_str(&mut self, s: &str) {
        self.put_uvarint(s.len() as u64);
        self.extend_from_slice(s.as_bytes());
    }

    fn put_opt_str(&mut self, s: Option<&str>) {
        match s {
            None => self.put_u8(0),
            Some(s) => {
                self.put_u8(1);
                self.put_str(s);
            }
        }
    }

    fn put_kv_map(&mut self, map: &BTreeMap<String, String>) {
        self.put_uvarint(map.len() as u64);
        for (k, v) in map {
            self.put_str(k);
            self.put_str(v);
        }
    }
}

/// `ceil(n / 8)`.
#[inline]
pub(crate) fn ceil8(n: usize) -> usize {
    n.div_ceil(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn varint_known_values() {
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (300, &[0xac, 0x02]),
            (
                u64::MAX,
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            ),
        ];
        for &(value, bytes) in cases {
            let mut out = Vec::new();
            out.put_uvarint(value);
            assert_eq!(out, bytes, "encoding {value}");
            let mut r = ByteReader::new(bytes);
            assert_eq!(r.uvarint().unwrap(), value);
            assert!(r.is_empty());
        }
    }

    #[test]
    fn varint_rejects_overflow_and_overlong() {
        let overflow = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert!(ByteReader::new(&overflow).uvarint().is_err());
        let overlong = [0x80; 11];
        assert!(ByteReader::new(&overlong).uvarint().is_err());
        assert!(ByteReader::new(&[0x80]).uvarint().is_err());
    }

    #[test]
    fn zigzag_known_values() {
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(i64::MAX), u64::MAX - 1);
        assert_eq!(zigzag(i64::MIN), u64::MAX);
    }

    #[test]
    fn strings_and_maps_round_trip() {
        let mut map = BTreeMap::new();
        map.insert("единица".to_owned(), "МБ".to_owned());
        map.insert("source".to_owned(), String::new());
        let mut out = Vec::new();
        out.put_str("устройство");
        out.put_opt_str(None);
        out.put_opt_str(Some("x"));
        out.put_kv_map(&map);
        let mut r = ByteReader::new(&out);
        assert_eq!(r.str().unwrap(), "устройство");
        assert_eq!(r.opt_string().unwrap(), None);
        assert_eq!(r.opt_string().unwrap().as_deref(), Some("x"));
        assert_eq!(r.kv_map().unwrap(), map);
        r.finish("test").unwrap();
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let data = [2u8, 0xc3, 0x28];
        assert!(ByteReader::new(&data).str().is_err());
    }

    proptest! {
        #[test]
        fn varint_round_trip(v in any::<u64>()) {
            let mut out = Vec::new();
            out.put_uvarint(v);
            let mut r = ByteReader::new(&out);
            prop_assert_eq!(r.uvarint().unwrap(), v);
            prop_assert!(r.is_empty());
        }

        #[test]
        fn svarint_round_trip(v in any::<i64>()) {
            let mut out = Vec::new();
            out.put_svarint(v);
            prop_assert_eq!(ByteReader::new(&out).svarint().unwrap(), v);
        }

        #[test]
        fn reader_never_panics(data in proptest::collection::vec(any::<u8>(), 0..64)) {
            let mut r = ByteReader::new(&data);
            let _ = r.uvarint();
            let _ = r.str();
            let _ = r.kv_map();
            let _ = r.opt_string();
            let _ = r.i64();
        }
    }
}
