//! File-level tools: integrity verification, recovery and compaction.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::fileio::read_vec_at;
use crate::format::{
    self, CommitRecord, FILE_HEADER_LEN, FRAME_HEADER_LEN, FrameKind, HASH_LEN, Hash, hash_hex,
};
use crate::read;
use crate::reader::Reader;
use crate::scan::{self, Stop};
use crate::writer::{CommitOptions, RecoveryReport, WriteOptions, Writer};

/// Result of [`verify`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Number of valid frames.
    pub frames: u64,
    /// Number of commits.
    pub commits: u64,
    /// Number of `Data` blocks whose segments were all decoded successfully.
    pub blocks: u64,
    /// Whether the file ends with a valid footer and index.
    pub finalized: bool,
    /// Hash of the last commit.
    pub head: Option<Hash>,
    /// `prev_hash` of the first commit if it is not zero: the head of the file
    /// this one was compacted from.
    pub origin: Option<Hash>,
    /// Problems found; empty if the file is intact.
    pub problems: Vec<String>,
    /// Observations that are not errors, such as an unfinished transaction.
    pub warnings: Vec<String>,
}

impl VerifyReport {
    /// True if no problems were found.
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Verifies a file completely: frame and segment checksums, decoding of all
/// segments, the hash chain of commits, and the consistency of the index.
pub fn verify(path: impl AsRef<Path>) -> Result<VerifyReport> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < FILE_HEADER_LEN {
        return Err(Error::NotTrosna);
    }
    format::check_file_header(&read_vec_at(&file, 0, FILE_HEADER_LEN as usize)?)?;
    let mut report = VerifyReport::default();

    let mut hasher = Sha256::new();
    let mut chain_problems = Vec::new();
    let mut frames = 0u64;
    let scanned = scan::scan(&file, len, true, |event| {
        frames += 1;
        match event.kind {
            FrameKind::Index | FrameKind::Footer => {}
            FrameKind::Commit => {
                let payload = &event.bytes[FRAME_HEADER_LEN..event.bytes.len() - 4];
                let record = CommitRecord::decode(payload)?;
                let content: Hash = std::mem::take(&mut hasher).finalize().into();
                if content != record.content_hash {
                    chain_problems.push(format!(
                        "commit {} at offset {}: content hash mismatch (the sealed frames were modified)",
                        record.number, event.offset
                    ));
                }
            }
            _ => hasher.update(event.bytes),
        }
        Ok(())
    });
    report.frames = frames;
    let outcome = match scanned {
        Ok(outcome) => outcome,
        Err(e) if e.is_corruption() || matches!(e, Error::Unsupported(_)) => {
            report.problems.push(e.to_string());
            return Ok(report);
        }
        Err(e) => return Err(e),
    };
    report.problems.extend(chain_problems);
    match &outcome.stop {
        Stop::EndOfFile => {}
        Stop::Truncated => report.warnings.push(format!(
            "the last frame (at offset {}) is incomplete: the writer was interrupted",
            outcome.stopped_at
        )),
        Stop::Invalid(reason) => {
            let msg = format!("invalid frame at offset {}: {reason}", outcome.stopped_at);
            if scan::find_valid_frame_after(&file, len, outcome.stopped_at)?.is_some() {
                report
                    .problems
                    .push(format!("{msg}; valid frames follow it"));
            } else {
                report
                    .warnings
                    .push(format!("{msg} (at the end of the file)"));
            }
        }
    }
    if outcome.discarded_frames > 0 {
        report.warnings.push(format!(
            "{} frames of an unfinished transaction at the end of the file are ignored",
            outcome.discarded_frames
        ));
    }

    let catalog = outcome.catalog;
    report.commits = catalog.commits.len() as u64;
    report.head = catalog.commits.last().map(|c| c.record.hash);
    report.origin = catalog
        .commits
        .first()
        .map(|c| c.record.prev_hash)
        .filter(|h| *h != [0; HASH_LEN]);

