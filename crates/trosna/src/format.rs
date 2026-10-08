//! The physical layout: file header, frames and frame payloads (SPEC §2–§8).

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::bytes::{ByteReader, PutBytes};
use crate::error::{Error, Result};
use crate::segment::SegmentHeader;
use crate::types::{ColumnSchema, DataType, DeviceSchema, MAX_COLUMNS, TimeUnit};

/// Magic bytes at the start of every file.
pub(crate) const MAGIC: &[u8; 6] = b"TROSNA";
/// Major version written by this library.
pub(crate) const VERSION_MAJOR: u8 = 1;
/// Minor version written by this library.
pub(crate) const VERSION_MINOR: u8 = 0;
/// Size of the file header.
pub(crate) const FILE_HEADER_LEN: u64 = 8;
/// Synchronisation marker at the start of every frame.
pub(crate) const SYNC: &[u8; 4] = b"TBLK";
/// Size of a frame header (sync, kind, flags, reserved, length).
pub(crate) const FRAME_HEADER_LEN: usize = 12;
/// Frame header plus trailing CRC.
pub(crate) const FRAME_OVERHEAD: u64 = 16;
/// Largest allowed frame payload.
pub(crate) const MAX_PAYLOAD: u32 = 1 << 30;
/// Total size of a `Footer` frame.
pub(crate) const FOOTER_FRAME_LEN: u64 = FRAME_OVERHEAD + 8;
/// Largest number of rows in one `Data` frame.
pub(crate) const MAX_ROWS: u32 = 1 << 24;
/// Size of a SHA-256 hash.
pub(crate) const HASH_LEN: usize = 32;

/// A SHA-256 hash.
pub type Hash = [u8; HASH_LEN];

/// Kind of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FrameKind {
    Meta,
    Device,
    Data,
    Tombstone,
    Annotation,
    Commit,
    Index,
    Footer,
    /// An ancillary frame (high bit set) that readers may ignore.
    Ancillary(u8),
}

impl FrameKind {
    pub(crate) fn code(self) -> u8 {
        match self {
            FrameKind::Meta => 0x01,
            FrameKind::Device => 0x02,
            FrameKind::Data => 0x03,
            FrameKind::Tombstone => 0x04,
            FrameKind::Annotation => 0x05,
            FrameKind::Commit => 0x06,
            FrameKind::Index => 0x07,
            FrameKind::Footer => 0x08,
            FrameKind::Ancillary(code) => code,
        }
    }

    pub(crate) fn from_code(code: u8) -> Result<Self> {
        Ok(match code {
            0x01 => FrameKind::Meta,
            0x02 => FrameKind::Device,
            0x03 => FrameKind::Data,
            0x04 => FrameKind::Tombstone,
            0x05 => FrameKind::Annotation,
            0x06 => FrameKind::Commit,
            0x07 => FrameKind::Index,
            0x08 => FrameKind::Footer,
            c if c & 0x80 != 0 => FrameKind::Ancillary(c),
            c => return Err(Error::Unsupported(format!("critical frame kind {c:#04x}"))),
        })
    }

    /// Derived frames are not part of commits (SPEC §3).
    pub(crate) fn is_derived(self) -> bool {
        matches!(self, FrameKind::Index | FrameKind::Footer)
    }
}

/// The 8-byte file header.
pub(crate) fn file_header() -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..6].copy_from_slice(MAGIC);
    h[6] = VERSION_MAJOR;
    h[7] = VERSION_MINOR;
    h
}

/// Validates a file header and returns `(major, minor)`.
pub(crate) fn check_file_header(bytes: &[u8]) -> Result<(u8, u8)> {
    if bytes.len() < FILE_HEADER_LEN as usize || &bytes[..6] != MAGIC {
        return Err(Error::NotTrosna);
    }
    let (major, minor) = (bytes[6], bytes[7]);
    if major != VERSION_MAJOR {
        return Err(Error::UnsupportedVersion { major, minor });
    }
    Ok((major, minor))
}

