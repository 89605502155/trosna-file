//! The catalogue: the in-memory description of every log frame of a file
//! except the column data. It is built from an `Index` frame or by scanning,
//! and maintained incrementally by the writer.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::{
    self, AnnotationOp, CommitRecord, DataHeader, FILE_HEADER_LEN, FRAME_HEADER_LEN, FrameKind,
    HASH_LEN, Hash, IndexEntry,
};
use crate::types::{DataType, DeviceSchema};

/// A device known to the catalogue.
#[derive(Debug, Clone)]
pub(crate) struct DeviceInfo {
    pub(crate) schema: Arc<DeviceSchema>,
    pub(crate) types: Vec<DataType>,
    pub(crate) offset: u64,
    /// Indices into [`Catalog::data`], in file order.
    pub(crate) data: Vec<usize>,
    /// Indices into [`Catalog::tombstones`], in file order.
    pub(crate) tombstones: Vec<usize>,
}

/// A `Data` frame.
#[derive(Debug, Clone)]
pub(crate) struct DataEntry {
    pub(crate) offset: u64,
    pub(crate) header: DataHeader,
    /// Absolute file offsets of the segments.
    pub(crate) segment_offsets: Vec<u64>,
}

/// A `Tombstone` frame.
#[derive(Debug, Clone)]
pub(crate) struct TombstoneEntry {
    pub(crate) offset: u64,
    pub(crate) device_id: u32,
    pub(crate) ranges: Vec<(i64, i64)>,
}

/// An `Annotation` frame.
#[derive(Debug, Clone)]
pub(crate) struct AnnotationEntry {
    pub(crate) offset: u64,
    pub(crate) ops: Vec<AnnotationOp>,
}

/// A `Commit` frame.
#[derive(Debug, Clone)]
pub(crate) struct CommitEntry {
    pub(crate) offset: u64,
    pub(crate) record: CommitRecord,
}

/// A `Meta` frame.
#[derive(Debug, Clone)]
pub(crate) struct MetaEntry {
    pub(crate) offset: u64,
    pub(crate) metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
enum Item {
    Meta,
    Device,
    Data,
    Tombstone,
    Annotation,
    Commit,
    Ancillary { kind: u8, body: Vec<u8> },
}

/// A visible annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnnotationState {
    pub(crate) device_id: u32,
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) label: String,
    pub(crate) note: Option<String>,
}

/// The catalogue of a file.
#[derive(Debug, Clone, Default)]
pub(crate) struct Catalog {
    order: Vec<(u64, Item)>,
    pub(crate) metas: Vec<MetaEntry>,
    pub(crate) devices: Vec<DeviceInfo>,
    by_name: HashMap<String, u32>,
    pub(crate) data: Vec<DataEntry>,
    pub(crate) tombstones: Vec<TombstoneEntry>,
    pub(crate) annotations: Vec<AnnotationEntry>,
    pub(crate) commits: Vec<CommitEntry>,
    /// Current (latest) state of annotations, used to validate operations.
    live_annotations: BTreeMap<u64, AnnotationState>,
    pub(crate) max_annotation_id: u64,
    /// Log frames after the last commit: count and offset of the first one.
    pending: (u32, Option<u64>),
}

