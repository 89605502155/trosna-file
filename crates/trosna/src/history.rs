//! History of a file: commits, annotations and differences between versions
//! (DESIGN §4).

use std::collections::BTreeMap;
use std::fs::File;

use crate::catalog::{AnnotationState, Catalog};
use crate::column::{Batch, Value};
use crate::error::{Error, Result};
use crate::format::{Hash, hash_hex};
use crate::read::{self, ReadSpec};

/// A labelled closed interval `[start, end]` on the time axis of a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Identifier, unique within the file.
    pub id: u64,
    /// Device the annotation belongs to.
    pub device: String,
    /// Start of the interval, in the time unit of the device.
    pub start: i64,
    /// End of the interval (inclusive); equals `start` for an instant event.
    pub end: i64,
    /// Label, e.g. `"anomaly"`.
    pub label: String,
    /// Optional free-form note.
    pub note: Option<String>,
}

impl Annotation {
    pub(crate) fn from_state(catalog: &Catalog, id: u64, state: &AnnotationState) -> Self {
        Annotation {
            id,
            device: catalog.device(state.device_id).schema.name.clone(),
            start: state.start,
            end: state.end,
            label: state.label.clone(),
            note: state.note.clone(),
        }
    }
}

/// Summary of what a commit changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitChanges {
    /// Devices created by the commit.
    pub devices_created: Vec<String>,
    /// Number of `Data` blocks written.
    pub blocks_written: u32,
    /// Number of rows written (including rows that replace existing points).
    pub rows_written: u64,
    /// Number of deleted time intervals.
    pub ranges_deleted: u32,
    /// Number of annotation operations.
    pub annotation_ops: u32,
    /// Whether the file metadata was changed.
    pub metadata_changed: bool,
}

/// A commit: an atomic, hashed set of changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Sequence number, starting at 1.
    pub number: u64,
    /// Wall-clock time of the commit, nanoseconds since the Unix epoch.
    pub time: i64,
    /// Author, if recorded.
    pub author: Option<String>,
    /// Message, if recorded.
    pub message: Option<String>,
    /// Hash of the commit; identifies the whole history up to it.
    pub hash: Hash,
    /// Hash of the previous commit (or of the origin of a compacted file).
    pub prev_hash: Hash,
    /// Offset of the `Commit` frame in the file.
    pub offset: u64,
    /// What the commit changed.
    pub changes: CommitChanges,
}

impl CommitInfo {
    /// The hash as 64 hexadecimal digits.
    pub fn hash_hex(&self) -> String {
        hash_hex(&self.hash)
    }

    /// The first 12 hexadecimal digits of the hash.
    pub fn short_hash(&self) -> String {
        self.hash_hex()[..12].to_owned()
    }

    pub(crate) fn from_catalog(catalog: &Catalog, index: usize) -> Self {
        let entry = &catalog.commits[index];
        let start = if index == 0 {
            0
        } else {
            catalog.commits[index - 1].offset
        };
        let end = entry.offset;
        let within = |offset: u64| start < offset && offset < end;
        let mut changes = CommitChanges::default();
        for device in &catalog.devices {
            if within(device.offset) {
                changes.devices_created.push(device.schema.name.clone());
            }
        }
        for data in catalog.data.iter().filter(|d| within(d.offset)) {
            changes.blocks_written += 1;
            changes.rows_written += u64::from(data.header.row_count);
        }
        changes.ranges_deleted = catalog
            .tombstones
            .iter()
            .filter(|t| within(t.offset))
            .map(|t| t.ranges.len() as u32)
            .sum();
        changes.annotation_ops = catalog
            .annotations
            .iter()
            .filter(|a| within(a.offset))
            .map(|a| a.ops.len() as u32)
            .sum();
        changes.metadata_changed = catalog.metas.iter().any(|m| within(m.offset));
        let record = &entry.record;
        CommitInfo {
            number: record.number,
            time: record.time,
            author: record.author.clone(),
            message: record.message.clone(),
            hash: record.hash,
            prev_hash: record.prev_hash,
            offset: entry.offset,
            changes,
        }
    }
}

/// Kind of a change between two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Present only in the newer version.
    Added,
    /// Present only in the older version.
    Removed,
    /// Present in both versions with different values.
    Changed,
}

impl ChangeKind {
    /// `"added"`, `"removed"` or `"changed"`.
    pub fn name(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Changed => "changed",
        }
    }
}

/// A changed point.
#[derive(Debug, Clone, PartialEq)]
pub struct PointChange {
    /// Time stamp of the point.
    pub time: i64,
    /// Kind of change.
    pub kind: ChangeKind,
    /// Values in the older version (all columns, in schema order).
    pub before: Option<Vec<Value>>,
    /// Values in the newer version.
    pub after: Option<Vec<Value>>,
}

