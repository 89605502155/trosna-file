//! String encodings (SPEC §6.1, §6.6).

use std::collections::{HashMap, HashSet};

use crate::bits::{BitReader, BitWriter, bit_width};
use crate::bytes::{ByteReader, PutBytes, ceil8};
use crate::column::Utf8Values;
use crate::error::{Error, Result};
use crate::segment::MAX_RAW_SEGMENT;

pub(crate) fn plain_encode<'a>(values: impl Iterator<Item = &'a str>, out: &mut Vec<u8>) {
    for s in values {
        out.put_str(s);
    }
}

pub(crate) fn plain_decode(data: &[u8], n: usize) -> Result<Utf8Values> {
    let mut r = ByteReader::new(data);
    // Each string needs at least one length byte, which bounds `n`.
    if n > data.len() {
        return Err(Error::corrupted(
            "string segment is too short for its count",
        ));
    }
    let mut out = Utf8Values::with_capacity(n, data.len().saturating_sub(n));
    for _ in 0..n {
        out.push(r.str()?)
            .map_err(|e| Error::corrupted(e.to_string()))?;
    }
    r.finish("plain strings")?;
    Ok(out)
}

pub(crate) fn dictionary_encode(values: &[&str], out: &mut Vec<u8>) {
    if values.is_empty() {
        return;
    }
    let mut ids: HashMap<&str, u64> = HashMap::new();
    let mut entries: Vec<&str> = Vec::new();
    let indices: Vec<u64> = values
        .iter()
        .map(|&s| {
            *ids.entry(s).or_insert_with(|| {
                entries.push(s);
                entries.len() as u64 - 1
            })
        })
        .collect();
    out.put_uvarint(entries.len() as u64);
    for e in &entries {
        out.put_str(e);
    }
    let width = bit_width(entries.len() as u64 - 1);
    let mut bits = BitWriter::with_capacity_bits(values.len() * width as usize);
    for i in indices {
        bits.write(i, width);
    }
    out.extend_from_slice(&bits.finish());
}

pub(crate) fn dictionary_decode(data: &[u8], n: usize) -> Result<Utf8Values> {
    let mut r = ByteReader::new(data);
    if n == 0 {
        r.finish("empty dictionary segment")?;
        return Ok(Utf8Values::new());
    }
    let size = r.len(n, "dictionary size")?;
    if size == 0 {
        return Err(Error::corrupted("empty dictionary for a non-empty segment"));
    }
    let mut entries = Vec::with_capacity(size);
    let mut seen = HashSet::with_capacity(size);
    for _ in 0..size {
        let s = r.str()?;
        if !seen.insert(s) {
            return Err(Error::corrupted("duplicate dictionary entry"));
        }
        entries.push(s);
    }
    let width = bit_width(size as u64 - 1);
    let packed = r.bytes(ceil8(n * width as usize))?;
    r.finish("dictionary indices")?;
    // A long entry repeated by a few index bits could expand a tiny segment
    // into gigabytes, so the size of the result is computed and checked
    // against the segment limit before anything is allocated.
    let mut bits = BitReader::new(packed);
    let mut total = 0usize;
    for _ in 0..n {
        let index = bits.read(width)? as usize;
        let entry = entries
            .get(index)
            .ok_or_else(|| Error::corrupted("dictionary index out of range"))?;
        total += entry.len();
        if total > MAX_RAW_SEGMENT {
            return Err(Error::corrupted(format!(
                "dictionary segment expands beyond {MAX_RAW_SEGMENT} bytes"
            )));
        }
    }
    bits.finish()?;
    let mut bits = BitReader::new(packed);
    let mut out = Utf8Values::with_capacity(n, total);
    for _ in 0..n {
        let entry = entries[bits.read(width)? as usize];
        out.push(entry)
            .map_err(|e| Error::corrupted(e.to_string()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn dictionary_bombs_are_refused_before_allocating() {
        // one 1 MiB entry, zero-width indices, 2^24 rows: 16 TiB of strings
        let mut data = Vec::new();
        data.put_uvarint(1);
        data.put_str(&"x".repeat(1 << 20));
        let err = dictionary_decode(&data, 1 << 24).unwrap_err();
        assert!(err.to_string().contains("expands beyond"), "{err}");
        assert_eq!(dictionary_decode(&data, 3).unwrap().len(), 3);
    }

    #[test]
    fn dictionary_is_compact_for_repeated_values() {
        let values: Vec<&str> = (0..1000)
            .map(|i| {
                if i % 3 == 0 {
                    "работает"
                } else {
                    "idle"
                }
            })
            .collect();
        let mut dict = Vec::new();
        dictionary_encode(&values, &mut dict);
        let mut plain = Vec::new();
        plain_encode(values.iter().copied(), &mut plain);
        assert!(dict.len() * 20 < plain.len());
        let decoded = dictionary_decode(&dict, values.len()).unwrap();
        assert_eq!(decoded.iter().collect::<Vec<_>>(), values);
    }

    #[test]
    fn single_entry_dictionary_uses_zero_bits() {
        let values = vec!["x"; 500];
        let mut out = Vec::new();
        dictionary_encode(&values, &mut out);
        assert_eq!(out, vec![1, 1, b'x']);
        assert_eq!(dictionary_decode(&out, 500).unwrap().len(), 500);
    }

    #[test]
    fn invalid_dictionaries_are_rejected() {
        // two equal entries
        assert!(dictionary_decode(&[2, 1, b'a', 1, b'a', 0], 1).is_err());
        // three entries need two-bit indices; index 3 is out of range
        assert!(dictionary_decode(&[3, 1, b'a', 1, b'b', 1, b'c', 0b11], 1).is_err());
        assert!(plain_decode(&[5, b'a'], 1).is_err());
    }

    proptest! {
        #[test]
        fn round_trip(values in proptest::collection::vec("[a-cя]{0,4}", 0..200)) {
            let refs: Vec<&str> = values.iter().map(String::as_str).collect();
            let mut out = Vec::new();
            dictionary_encode(&refs, &mut out);
            let decoded = dictionary_decode(&out, refs.len()).unwrap();
            prop_assert_eq!(decoded.iter().collect::<Vec<_>>(), refs.clone());
            let mut out = Vec::new();
            plain_encode(refs.iter().copied(), &mut out);
            let decoded = plain_decode(&out, refs.len()).unwrap();
            prop_assert_eq!(decoded.iter().collect::<Vec<_>>(), refs);
        }

        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..64), n in 0usize..100) {
            let _ = plain_decode(&data, n);
            let _ = dictionary_decode(&data, n);
        }
    }
}