    for entry in &catalog.data {
        let decoded = read::decode_block_time(&file, entry, true).and_then(|_| {
            (0..entry.header.segments.len() - 1).try_for_each(|c| {
                read::decode_block_column(&file, &catalog, entry, c, true).map(drop)
            })
        });
        match decoded {
            Ok(()) => report.blocks += 1,
            Err(e) => report.problems.push(e.to_string()),
        }
    }

    match scan::load_index(&file, len) {
        Ok(Some(index)) => {
            report.finalized = outcome.ends_with_footer;
            if index.index_entries() != catalog.index_entries() {
                report
                    .problems
                    .push("the index does not match the frames of the file".into());
            }
        }
        Ok(None) => report
            .warnings
            .push("the file is not finalized; run `trosna recover`".into()),
        Err(e) => report.problems.push(format!("damaged index: {e}")),
    }
    Ok(report)
}

/// Result of [`recover`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverReport {
    /// The file was already finalized and was not changed.
    pub was_finalized: bool,
    /// What was removed, if anything.
    pub recovery: Option<RecoveryReport>,
}

/// Finalizes a file whose writer did not close it: removes the unfinished
/// transaction and writes a new index. With `repair_corruption`, data after a
/// damaged frame in the middle of the file is discarded as well.
pub fn recover(path: impl AsRef<Path>, repair_corruption: bool) -> Result<RecoverReport> {
    let options = WriteOptions {
        repair_corruption,
        ..WriteOptions::default()
    };
    let writer = Writer::open_with(path, options)?;
    let report = RecoverReport {
        was_finalized: writer.recovery().is_none(),
        recovery: writer.recovery().cloned(),
    };
    writer.close()?;
    Ok(report)
}

/// Result of [`compact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    /// Version of the source file that was written.
    pub version: u64,
    /// Hash of that version, recorded as the origin of the new file.
    pub origin: Hash,
    /// Points written.
    pub rows: u64,
    /// Annotations written.
    pub annotations: u64,
    /// Size of the source file.
    pub bytes_before: u64,
    /// Size of the new file.
    pub bytes_after: u64,
}

/// Writes the state of `source` at version `as_of` (the latest by default)
/// into a new file without history and without overlapping blocks. The first
/// commit of the new file links to the source version's hash.
pub fn compact(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
    as_of: Option<u64>,
    options: WriteOptions,
) -> Result<CompactReport> {
    let reader = Reader::open(source)?;
    let version = as_of.unwrap_or_else(|| reader.head().map_or(0, |c| c.number));
    let origin = if version == 0 {
        [0; HASH_LEN]
    } else {
        reader.commit(version)?.hash
    };
    let mut writer = Writer::create_with(target.as_ref(), options)?;
    writer.set_chain_origin(origin)?;

    let mut metadata: BTreeMap<String, String> = reader.metadata_as_of(version)?;
    metadata.insert("trosna.compacted_from".into(), hash_hex(&origin));
    metadata.insert("trosna.compacted_from_commit".into(), version.to_string());
    for (key, value) in &metadata {
        writer.set_metadata(key, value)?;
    }

    let mut rows = 0u64;
    let existing: Vec<_> = reader
        .devices()
        .filter(|d| reader.device_exists_at(&d.name, version))
        .cloned()
        .collect();
    for schema in existing {
        let name = schema.name.clone();
        writer.create_device(schema)?;
        for batch in reader.query(&name)?.as_of(version).batches()? {
            let batch = batch?;
            rows += batch.len() as u64;
            writer.write(&name, &batch)?;
        }
    }
    let annotations = reader.annotations(None, Some(version))?;
    for a in &annotations {
        writer.restore_annotation(a.id, &a.device, a.start, a.end, &a.label, a.note.as_deref())?;
    }
    let short = &hash_hex(&origin)[..12];
    writer.commit_with(CommitOptions::message(format!(
        "compacted from {short} at version {version}"
    )))?;
    writer.close()?;
    Ok(CompactReport {
        version,
        origin,
        rows,
        annotations: annotations.len() as u64,
        bytes_before: reader.file_size(),
        bytes_after: std::fs::metadata(target)?.len(),
    })
}