/// CRC32C of a frame: over the header bytes after the sync marker and the payload.
pub(crate) fn frame_crc(header: &[u8; FRAME_HEADER_LEN], payload: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(&header[4..]), payload)
}

/// Serialises a complete frame.
pub(crate) fn encode_frame(kind: FrameKind, payload: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|&l| l <= MAX_PAYLOAD)
        .ok_or_else(|| {
            Error::LimitExceeded(format!("a frame of {} bytes exceeds 1 GiB", payload.len()))
        })?;
    let mut header = [0u8; FRAME_HEADER_LEN];
    header[..4].copy_from_slice(SYNC);
    header[4] = kind.code();
    header[8..].copy_from_slice(&len.to_le_bytes());
    let mut frame = Vec::with_capacity(payload.len() + FRAME_OVERHEAD as usize);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    frame.put_u32(frame_crc(&header, payload));
    Ok(frame)
}

/// The fixed part of a frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameHeader {
    pub(crate) kind: u8,
    pub(crate) len: u32,
}

impl FrameHeader {
    /// Total size of the frame including header and CRC.
    pub(crate) fn frame_len(self) -> u64 {
        u64::from(self.len) + FRAME_OVERHEAD
    }
}

/// Parses and validates a frame header (not the CRC).
pub(crate) fn parse_frame_header(bytes: &[u8; FRAME_HEADER_LEN]) -> Result<FrameHeader> {
    if &bytes[..4] != SYNC {
        return Err(Error::corrupted("missing frame synchronisation marker"));
    }
    if bytes[5] != 0 || bytes[6] != 0 || bytes[7] != 0 {
        return Err(Error::corrupted("non-zero reserved bytes in frame header"));
    }
    let len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if len > MAX_PAYLOAD {
        return Err(Error::corrupted(format!(
            "frame length {len} exceeds the limit"
        )));
    }
    Ok(FrameHeader {
        kind: bytes[4],
        len,
    })
}

// ---------------------------------------------------------------- Meta

pub(crate) fn encode_meta(metadata: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = Vec::new();
    out.put_kv_map(metadata);
    out
}

pub(crate) fn decode_meta(payload: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut r = ByteReader::new(payload);
    let map = r.kv_map()?;
    r.finish("Meta frame")?;
    Ok(map)
}

// ---------------------------------------------------------------- Device

pub(crate) fn encode_device(id: u32, schema: &DeviceSchema) -> Vec<u8> {
    let mut out = Vec::new();
    out.put_uvarint(u64::from(id));
    out.put_str(&schema.name);
    out.put_u8(schema.time_unit.code());
    out.put_opt_str(schema.timezone.as_deref());
    out.put_str(&schema.time_name);
    out.put_uvarint(schema.columns.len() as u64);
    for column in &schema.columns {
        out.put_str(&column.name);
        out.put_u8(column.data_type.code());
        out.put_kv_map(&column.metadata);
    }
    out.put_kv_map(&schema.metadata);
    out
}

pub(crate) fn decode_device(payload: &[u8]) -> Result<(u32, DeviceSchema)> {
    let mut r = ByteReader::new(payload);
    let id = r.uvarint_u32()?;
    let name = r.string()?;
    let time_unit = TimeUnit::from_code(r.u8()?)?;
    let timezone = r.opt_string()?;
    let time_name = r.string()?;
    let count = r.len(MAX_COLUMNS, "column count")?;
    let mut columns = Vec::with_capacity(count.min(r.remaining()));
    for _ in 0..count {
        let name = r.string()?;
        let data_type = DataType::from_code(r.u8()?)?;
        let metadata = r.kv_map()?;
        columns.push(ColumnSchema {
            name,
            data_type,
            metadata,
        });
    }
    let metadata = r.kv_map()?;
    r.finish("Device frame")?;
    let schema = DeviceSchema {
        name,
        time_unit,
        timezone,
        time_name,
        columns,
        metadata,
    };
    schema
        .validate()
        .map_err(|e| Error::corrupted(e.to_string()))?;
    Ok((id, schema))
}

