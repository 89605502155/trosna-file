//! Boolean encodings (SPEC §6.1, §6.4).

use crate::bytes::{ByteReader, PutBytes, ceil8};
use crate::column::Bitmap;
use crate::error::{Error, Result};

pub(crate) fn plain_encode(values: &Bitmap, out: &mut Vec<u8>) {
    out.extend_from_slice(values.as_bytes());
}

pub(crate) fn plain_decode(data: &[u8], n: usize) -> Result<Bitmap> {
    if data.len() != ceil8(n) {
        return Err(Error::corrupted(format!(
            "boolean bitmap has {} bytes for {n} values",
            data.len()
        )));
    }
    if !n.is_multiple_of(8) && data[data.len() - 1] >> (n % 8) != 0 {
        return Err(Error::corrupted("non-zero padding bits in boolean bitmap"));
    }
    Bitmap::from_bytes(data.to_vec(), n).map_err(|e| Error::corrupted(e.to_string()))
}

pub(crate) fn rle_encode(values: &Bitmap, out: &mut Vec<u8>) {
    let mut iter = values.iter().peekable();
    while let Some(v) = iter.next() {
        let mut run = 1u64;
        while iter.peek() == Some(&v) {
            iter.next();
            run += 1;
        }
        out.put_uvarint(run);
        out.put_u8(u8::from(v));
    }
}

pub(crate) fn rle_decode(data: &[u8], n: usize) -> Result<Bitmap> {
    let mut r = ByteReader::new(data);
    let mut out = Bitmap::with_capacity(super::decode_capacity(n, data.len()));
    while out.len() < n {
        let run = r.len(n - out.len(), "run length")?;
        if run == 0 {
            return Err(Error::corrupted("zero-length run"));
        }
        let v = match r.u8()? {
            0 => false,
            1 => true,
            other => return Err(Error::corrupted(format!("invalid boolean byte {other}"))),
        };
        for _ in 0..run {
            out.push(v);
        }
    }
    r.finish("rle booleans")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn padding_is_checked() {
        assert!(plain_decode(&[0b0000_0101], 3).is_ok());
        assert!(plain_decode(&[0b1000_0101], 3).is_err());
        assert!(plain_decode(&[0, 0], 3).is_err());
        assert!(rle_decode(&[1, 2], 1).is_err());
    }

    proptest! {
        #[test]
        fn round_trip(values in proptest::collection::vec(any::<bool>(), 0..300)) {
            let bitmap: Bitmap = values.iter().copied().collect();
            let mut out = Vec::new();
            plain_encode(&bitmap, &mut out);
            prop_assert_eq!(&plain_decode(&out, values.len()).unwrap(), &bitmap);
            let mut out = Vec::new();
            rle_encode(&bitmap, &mut out);
            prop_assert_eq!(&rle_decode(&out, values.len()).unwrap(), &bitmap);
        }

        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..64), n in 0usize..200) {
            let _ = plain_decode(&data, n);
            let _ = rle_decode(&data, n);
        }
    }
}
