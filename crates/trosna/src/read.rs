//! The read engine: block selection, projection and merge-on-read
//! (DESIGN §6). Shared by [`crate::Reader`] and [`crate::Writer`].

use std::fs::File;
use std::sync::Arc;

use crate::catalog::{Catalog, DataEntry};
use crate::column::{Batch, Column, ColumnBuilder};
use crate::error::{Error, Result};
use crate::fileio::read_vec_at;
use crate::segment;
use crate::types::DataType;

/// What to read.
#[derive(Debug, Clone)]
pub(crate) struct ReadSpec {
    pub(crate) device_id: u32,
    /// Positions of the requested value columns in the device schema.
    pub(crate) columns: Vec<usize>,
    /// Inclusive time bounds.
    pub(crate) lo: i64,
    pub(crate) hi: i64,
    /// Exclusive offset bound of the version to read.
    pub(crate) limit: u64,
    pub(crate) verify: bool,
}

/// A group of transitively time-overlapping blocks.
#[derive(Debug, Clone)]
pub(crate) struct Cluster {
    /// Indices into [`Catalog::data`], sorted by `t_min`.
    blocks: Vec<usize>,
    /// For each block, the sorted disjoint time intervals deleted by later tombstones.
    deletions: Vec<Vec<(i64, i64)>>,
}

/// The blocks to read for a query, grouped into clusters.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub(crate) spec: ReadSpec,
    pub(crate) names: Arc<[String]>,
    pub(crate) types: Vec<DataType>,
    pub(crate) clusters: Vec<Cluster>,
}