// ---------------------------------------------------------------- Data

/// Header of a `Data` frame.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DataHeader {
    pub(crate) device_id: u32,
    pub(crate) row_count: u32,
    pub(crate) t_min: i64,
    pub(crate) t_max: i64,
    /// Time segment first, then the value columns in schema order.
    pub(crate) segments: Vec<SegmentHeader>,
}

impl DataHeader {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.put_uvarint(u64::from(self.device_id));
        out.put_uvarint(u64::from(self.row_count));
        out.put_i64(self.t_min);
        out.put_i64(self.t_max);
        out.put_uvarint(self.segments.len() as u64);
        for s in &self.segments {
            s.write(&mut out);
        }
        out
    }

    /// Parses a header; `column_types` maps a device id to its value column types.
    pub(crate) fn decode<'t>(
        bytes: &[u8],
        column_types: impl FnOnce(u32) -> Result<&'t [DataType]>,
    ) -> Result<Self> {
        let mut r = ByteReader::new(bytes);
        let device_id = r.uvarint_u32()?;
        let types = column_types(device_id)?;
        let row_count = r.uvarint_u32()?;
        if row_count == 0 || row_count > MAX_ROWS {
            return Err(Error::corrupted(format!("invalid row count {row_count}")));
        }
        let t_min = r.i64()?;
        let t_max = r.i64()?;
        if t_min > t_max || (row_count == 1 && t_min != t_max) || (row_count > 1 && t_min == t_max)
        {
            return Err(Error::corrupted("inconsistent time range of a Data frame"));
        }
        let count = r.len(MAX_COLUMNS + 1, "segment count")?;
        if count != types.len() + 1 {
            return Err(Error::corrupted(format!(
                "Data frame has {count} segments, the device has {} columns",
                types.len()
            )));
        }
        let mut segments = Vec::with_capacity(count);
        segments.push(SegmentHeader::read(&mut r, None)?);
        for &t in types {
            let s = SegmentHeader::read(&mut r, Some(t))?;
            if s.null_count > row_count {
                return Err(Error::corrupted("null count exceeds the row count"));
            }
            segments.push(s);
        }
        r.finish("Data frame header")?;
        Ok(DataHeader {
            device_id,
            row_count,
            t_min,
            t_max,
            segments,
        })
    }

    /// Total size of the stored segments.
    pub(crate) fn segments_len(&self) -> u64 {
        self.segments.iter().map(|s| u64::from(s.stored_len)).sum()
    }
}

/// Builds the payload of a `Data` frame from its header and segment bytes.
pub(crate) fn encode_data_payload(header: &DataHeader, segments: &[Vec<u8>]) -> Vec<u8> {
    let header_bytes = header.encode();
    let total: usize = segments.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(4 + header_bytes.len() + total);
    out.put_u32(header_bytes.len() as u32);
    out.extend_from_slice(&header_bytes);
    for s in segments {
        out.extend_from_slice(s);
    }
    out
}

// ---------------------------------------------------------------- Tombstone

pub(crate) fn encode_tombstone(device_id: u32, ranges: &[(i64, i64)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.put_uvarint(u64::from(device_id));
    out.put_uvarint(ranges.len() as u64);
    for &(start, end) in ranges {
        out.put_i64(start);
        out.put_i64(end);
    }
    out
}

pub(crate) fn decode_tombstone(payload: &[u8]) -> Result<(u32, Vec<(i64, i64)>)> {
    let mut r = ByteReader::new(payload);
    let device_id = r.uvarint_u32()?;
    let count = r.len(r.remaining() / 16, "range count")?;
    if count == 0 {
        return Err(Error::corrupted("tombstone without ranges"));
    }
    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        let start = r.i64()?;
        let end = r.i64()?;
        if start > end {
            return Err(Error::corrupted("tombstone range with start after end"));
        }
        ranges.push((start, end));
    }
    r.finish("Tombstone frame")?;
    Ok((device_id, ranges))
}

