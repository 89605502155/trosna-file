//! Building the catalogue: from the index of a finalized file, or by
//! scanning the frames of a file whose writer did not finish (SPEC §9).

use std::fs::File;

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::fileio::{read_exact_at, read_vec_at};
use crate::format::{
    self, FILE_HEADER_LEN, FOOTER_FRAME_LEN, FRAME_HEADER_LEN, FRAME_OVERHEAD, FrameHeader,
    FrameKind, SYNC,
};

/// Why a scan stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The last frame ends exactly at the end of the file.
    EndOfFile,
    /// The last frame is incomplete.
    Truncated,
    /// An invalid frame was found.
    Invalid(String),
}

/// Result of a scan.
#[derive(Debug)]
pub(crate) struct ScanOutcome {
    /// Committed frames only.
    pub(crate) catalog: Catalog,
    /// End of the last valid `Commit` frame (or of the file header).
    pub(crate) committed_end: u64,
    /// Offset at which the scan stopped.
    pub(crate) stopped_at: u64,
    pub(crate) stop: Stop,
    /// Valid frames after the last commit, which were discarded.
    pub(crate) discarded_frames: u32,
    /// Offset of the last `Footer` frame seen and whether it was the last frame.
    pub(crate) ends_with_footer: bool,
}

/// A frame visited by a full scan.
#[derive(Debug)]
pub(crate) struct FrameEvent<'a> {
    pub(crate) offset: u64,
    pub(crate) kind: FrameKind,
    /// The complete frame bytes (sync … crc).
    pub(crate) bytes: &'a [u8],
}

fn read_frame_header(file: &File, offset: u64) -> Result<FrameHeader> {
    let mut buf = [0u8; FRAME_HEADER_LEN];
    read_exact_at(file, &mut buf, offset)?;
    format::parse_frame_header(&buf)
}

/// Reads the payload of a frame and checks its CRC.
fn read_checked_payload(file: &File, offset: u64, header: FrameHeader) -> Result<Vec<u8>> {
    let mut frame = read_vec_at(file, offset, header.frame_len() as usize)?;
    check_frame_bytes(&frame)?;
    frame.truncate(frame.len() - 4);
    frame.drain(..FRAME_HEADER_LEN);
    Ok(frame)
}

/// Checks the CRC of complete frame bytes.
pub(crate) fn check_frame_bytes(frame: &[u8]) -> Result<()> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    header.copy_from_slice(&frame[..FRAME_HEADER_LEN]);
    let payload = &frame[FRAME_HEADER_LEN..frame.len() - 4];
    let stored = &frame[frame.len() - 4..];
    let crc = u32::from_le_bytes([stored[0], stored[1], stored[2], stored[3]]);
    if format::frame_crc(&header, payload) != crc {
        return Err(Error::corrupted("frame checksum mismatch"));
    }
    Ok(())
}