fn overlaps(a: (i64, i64), b: (i64, i64)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

/// Sorts intervals and merges overlapping or adjacent ones.
pub(crate) fn merge_intervals(mut intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    intervals.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(intervals.len());
    for (lo, hi) in intervals {
        match out.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// Plans a query.
pub(crate) fn plan(catalog: &Catalog, spec: ReadSpec) -> Plan {
    let device = catalog.device(spec.device_id);
    let names: Arc<[String]> = spec
        .columns
        .iter()
        .map(|&c| device.schema.columns[c].name.clone())
        .collect();
    let types = spec.columns.iter().map(|&c| device.types[c]).collect();
    let query = (spec.lo, spec.hi);
    let mut clusters = Vec::new();
    if spec.lo <= spec.hi && device.offset < spec.limit {
        let mut blocks: Vec<usize> = device
            .data
            .iter()
            .copied()
            .filter(|&i| {
                let h = &catalog.data[i].header;
                catalog.data[i].offset < spec.limit && overlaps((h.t_min, h.t_max), query)
            })
            .collect();
        blocks.sort_by_key(|&i| (catalog.data[i].header.t_min, catalog.data[i].offset));
        let tombstones: Vec<_> = device
            .tombstones
            .iter()
            .map(|&i| &catalog.tombstones[i])
            .filter(|t| t.offset < spec.limit)
            .collect();
        let mut current: Option<(Cluster, i64)> = None;
        for i in blocks {
            let entry = &catalog.data[i];
            let range = (entry.header.t_min, entry.header.t_max);
            let deleted = merge_intervals(
                tombstones
                    .iter()
                    .filter(|t| t.offset > entry.offset)
                    .flat_map(|t| t.ranges.iter().copied())
                    .filter(|&r| overlaps(r, range) && overlaps(r, query))
                    .collect(),
            );
            match &mut current {
                Some((cluster, max)) if range.0 <= *max => {
                    cluster.blocks.push(i);
                    cluster.deletions.push(deleted);
                    *max = (*max).max(range.1);
                }
                _ => {
                    if let Some((cluster, _)) = current.take() {
                        clusters.push(cluster);
                    }
                    current = Some((
                        Cluster {
                            blocks: vec![i],
                            deletions: vec![deleted],
                        },
                        range.1,
                    ));
                }
            }
        }
        if let Some((cluster, _)) = current {
            clusters.push(cluster);
        }
    }
    Plan {
        spec,
        names,
        types,
        clusters,
    }
}

fn read_segment(file: &File, entry: &DataEntry, segment: usize) -> Result<Vec<u8>> {
    let len = entry.header.segments[segment].stored_len as usize;
    read_vec_at(file, entry.segment_offsets[segment], len).map_err(|e| e.at(entry.offset))
}

/// Decodes and validates the time column of a block.
pub(crate) fn decode_block_time(file: &File, entry: &DataEntry, verify: bool) -> Result<Vec<i64>> {
    let header = &entry.header;
    let stored = read_segment(file, entry, 0)?;
    let time = segment::decode_time(
        &header.segments[0],
        &stored,
        header.row_count as usize,
        verify,
    )
    .map_err(|e| e.at(entry.offset))?;
    let increasing = time.windows(2).all(|w| w[0] < w[1]);
    if !increasing || time.first() != Some(&header.t_min) || time.last() != Some(&header.t_max) {
        return Err(
            Error::corrupted("time stamps of a block are not strictly increasing").at(entry.offset),
        );
    }
    Ok(time)
}

/// Decodes value column `column` of a block.
pub(crate) fn decode_block_column(
    file: &File,
    catalog: &Catalog,
    entry: &DataEntry,
    column: usize,
    verify: bool,
) -> Result<Column> {
    let data_type = catalog.device(entry.header.device_id).types[column];
    let stored = read_segment(file, entry, column + 1)?;
    segment::decode_column(
        &entry.header.segments[column + 1],
        &stored,
        entry.header.row_count as usize,
        data_type,
        verify,
    )
    .map_err(|e| e.at(entry.offset))
}

/// True if `t` lies in one of the sorted disjoint intervals, advancing `cursor`.
fn deleted(intervals: &[(i64, i64)], cursor: &mut usize, t: i64) -> bool {
    while *cursor < intervals.len() && intervals[*cursor].1 < t {
        *cursor += 1;
    }
    *cursor < intervals.len() && intervals[*cursor].0 <= t
}

/// Executes one cluster of a plan. Returns `None` if no rows remain.
pub(crate) fn execute(
    file: &File,
    catalog: &Catalog,
    plan: &Plan,
    cluster: &Cluster,
) -> Result<Option<Batch>> {
    let spec = &plan.spec;
    if cluster.blocks.len() == 1 && cluster.deletions[0].is_empty() {
        // Fast path: a block that nothing overlaps or deletes.
        let entry = &catalog.data[cluster.blocks[0]];
        let time = decode_block_time(file, entry, spec.verify)?;
        let start = time.partition_point(|&t| t < spec.lo);
        let end = time.partition_point(|&t| t <= spec.hi);
        if start >= end {
            return Ok(None);
        }
        let whole = start == 0 && end == time.len();
        let mut columns = Vec::with_capacity(spec.columns.len());
        for &c in &spec.columns {
            let column = decode_block_column(file, catalog, entry, c, spec.verify)?;
            columns.push(if whole {
                column
            } else {
                column.slice(start..end)
            });
        }
        let time = if whole {
            time
        } else {
            time[start..end].to_vec()
        };
        return Batch::try_from_parts(time, plan.names.clone(), columns).map(Some);
    }

    // Merge path: newest version of every time stamp, minus deletions.
    let mut times = Vec::with_capacity(cluster.blocks.len());
    let mut picks: Vec<(i64, u64, u32, u32)> = Vec::new(); // (t, offset, block, row)
    for (b, &i) in cluster.blocks.iter().enumerate() {
        let entry = &catalog.data[i];
        let time = decode_block_time(file, entry, spec.verify)?;
        let mut cursor = 0;
        let start = time.partition_point(|&t| t < spec.lo);
        for (row, &t) in time.iter().enumerate().skip(start) {
            if t > spec.hi {
                break;
            }
            if !deleted(&cluster.deletions[b], &mut cursor, t) {
                picks.push((t, entry.offset, b as u32, row as u32));
            }
        }
        times.push(time);
    }
    picks.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    picks.dedup_by_key(|p| p.0);
    if picks.is_empty() {
        return Ok(None);
    }
    let mut used = vec![false; cluster.blocks.len()];
    for p in &picks {
        used[p.2 as usize] = true;
    }
    let mut columns = Vec::with_capacity(spec.columns.len());
    for (k, &c) in spec.columns.iter().enumerate() {
        let decoded: Vec<Option<Column>> = cluster
            .blocks
            .iter()
            .enumerate()
            .map(|(b, &i)| {
                used[b]
                    .then(|| decode_block_column(file, catalog, &catalog.data[i], c, spec.verify))
                    .transpose()
            })
            .collect::<Result<_>>()?;
        let mut builder = ColumnBuilder::new(plan.types[k], picks.len());
        for p in &picks {
            let source = decoded[p.2 as usize]
                .as_ref()
                .ok_or_else(|| Error::corrupted("internal: missing decoded block"))?;
            builder.push_from(source, p.3 as usize)?;
        }
        columns.push(builder.finish());
    }
    let time = picks.iter().map(|p| p.0).collect();
    Batch::try_from_parts(time, plan.names.clone(), columns).map(Some)
}

/// Executes a whole plan and concatenates the result.
pub(crate) fn execute_all(file: &File, catalog: &Catalog, plan: &Plan) -> Result<Batch> {
    let mut batches = Vec::new();
    for cluster in &plan.clusters {
        if let Some(batch) = execute(file, catalog, plan, cluster)? {
            batches.push(batch);
        }
    }
    match batches.len() {
        0 => Ok(Batch::empty(plan.names.clone(), &plan.types)),
        1 => Ok(batches.pop().expect("one batch")),
        _ => Batch::concat(&batches),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_merging() {
        assert_eq!(
            merge_intervals(vec![(5, 7), (1, 2), (3, 4), (10, 12), (11, 11)]),
            vec![(1, 7), (10, 12)]
        );
        assert_eq!(
            merge_intervals(vec![(i64::MIN, 0), (1, i64::MAX)]),
            vec![(i64::MIN, i64::MAX)]
        );
        assert!(merge_intervals(vec![]).is_empty());
    }

    #[test]
    fn deletion_cursor() {
        let intervals = [(2, 3), (7, 9)];
        let mut cursor = 0;
        let hits: Vec<bool> = (0..11)
            .map(|t| deleted(&intervals, &mut cursor, t))
            .collect();
        assert_eq!(
            hits,
            [
                false, false, true, true, false, false, false, true, true, true, false
            ]
        );
    }
}