impl Catalog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adds a log frame. `payload_len` is known when scanning and lets `Data`
    /// frames be checked against their frame length.
    pub(crate) fn apply(
        &mut self,
        kind: FrameKind,
        offset: u64,
        body: &[u8],
        payload_len: Option<u64>,
    ) -> Result<()> {
        self.apply_inner(kind, offset, body, payload_len)
            .map_err(|e| e.at(offset))
    }

    fn apply_inner(
        &mut self,
        kind: FrameKind,
        offset: u64,
        body: &[u8],
        payload_len: Option<u64>,
    ) -> Result<()> {
        if let Some((last, _)) = self.order.last()
            && offset <= *last
        {
            return Err(Error::corrupted("frames are not in file order"));
        }
        let item = match kind {
            FrameKind::Meta => {
                let metadata = format::decode_meta(body)?;
                self.metas.push(MetaEntry { offset, metadata });
                Item::Meta
            }
            FrameKind::Device => {
                let (id, schema) = format::decode_device(body)?;
                if id as usize != self.devices.len() {
                    return Err(Error::corrupted(format!(
                        "device id {id} out of sequence (expected {})",
                        self.devices.len()
                    )));
                }
                if self.by_name.contains_key(&schema.name) {
                    return Err(Error::corrupted(format!(
                        "duplicate device '{}'",
                        schema.name
                    )));
                }
                self.by_name.insert(schema.name.clone(), id);
                self.devices.push(DeviceInfo {
                    types: schema.columns.iter().map(|c| c.data_type).collect(),
                    schema: Arc::new(schema),
                    offset,
                    data: Vec::new(),
                    tombstones: Vec::new(),
                });
                Item::Device
            }
            FrameKind::Data => {
                let devices = &self.devices;
                let header = DataHeader::decode(body, |id| {
                    devices
                        .get(id as usize)
                        .map(|d| d.types.as_slice())
                        .ok_or_else(|| {
                            Error::corrupted(format!("Data frame for unknown device {id}"))
                        })
                })?;
                let mut position = offset + FRAME_HEADER_LEN as u64 + 4 + body.len() as u64;
                if let Some(len) = payload_len
                    && 4 + body.len() as u64 + header.segments_len() != len
                {
                    return Err(Error::corrupted(
                        "segment lengths do not match the frame length",
                    ));
                }
                let segment_offsets = header
                    .segments
                    .iter()
                    .map(|s| {
                        let start = position;
                        position += u64::from(s.stored_len);
                        start
                    })
                    .collect();
                let device = header.device_id as usize;
                self.devices[device].data.push(self.data.len());
                self.data.push(DataEntry {
                    offset,
                    header,
                    segment_offsets,
                });
                Item::Data
            }
            FrameKind::Tombstone => {
                let (device_id, ranges) = format::decode_tombstone(body)?;
                let device = self.devices.get_mut(device_id as usize).ok_or_else(|| {
                    Error::corrupted(format!("tombstone for unknown device {device_id}"))
                })?;
                device.tombstones.push(self.tombstones.len());
                self.tombstones.push(TombstoneEntry {
                    offset,
                    device_id,
                    ranges,
                });
                Item::Tombstone
            }
            FrameKind::Annotation => {
                let ops = format::decode_annotation_ops(body)?;
                for op in &ops {
                    self.apply_annotation_op(op)?;
                }
                self.annotations.push(AnnotationEntry { offset, ops });
                Item::Annotation
            }
            FrameKind::Commit => {
                let record = CommitRecord::decode(body)?;
                self.check_commit(offset, &record)?;
                self.commits.push(CommitEntry { offset, record });
                self.pending = (0, None);
                self.order.push((offset, Item::Commit));
                return Ok(());
            }
            FrameKind::Ancillary(kind) => Item::Ancillary {
                kind,
                body: body.to_vec(),
            },
            FrameKind::Index | FrameKind::Footer => {
                return Err(Error::corrupted("derived frame listed as a log frame"));
            }
        };
        self.order.push((offset, item));
        self.pending.0 += 1;
        self.pending.1.get_or_insert(offset);
        Ok(())
    }

    fn apply_annotation_op(&mut self, op: &AnnotationOp) -> Result<()> {
        match op {
            AnnotationOp::Upsert {
                id,
                device_id,
                start,
                end,
                label,
                note,
            } => {
                if *device_id as usize >= self.devices.len() {
                    return Err(Error::corrupted(format!(
                        "annotation for unknown device {device_id}"
                    )));
                }
                self.live_annotations.insert(
                    *id,
                    AnnotationState {
                        device_id: *device_id,
                        start: *start,
                        end: *end,
                        label: label.clone(),
                        note: note.clone(),
                    },
                );
                self.max_annotation_id = self.max_annotation_id.max(*id);
            }
            AnnotationOp::Remove { id } => {
                if self.live_annotations.remove(id).is_none() {
                    return Err(Error::corrupted(format!(
                        "removal of absent annotation {id}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn check_commit(&self, offset: u64, record: &CommitRecord) -> Result<()> {
        let expected_number = self.commits.len() as u64 + 1;
        if record.number != expected_number {
            return Err(Error::corrupted(format!(
                "commit number {} out of sequence (expected {expected_number})",
                record.number
            )));
        }
        if let Some(prev) = self.commits.last()
            && record.prev_hash != prev.record.hash
        {
            return Err(Error::corrupted(
                "commit does not link to the previous commit",
            ));
        }
        let (count, first) = self.pending;
        if record.frame_count != count || record.first_offset != first.unwrap_or(offset) {
            return Err(Error::corrupted(
                "commit does not describe the frames it seals",
            ));
        }
        Ok(())
    }

    /// Removes all frames at or after `offset`.
    pub(crate) fn truncate(&mut self, offset: u64) {
        while let Some((pos, item)) = self.order.last() {
            if *pos < offset {
                break;
            }
            match item {
                Item::Meta => {
                    self.metas.pop();
                }
                Item::Device => {
                    if let Some(device) = self.devices.pop() {
                        self.by_name.remove(&device.schema.name);
                    }
                }
                Item::Data => {
                    if let Some(entry) = self.data.pop() {
                        self.devices[entry.header.device_id as usize].data.pop();
                    }
                }
                Item::Tombstone => {
                    if let Some(entry) = self.tombstones.pop() {
                        self.devices[entry.device_id as usize].tombstones.pop();
                    }
                }
                Item::Annotation => {
                    self.annotations.pop();
                }
                Item::Commit => {
                    self.commits.pop();
                }
                Item::Ancillary { .. } => {}
            }
            self.order.pop();
        }
        // Rebuild the derived state.
        self.live_annotations.clear();
        self.max_annotation_id = 0;
        let ops: Vec<AnnotationOp> = self
            .annotations
            .iter()
            .flat_map(|a| a.ops.clone())
            .collect();
        for op in &ops {
            // The operations were valid when applied in this order.
            let _ = self.apply_annotation_op(op);
        }
        let last_commit = self.commits.last().map_or(0, |c| c.offset);
        let after: Vec<u64> = self
            .order
            .iter()
            .filter(|(pos, item)| *pos > last_commit && !matches!(item, Item::Commit))
            .map(|(pos, _)| *pos)
            .collect();
        self.pending = (after.len() as u32, after.first().copied());
    }

    /// Builds a catalogue from the entries of an `Index` frame.
    pub(crate) fn from_index(entries: &[IndexEntry]) -> Result<Self> {
        let mut catalog = Catalog::new();
        for entry in entries {
            let kind = FrameKind::from_code(entry.kind)?;
            catalog.apply(kind, entry.offset, &entry.body, None)?;
        }
        if catalog.pending.0 != 0 {
            return Err(Error::corrupted("the index lists uncommitted frames"));
        }
        Ok(catalog)
    }

    /// Entries of an `Index` frame describing this catalogue.
    pub(crate) fn index_entries(&self) -> Vec<IndexEntry> {
        let (mut meta, mut device, mut data, mut tomb, mut ann, mut commit) = (0, 0, 0, 0, 0, 0);
        self.order
            .iter()
            .map(|(offset, item)| {
                let (kind, body) = match item {
                    Item::Meta => {
                        meta += 1;
                        (
                            FrameKind::Meta,
                            format::encode_meta(&self.metas[meta - 1].metadata),
                        )
                    }
                    Item::Device => {
                        device += 1;
                        let d = &self.devices[device - 1];
                        (
                            FrameKind::Device,
                            format::encode_device(device as u32 - 1, &d.schema),
                        )
                    }
                    Item::Data => {
                        data += 1;
                        (FrameKind::Data, self.data[data - 1].header.encode())
                    }
                    Item::Tombstone => {
                        tomb += 1;
                        let t = &self.tombstones[tomb - 1];
                        (
                            FrameKind::Tombstone,
                            format::encode_tombstone(t.device_id, &t.ranges),
                        )
                    }
                    Item::Annotation => {
                        ann += 1;
                        (
                            FrameKind::Annotation,
                            format::encode_annotation_ops(&self.annotations[ann - 1].ops),
                        )
                    }
                    Item::Commit => {
                        commit += 1;
                        (FrameKind::Commit, self.commits[commit - 1].record.encode())
                    }
                    Item::Ancillary { kind, body } => (FrameKind::Ancillary(*kind), body.clone()),
                };
                IndexEntry {
                    kind: kind.code(),
                    offset: *offset,
                    body,
                }
            })
            .collect()
    }

    /// Number and hash of the last commit (`0` and zeros if there is none).
    pub(crate) fn head(&self) -> (u64, Hash) {
        self.commits
            .last()
            .map_or((0, [0; HASH_LEN]), |c| (c.record.number, c.record.hash))
    }

    /// Frames after the last commit.
    pub(crate) fn pending_frames(&self) -> u32 {
        self.pending.0
    }

    /// Offset of the first frame after the last commit, if any.
    pub(crate) fn pending_first_offset(&self) -> Option<u64> {
        self.pending.1
    }

    /// Id of the named device.
    pub(crate) fn device_id(&self, name: &str) -> Result<u32> {
        self.by_name
            .get(name)
            .copied()
            .ok_or_else(|| Error::UnknownDevice(name.to_owned()))
    }

    pub(crate) fn device(&self, id: u32) -> &DeviceInfo {
        &self.devices[id as usize]
    }

    /// The exclusive offset bound of version `as_of`: frames before it belong
    /// to the version. `None` means everything in the catalogue.
    pub(crate) fn limit(&self, as_of: Option<u64>) -> Result<u64> {
        match as_of {
            None => Ok(u64::MAX),
            Some(0) => Ok(FILE_HEADER_LEN),
            Some(n) => self
                .commits
                .get(n as usize - 1)
                .map(|c| c.offset)
                .ok_or(Error::UnknownCommit(n)),
        }
    }

    /// File metadata at the given limit.
    pub(crate) fn metadata_at(&self, limit: u64) -> BTreeMap<String, String> {
        self.metas
            .iter()
            .rev()
            .find(|m| m.offset < limit)
            .map(|m| m.metadata.clone())
            .unwrap_or_default()
    }

    /// Annotations visible at the given limit.
    pub(crate) fn annotations_at(&self, limit: u64) -> BTreeMap<u64, AnnotationState> {
        let mut state = BTreeMap::new();
        for entry in self.annotations.iter().take_while(|a| a.offset < limit) {
            for op in &entry.ops {
                match op {
                    AnnotationOp::Upsert {
                        id,
                        device_id,
                        start,
                        end,
                        label,
                        note,
                    } => {
                        state.insert(
                            *id,
                            AnnotationState {
                                device_id: *device_id,
                                start: *start,
                                end: *end,
                                label: label.clone(),
                                note: note.clone(),
                            },
                        );
                    }
                    AnnotationOp::Remove { id } => {
                        state.remove(id);
                    }
                }
            }
        }
        state
    }

    /// The latest state of an annotation, including uncommitted operations.
    pub(crate) fn live_annotation(&self, id: u64) -> Option<&AnnotationState> {
        self.live_annotations.get(&id)
    }

    /// Commit number whose frames contain `offset` (the first commit at or after it).
    pub(crate) fn commit_of(&self, offset: u64) -> Option<u64> {
        let i = self.commits.partition_point(|c| c.offset < offset);
        self.commits.get(i).map(|c| c.record.number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TimeUnit;

    fn device_body(id: u32, name: &str) -> Vec<u8> {
        let schema = DeviceSchema::new(name, TimeUnit::Second).with_column("x", DataType::Int64);
        format::encode_device(id, &schema)
    }

    fn commit_body(catalog: &Catalog, offset: u64, time: i64) -> Vec<u8> {
        let (count, first) = catalog.pending;
        CommitRecord {
            number: catalog.commits.len() as u64 + 1,
            time,
            first_offset: first.unwrap_or(offset),
            frame_count: count,
            prev_hash: catalog.head().1,
            content_hash: [0; 32],
            author: None,
            message: None,
            hash: [0; 32],
        }
        .seal()
        .encode()
    }

    #[test]
    fn devices_commits_and_truncation() {
        let mut c = Catalog::new();
        c.apply(FrameKind::Device, 8, &device_body(0, "a"), None)
            .unwrap();
        let body = commit_body(&c, 100, 1);
        c.apply(FrameKind::Commit, 100, &body, None).unwrap();
        c.apply(FrameKind::Device, 200, &device_body(1, "b"), None)
            .unwrap();
        assert_eq!(c.pending_frames(), 1);
        assert!(
            c.apply(FrameKind::Device, 300, &device_body(2, "a"), None)
                .is_err()
        );
        c.truncate(150);
        assert_eq!(c.devices.len(), 1);
        assert_eq!(c.pending_frames(), 0);
        assert!(c.device_id("b").is_err());
        assert_eq!(c.head().0, 1);
        assert_eq!(c.limit(Some(1)).unwrap(), 100);
        assert!(c.limit(Some(2)).is_err());
    }

    #[test]
    fn commit_sequence_is_checked() {
        let mut c = Catalog::new();
        c.apply(FrameKind::Device, 8, &device_body(0, "a"), None)
            .unwrap();
        let mut record = CommitRecord::decode(&commit_body(&c, 100, 1)).unwrap();
        record.frame_count = 5;
        assert!(
            c.apply(FrameKind::Commit, 100, &record.seal().encode(), None)
                .is_err()
        );
    }

    #[test]
    fn annotations_are_validated() {
        let mut c = Catalog::new();
        c.apply(FrameKind::Device, 8, &device_body(0, "a"), None)
            .unwrap();
        let remove = format::encode_annotation_ops(&[AnnotationOp::Remove { id: 1 }]);
        assert!(c.apply(FrameKind::Annotation, 50, &remove, None).is_err());
        let upsert = format::encode_annotation_ops(&[AnnotationOp::Upsert {
            id: 7,
            device_id: 0,
            start: 1,
            end: 2,
            label: "x".into(),
            note: None,
        }]);
        c.apply(FrameKind::Annotation, 60, &upsert, None).unwrap();
        assert_eq!(c.max_annotation_id, 7);
        c.apply(
            FrameKind::Annotation,
            70,
            &format::encode_annotation_ops(&[AnnotationOp::Remove { id: 7 }]),
            None,
        )
        .unwrap();
        assert_eq!(c.annotations_at(65).len(), 1);
        assert_eq!(c.annotations_at(u64::MAX).len(), 0);
    }

    #[test]
    fn index_round_trip() {
        let mut c = Catalog::new();
        c.apply(FrameKind::Device, 8, &device_body(0, "a"), None)
            .unwrap();
        let mut meta = BTreeMap::new();
        meta.insert("k".to_owned(), "v".to_owned());
        c.apply(FrameKind::Meta, 40, &format::encode_meta(&meta), None)
            .unwrap();
        let body = commit_body(&c, 80, 5);
        c.apply(FrameKind::Commit, 80, &body, None).unwrap();
        let entries = c.index_entries();
        let rebuilt = Catalog::from_index(&entries).unwrap();
        assert_eq!(rebuilt.index_entries(), entries);
        assert_eq!(rebuilt.metadata_at(u64::MAX), meta);
        assert_eq!(rebuilt.metadata_at(40), BTreeMap::new());
    }
}