/// Scans the frames of a file from the header onwards.
///
/// With `full`, every frame is read completely and its checksum verified, and
/// `visit` is called for each valid frame; otherwise only the parts needed to
/// build the catalogue are read.
pub(crate) fn scan(
    file: &File,
    len: u64,
    full: bool,
    mut visit: impl FnMut(FrameEvent<'_>) -> Result<()>,
) -> Result<ScanOutcome> {
    let mut catalog = Catalog::new();
    let mut pos = FILE_HEADER_LEN;
    let mut committed_end = FILE_HEADER_LEN;
    let mut last_kind = None;
    let stop = loop {
        if pos == len {
            break Stop::EndOfFile;
        }
        if len - pos < FRAME_OVERHEAD {
            break Stop::Truncated;
        }
        let header = match read_frame_header(file, pos) {
            Ok(h) => h,
            Err(Error::Corrupted { reason, .. }) => break Stop::Invalid(reason),
            Err(e) => return Err(e),
        };
        if pos + header.frame_len() > len {
            break Stop::Truncated;
        }
        let (kind, body) = match (FrameKind::from_code(header.kind).ok(), full) {
            (Some(FrameKind::Data), false) => {
                if header.len < 4 {
                    break Stop::Invalid("Data frame too short".into());
                }
                let mut len_bytes = [0u8; 4];
                read_exact_at(file, &mut len_bytes, pos + FRAME_HEADER_LEN as u64)?;
                let header_len = u64::from(u32::from_le_bytes(len_bytes));
                if header_len + 4 > u64::from(header.len) {
                    break Stop::Invalid("Data frame header exceeds the frame".into());
                }
                let body =
                    read_vec_at(file, pos + FRAME_HEADER_LEN as u64 + 4, header_len as usize)?;
                (FrameKind::Data, body)
            }
            (Some(kind @ (FrameKind::Index | FrameKind::Footer)), false) => (kind, Vec::new()),
            _ => {
                let frame = read_vec_at(file, pos, header.frame_len() as usize)?;
                if let Err(Error::Corrupted { reason, .. }) = check_frame_bytes(&frame) {
                    break Stop::Invalid(reason);
                }
                // The frame is intact, so an unknown critical kind is a real incompatibility.
                let kind = FrameKind::from_code(header.kind).map_err(|e| e.at(pos))?;
                visit(FrameEvent {
                    offset: pos,
                    kind,
                    bytes: &frame,
                })?;
                let payload = &frame[FRAME_HEADER_LEN..frame.len() - 4];
                let body = if kind == FrameKind::Data {
                    if payload.len() < 4 {
                        return Err(Error::corrupted("Data frame too short").at(pos));
                    }
                    let header_len =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
                            as usize;
                    if header_len + 4 > payload.len() {
                        return Err(Error::corrupted("Data frame header exceeds the frame").at(pos));
                    }
                    payload[4..4 + header_len].to_vec()
                } else {
                    payload.to_vec()
                };
                (kind, body)
            }
        };
        if !kind.is_derived() {
            catalog.apply(kind, pos, &body, Some(u64::from(header.len)))?;
        }
        pos += header.frame_len();
        if kind == FrameKind::Commit {
            committed_end = pos;
        }
        last_kind = Some(kind);
    };
    let discarded_frames = catalog.pending_frames();
    catalog.truncate(committed_end);
    Ok(ScanOutcome {
        catalog,
        committed_end,
        stopped_at: pos,
        ends_with_footer: stop == Stop::EndOfFile && last_kind == Some(FrameKind::Footer),
        stop,
        discarded_frames,
    })
}

/// Loads the catalogue from the footer and index of a finalized file.
/// Returns `Ok(None)` if the file does not end with a valid footer.
pub(crate) fn load_index(file: &File, len: u64) -> Result<Option<Catalog>> {
    if len < FILE_HEADER_LEN + FOOTER_FRAME_LEN {
        return Ok(None);
    }
    let footer_offset = len - FOOTER_FRAME_LEN;
    let footer = read_vec_at(file, footer_offset, FOOTER_FRAME_LEN as usize)?;
    let mut header = [0u8; FRAME_HEADER_LEN];
    header.copy_from_slice(&footer[..FRAME_HEADER_LEN]);
    match format::parse_frame_header(&header) {
        Ok(h) if h.kind == FrameKind::Footer.code() && h.len == 8 => {}
        _ => return Ok(None),
    }
    if check_frame_bytes(&footer).is_err() {
        return Ok(None);
    }
    let index_offset = format::decode_footer(&footer[FRAME_HEADER_LEN..FRAME_HEADER_LEN + 8])?;
    let bad = |reason: &str| Error::corrupted(reason.to_owned()).at(footer_offset);
    if index_offset < FILE_HEADER_LEN || index_offset + FRAME_OVERHEAD > footer_offset {
        return Err(bad("the footer points outside the file"));
    }
    let index_header = read_frame_header(file, index_offset).map_err(|e| e.at(index_offset))?;
    if index_header.kind != FrameKind::Index.code()
        || index_offset + index_header.frame_len() > footer_offset
    {
        return Err(bad("the footer does not point to an index"));
    }
    let payload =
        read_checked_payload(file, index_offset, index_header).map_err(|e| e.at(index_offset))?;
    let (head, head_hash, entries) =
        format::decode_index(&payload).map_err(|e| e.at(index_offset))?;
    if entries.iter().any(|e| e.offset >= index_offset) {
        return Err(bad("the index lists frames after itself"));
    }
    let catalog = Catalog::from_index(&entries)?;
    if catalog.head() != (head, head_hash) {
        return Err(bad("the index head does not match its commits"));
    }
    Ok(Some(catalog))
}

/// Looks for a valid frame starting after `from`. Used to tell an interrupted
/// write (garbage only at the end) from corruption in the middle of a file.
pub(crate) fn find_valid_frame_after(file: &File, len: u64, from: u64) -> Result<Option<u64>> {
    const WINDOW: u64 = 1 << 16;
    let mut start = from + 1;
    while start + FRAME_OVERHEAD <= len {
        let n = WINDOW.min(len - start) as usize;
        let buf = read_vec_at(file, start, n)?;
        for i in 0..buf.len().saturating_sub(3) {
            if &buf[i..i + 4] != SYNC {
                continue;
            }
            let pos = start + i as u64;
            let Ok(header) = read_frame_header(file, pos) else {
                continue;
            };
            if pos + header.frame_len() > len {
                continue;
            }
            let frame = read_vec_at(file, pos, header.frame_len() as usize)?;
            if check_frame_bytes(&frame).is_ok() {
                return Ok(Some(pos));
            }
        }
        // Overlap windows so that a marker split across them is not missed.
        start += (n as u64).saturating_sub(3).max(1);
    }
    Ok(None)
}
