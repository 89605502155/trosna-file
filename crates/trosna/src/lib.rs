//! # Trosna
//!
//! A memory-safe implementation of the Trosna time-series file format
//! (`*.trosna`): columnar storage with time-series encodings (delta-of-delta,
//! Gorilla XOR, TS_2DIFF-style bit packing, RLE, dictionaries) and a
//! general-purpose codec, organised as an append-only log of checksummed
//! frames with atomic, hash-chained commits.
//!
//! Points can be inserted, updated and deleted anywhere on the time axis,
//! devices can carry versioned interval annotations, and every past version
//! of the file stays readable ("time travel"). The commit hashes make the
//! history tamper-evident.
//!
//! ```no_run
//! use trosna::{Batch, Column, DataType, DeviceSchema, Reader, TimeUnit, Value, Writer};
//!
//! // Create a file with one device.
//! let mut w = Writer::create("metrics.trosna")?;
//! w.create_device(
//!     DeviceSchema::new("vm01", TimeUnit::Millisecond)
//!         .with_column("cpu", DataType::Float64)
//!         .with_column("ram_mb", DataType::Int64),
//! )?;
//! let batch = Batch::new(
//!     vec![1_000, 2_000, 3_000],
//!     vec![
//!         ("cpu", Column::from(vec![0.31, 0.35, 0.33])),
//!         ("ram_mb", Column::from(vec![2048i64, 2051, 2049])),
//!     ],
//! )?;
//! w.write("vm01", &batch)?;
//! w.commit()?;
//!
//! // Edit: add, change and delete points, annotate an interval.
//! w.write_row("vm01", 1_500, &[("cpu", Value::from(0.9)), ("ram_mb", Value::from(4096i64))])?;
//! w.update("vm01", 3_000, &[("cpu", Value::from(0.30))])?;
//! w.delete("vm01", 2_000)?;
//! w.annotate("vm01", 1_400, 1_600, "spike", Some("backup job"))?;
//! w.close()?;
//!
//! // Read the latest version, or the version after the first commit.
//! let r = Reader::open("metrics.trosna")?;
//! let now = r.query("vm01")?.columns(["cpu"]).collect()?;
//! let before = r.query("vm01")?.as_of(1).collect()?;
//! assert_eq!(now.time(), &[1_000, 1_500, 3_000]);
//! assert_eq!(before.time(), &[1_000, 2_000, 3_000]);
//! # Ok::<(), trosna::Error>(())
//! ```
//!
//! The byte-level format is specified in `docs/SPEC.md` of the repository.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bits;
mod bytes;
mod catalog;
mod codec;
mod column;
mod encoding;
mod error;
mod fileio;
mod format;
mod history;
mod read;
mod reader;
mod scan;
mod segment;
mod tools;
mod types;
mod writer;

pub use codec::Codec;
pub use column::{Batch, Bitmap, Column, Utf8Values, Value, Values};
pub use encoding::{Encoding, EncodingPolicy};
pub use error::{Error, Result};
pub use format::{Hash, hash_hex, parse_hash_hex};
pub use history::{
    Annotation, AnnotationChange, ChangeKind, CommitChanges, CommitInfo, Diff, PointChange,
};
pub use reader::{Batches, BlockInfo, Query, ReadOptions, Reader, SegmentInfo};
pub use segment::Statistics;
pub use tools::{CompactReport, RecoverReport, VerifyReport, compact, recover, verify};
pub use types::{ColumnSchema, DataType, DeviceSchema, MAX_COLUMNS, TimeUnit};
pub use writer::{CommitOptions, RecoveryReport, WriteOptions, Writer};

/// The recommended file name extension, without the dot.
pub const EXTENSION: &str = "trosna";
