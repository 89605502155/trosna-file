//! Positioned reads, so that a reader can be shared between threads.

use std::fs::File;
use std::io;

use crate::error::{Error, Result};

/// Reads exactly `buf.len()` bytes at `offset`.
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            let n = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut buf[done..],
                offset + done as u64,
            )?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = file;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }
}

/// Reads `len` bytes at `offset`, reporting a short file as corruption.
///
/// The length usually comes from the file itself, so it is checked against the
/// file size before any memory is allocated: a corrupted length field must not
/// make the reader reserve up to a gigabyte.
pub(crate) fn read_vec_at(file: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let size = file.metadata()?.len();
    if offset.checked_add(len as u64).is_none_or(|end| end > size) {
        return Err(Error::corrupted("data extends beyond the end of the file").at(offset));
    }
    let mut buf = vec![0u8; len];
    read_exact_at(file, &mut buf, offset).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::corrupted("data extends beyond the end of the file").at(offset)
        } else {
            Error::Io(e)
        }
    })?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn lengths_beyond_the_file_are_refused_without_allocating() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"0123456789").unwrap();
        assert_eq!(read_vec_at(&file, 2, 8).unwrap(), b"23456789");
        for (offset, len) in [(2, 9), (0, 1 << 30), (u64::MAX, 1), (11, 0)] {
            let err = read_vec_at(&file, offset, len).unwrap_err();
            assert!(matches!(err, Error::Corrupted { .. }), "{err}");
        }
        assert!(read_vec_at(&file, 10, 0).unwrap().is_empty());
    }
}