// ---------------------------------------------------------------- Annotation

/// An operation on annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnnotationOp {
    Upsert {
        id: u64,
        device_id: u32,
        start: i64,
        end: i64,
        label: String,
        note: Option<String>,
    },
    Remove {
        id: u64,
    },
}

pub(crate) fn encode_annotation_ops(ops: &[AnnotationOp]) -> Vec<u8> {
    let mut out = Vec::new();
    out.put_uvarint(ops.len() as u64);
    for op in ops {
        match op {
            AnnotationOp::Upsert {
                id,
                device_id,
                start,
                end,
                label,
                note,
            } => {
                out.put_u8(1);
                out.put_uvarint(*id);
                out.put_uvarint(u64::from(*device_id));
                out.put_i64(*start);
                out.put_i64(*end);
                out.put_str(label);
                out.put_opt_str(note.as_deref());
            }
            AnnotationOp::Remove { id } => {
                out.put_u8(2);
                out.put_uvarint(*id);
            }
        }
    }
    out
}

pub(crate) fn decode_annotation_ops(payload: &[u8]) -> Result<Vec<AnnotationOp>> {
    let mut r = ByteReader::new(payload);
    let count = r.len(r.remaining() / 2, "annotation operation count")?;
    if count == 0 {
        return Err(Error::corrupted("annotation frame without operations"));
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        let tag = r.u8()?;
        let id = r.uvarint()?;
        if id == 0 {
            return Err(Error::corrupted("annotation identifier 0"));
        }
        ops.push(match tag {
            1 => {
                let device_id = r.uvarint_u32()?;
                let start = r.i64()?;
                let end = r.i64()?;
                if start > end {
                    return Err(Error::corrupted("annotation with start after end"));
                }
                AnnotationOp::Upsert {
                    id,
                    device_id,
                    start,
                    end,
                    label: r.string()?,
                    note: r.opt_string()?,
                }
            }
            2 => AnnotationOp::Remove { id },
            t => {
                return Err(Error::corrupted(format!(
                    "unknown annotation operation {t}"
                )));
            }
        });
    }
    r.finish("Annotation frame")?;
    Ok(ops)
}

// ---------------------------------------------------------------- Commit

/// The payload of a `Commit` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitRecord {
    pub(crate) number: u64,
    pub(crate) time: i64,
    pub(crate) first_offset: u64,
    pub(crate) frame_count: u32,
    pub(crate) prev_hash: Hash,
    pub(crate) content_hash: Hash,
    pub(crate) author: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) hash: Hash,
}

const COMMIT_DOMAIN: &[u8] = b"trosna/commit/v1\0";

impl CommitRecord {
    fn body(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.put_u64(self.number);
        out.put_i64(self.time);
        out.put_u64(self.first_offset);
        out.put_u32(self.frame_count);
        out.extend_from_slice(&self.prev_hash);
        out.extend_from_slice(&self.content_hash);
        out.put_opt_str(self.author.as_deref());
        out.put_opt_str(self.message.as_deref());
        out
    }

    fn compute_hash(body: &[u8]) -> Hash {
        let mut h = Sha256::new();
        h.update(COMMIT_DOMAIN);
        h.update(body);
        h.finalize().into()
    }

    /// Fills in `hash` from the other fields.
    pub(crate) fn seal(mut self) -> Self {
        self.hash = Self::compute_hash(&self.body());
        self
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = self.body();
        out.extend_from_slice(&self.hash);
        out
    }