/// A changed annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationChange {
    /// Identifier of the annotation.
    pub id: u64,
    /// Kind of change.
    pub kind: ChangeKind,
    /// The annotation in the older version.
    pub before: Option<Annotation>,
    /// The annotation in the newer version.
    pub after: Option<Annotation>,
}

/// Differences of one device between two versions of a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Diff {
    /// Device name.
    pub device: String,
    /// Names of the value columns, the order of [`PointChange::before`] and [`PointChange::after`].
    pub columns: Vec<String>,
    /// Older version (commit number, 0 = empty file).
    pub from: u64,
    /// Newer version.
    pub to: u64,
    /// Changed points, ordered by time.
    pub points: Vec<PointChange>,
    /// Changed annotations of the device, ordered by identifier.
    pub annotations: Vec<AnnotationChange>,
}

fn rows_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same_as(y))
}

fn read_range(
    file: &File,
    catalog: &Catalog,
    device_id: u32,
    range: (i64, i64),
    limit: u64,
) -> Result<Batch> {
    let spec = ReadSpec {
        device_id,
        columns: (0..catalog.device(device_id).types.len()).collect(),
        lo: range.0,
        hi: range.1,
        limit,
        verify: true,
    };
    read::execute_all(file, catalog, &read::plan(catalog, spec))
}

/// Computes the differences of a device between versions `from` and `to`.
/// Only the time ranges touched by the commits in between are read.
pub(crate) fn diff(
    file: &File,
    catalog: &Catalog,
    device_id: u32,
    from: u64,
    to: u64,
) -> Result<Diff> {
    if from > to {
        return Err(Error::InvalidArgument(format!(
            "version {from} is newer than {to}"
        )));
    }
    let old_limit = catalog.limit(Some(from))?;
    let new_limit = catalog.limit(Some(to))?;
    let device = catalog.device(device_id);
    let touched = |offset: u64| old_limit <= offset && offset < new_limit;
    let mut ranges: Vec<(i64, i64)> = device
        .data
        .iter()
        .map(|&i| &catalog.data[i])
        .filter(|d| touched(d.offset))
        .map(|d| (d.header.t_min, d.header.t_max))
        .collect();
    for &i in &device.tombstones {
        let t = &catalog.tombstones[i];
        if touched(t.offset) {
            ranges.extend_from_slice(&t.ranges);
        }
    }
    let mut points = Vec::new();
    for range in read::merge_intervals(ranges) {
        let old = read_range(file, catalog, device_id, range, old_limit)?;
        let new = read_range(file, catalog, device_id, range, new_limit)?;
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            let old_t = old.time().get(i).copied();
            let new_t = new.time().get(j).copied();
            match (old_t, new_t) {
                (Some(a), Some(b)) if a == b => {
                    let (before, after) = (old.row(i), new.row(j));
                    if !rows_equal(&before, &after) {
                        points.push(PointChange {
                            time: a,
                            kind: ChangeKind::Changed,
                            before: Some(before),
                            after: Some(after),
                        });
                    }
                    i += 1;
                    j += 1;
                }
                (Some(a), b) if b.is_none_or(|b| a < b) => {
                    points.push(PointChange {
                        time: a,
                        kind: ChangeKind::Removed,
                        before: Some(old.row(i)),
                        after: None,
                    });
                    i += 1;
                }
                (_, Some(b)) => {
                    points.push(PointChange {
                        time: b,
                        kind: ChangeKind::Added,
                        before: None,
                        after: Some(new.row(j)),
                    });
                    j += 1;
                }
                (_, None) => break,
            }
        }
    }
    let of_device = |states: BTreeMap<u64, AnnotationState>| -> BTreeMap<u64, Annotation> {
        states
            .into_iter()
            .filter(|(_, s)| s.device_id == device_id)
            .map(|(id, s)| (id, Annotation::from_state(catalog, id, &s)))
            .collect()
    };
    let old_annotations = of_device(catalog.annotations_at(old_limit));
    let mut new_annotations = of_device(catalog.annotations_at(new_limit));
    let mut annotations = Vec::new();
    for (id, before) in old_annotations {
        match new_annotations.remove(&id) {
            None => annotations.push(AnnotationChange {
                id,
                kind: ChangeKind::Removed,
                before: Some(before),
                after: None,
            }),
            Some(after) if after != before => annotations.push(AnnotationChange {
                id,
                kind: ChangeKind::Changed,
                before: Some(before),
                after: Some(after),
            }),
            Some(_) => {}
        }
    }
    for (id, after) in new_annotations {
        annotations.push(AnnotationChange {
            id,
            kind: ChangeKind::Added,
            before: None,
            after: Some(after),
        });
    }
    annotations.sort_by_key(|a| a.id);
    Ok(Diff {
        device: device.schema.name.clone(),
        columns: device
            .schema
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect(),
        from,
        to,
        points,
        annotations,
    })
}