    /// Parses a commit and checks its `commit_hash`.
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = ByteReader::new(payload);
        let number = r.u64()?;
        let time = r.i64()?;
        let first_offset = r.u64()?;
        let frame_count = r.u32()?;
        let prev_hash = r.array()?;
        let content_hash = r.array()?;
        let author = r.opt_string()?;
        let message = r.opt_string()?;
        let body_len = r.position();
        let hash: Hash = r.array()?;
        r.finish("Commit frame")?;
        if Self::compute_hash(&payload[..body_len]) != hash {
            return Err(Error::corrupted("commit hash does not match its content"));
        }
        Ok(CommitRecord {
            number,
            time,
            first_offset,
            frame_count,
            prev_hash,
            content_hash,
            author,
            message,
            hash,
        })
    }
}

// ---------------------------------------------------------------- Index & Footer

/// One entry of an `Index` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    pub(crate) kind: u8,
    pub(crate) offset: u64,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn encode_index(head_commit: u64, head_hash: &Hash, entries: &[IndexEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.put_u64(head_commit);
    out.extend_from_slice(head_hash);
    out.put_uvarint(entries.len() as u64);
    for e in entries {
        out.put_u8(e.kind);
        out.put_u64(e.offset);
        out.put_uvarint(e.body.len() as u64);
        out.extend_from_slice(&e.body);
    }
    out
}

pub(crate) fn decode_index(payload: &[u8]) -> Result<(u64, Hash, Vec<IndexEntry>)> {
    let mut r = ByteReader::new(payload);
    let head_commit = r.u64()?;
    let head_hash = r.array()?;
    // Every entry takes at least 10 bytes.
    let count = r.len(r.remaining() / 10, "index entry count")?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = r.u8()?;
        let offset = r.u64()?;
        let len = r.len(r.remaining(), "index entry length")?;
        entries.push(IndexEntry {
            kind,
            offset,
            body: r.bytes(len)?.to_vec(),
        });
    }
    r.finish("Index frame")?;
    Ok((head_commit, head_hash, entries))
}

pub(crate) fn encode_footer(index_offset: u64) -> Vec<u8> {
    index_offset.to_le_bytes().to_vec()
}

pub(crate) fn decode_footer(payload: &[u8]) -> Result<u64> {
    let mut r = ByteReader::new(payload);
    let offset = r.u64()?;
    r.finish("Footer frame")?;
    Ok(offset)
}

/// Formats a hash as lowercase hexadecimal.
pub fn hash_hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses a hash written as 64 hexadecimal digits.
pub fn parse_hash_hex(s: &str) -> Option<Hash> {
    if s.len() != HASH_LEN * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; HASH_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Codec;
    use crate::encoding::Encoding;

    #[test]
    fn frame_layout() {
        let frame = encode_frame(FrameKind::Footer, &encode_footer(1234)).unwrap();
        assert_eq!(frame.len() as u64, FOOTER_FRAME_LEN);
        assert_eq!(&frame[..4], SYNC);
        let mut header = [0u8; FRAME_HEADER_LEN];
        header.copy_from_slice(&frame[..FRAME_HEADER_LEN]);
        let parsed = parse_frame_header(&header).unwrap();
        assert_eq!(parsed.kind, 0x08);
        assert_eq!(parsed.len, 8);
        let payload = &frame[12..20];
        let crc = u32::from_le_bytes(frame[20..24].try_into().unwrap());
        assert_eq!(frame_crc(&header, payload), crc);
        assert_eq!(decode_footer(payload).unwrap(), 1234);
    }

    #[test]
    fn frame_kinds() {
        assert!(FrameKind::from_code(0x09).is_err());
        assert_eq!(
            FrameKind::from_code(0x91).unwrap(),
            FrameKind::Ancillary(0x91)
        );
        assert!(FrameKind::Index.is_derived());
        assert!(!FrameKind::Data.is_derived());
    }

    #[test]
    fn header_checks() {
        assert_eq!(check_file_header(&file_header()).unwrap(), (1, 0));
        assert!(matches!(
            check_file_header(b"PAR1xxxx"),
            Err(Error::NotTrosna)
        ));
        let mut v2 = file_header();
        v2[6] = 2;
        assert!(matches!(
            check_file_header(&v2),
            Err(Error::UnsupportedVersion { major: 2, .. })
        ));
    }

    #[test]
    fn device_round_trip() {
        let schema = DeviceSchema::new("ВМ-01", TimeUnit::Nanosecond)
            .with_timezone("Europe/Moscow")
            .with_time_name("ts")
            .with_column_schema(
                ColumnSchema::new("ram", DataType::Int64).with_metadata("unit", "MB"),
            )
            .with_column("ok", DataType::Bool)
            .with_metadata("host", "kvm-1");
        let bytes = encode_device(3, &schema);
        assert_eq!(decode_device(&bytes).unwrap(), (3, schema));
    }

    #[test]
    fn data_header_round_trip() {
        let seg = |encoding| SegmentHeader {
            encoding,
            codec: Codec::Zstd,
            has_validity: false,
            null_count: 0,
            stored_len: 10,
            raw_len: 20,
            crc: 7,
            stats: None,
        };
        let header = DataHeader {
            device_id: 0,
            row_count: 5,
            t_min: -5,
            t_max: 5,
            segments: vec![seg(Encoding::DeltaOfDelta), seg(Encoding::Xor)],
        };
        let bytes = header.encode();
        let types = [DataType::Float64];
        assert_eq!(
            DataHeader::decode(&bytes, |_| Ok(&types[..])).unwrap(),
            header
        );
        let wrong = [DataType::Float64, DataType::Bool];
        assert!(DataHeader::decode(&bytes, |_| Ok(&wrong[..])).is_err());
    }

    #[test]
    fn commit_hash_is_checked() {
        let commit = CommitRecord {
            number: 1,
            time: 1_700_000_000_000_000_000,
            first_offset: 8,
            frame_count: 2,
            prev_hash: [0; 32],
            content_hash: [9; 32],
            author: Some("Андрей".into()),
            message: None,
            hash: [0; 32],
        }
        .seal();
        let mut bytes = commit.encode();
        assert_eq!(CommitRecord::decode(&bytes).unwrap(), commit);
        bytes[3] ^= 1;
        assert!(CommitRecord::decode(&bytes).is_err());
    }

    #[test]
    fn other_payloads_round_trip() {
        let ranges = vec![(1, 5), (-3, -3)];
        assert_eq!(
            decode_tombstone(&encode_tombstone(2, &ranges)).unwrap(),
            (2, ranges)
        );
        assert!(decode_tombstone(&encode_tombstone(2, &[(5, 1)])).is_err());
        assert!(decode_tombstone(&encode_tombstone(2, &[])).is_err());

        let ops = vec![
            AnnotationOp::Upsert {
                id: 1,
                device_id: 0,
                start: 10,
                end: 20,
                label: "аномалия".into(),
                note: Some("reboot".into()),
            },
            AnnotationOp::Remove { id: 1 },
        ];
        assert_eq!(
            decode_annotation_ops(&encode_annotation_ops(&ops)).unwrap(),
            ops
        );

        let mut meta = BTreeMap::new();
        meta.insert("k".to_owned(), "v".to_owned());
        assert_eq!(decode_meta(&encode_meta(&meta)).unwrap(), meta);

        let entries = vec![IndexEntry {
            kind: 3,
            offset: 8,
            body: vec![1, 2, 3],
        }];
        let bytes = encode_index(4, &[1; 32], &entries);
        assert_eq!(decode_index(&bytes).unwrap(), (4, [1; 32], entries));
    }

    #[test]
    fn hash_hex_round_trip() {
        let h = [0xab; 32];
        assert_eq!(parse_hash_hex(&hash_hex(&h)), Some(h));
        assert_eq!(parse_hash_hex("xyz"), None);
    }
}
