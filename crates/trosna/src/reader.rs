//! Reading Trosna files.

use std::collections::BTreeMap;
use std::fs::File;
use std::ops::{Deref, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::catalog::Catalog;
use crate::codec::Codec;
use crate::column::Batch;
use crate::encoding::Encoding;
use crate::error::{Error, Result};
use crate::fileio::read_vec_at;
use crate::format::{self, FILE_HEADER_LEN};
use crate::history::{self, Annotation, CommitInfo, Diff};
use crate::read::{self, Plan, ReadSpec};
use crate::scan;
use crate::segment::Statistics;
use crate::types::{DataType, DeviceSchema};
use crate::writer::inclusive_bounds;

/// Options for [`Reader::open_with`].
#[derive(Debug, Clone)]
pub struct ReadOptions {
    /// Fail with [`Error::NotFinalized`] instead of scanning a file that was
    /// not closed properly (default `false`).
    pub strict: bool,
    /// Verify the CRC32C checksum of every segment that is read (default `true`).
    pub verify_checksums: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions {
            strict: false,
            verify_checksums: true,
        }
    }
}

/// Description of a stored block, for inspection and statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockInfo {
    /// Offset of the `Data` frame.
    pub offset: u64,
    /// Commit that wrote the block.
    pub commit: Option<u64>,
    /// Number of rows.
    pub rows: u32,
    /// First time stamp.
    pub t_min: i64,
    /// Last time stamp.
    pub t_max: i64,
    /// The time segment followed by the value segments.
    pub segments: Vec<SegmentInfo>,
}

/// Description of a stored segment.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentInfo {
    /// Column name.
    pub column: String,
    /// Column type, `None` for the time column.
    pub data_type: Option<DataType>,
    /// Encoding chosen for the segment.
    pub encoding: Encoding,
    /// Codec applied after encoding.
    pub codec: Codec,
    /// Bytes stored in the file.
    pub stored_bytes: u32,
    /// Bytes after decompression (validity bitmap plus encoded values).
    pub encoded_bytes: u32,
    /// Number of null values.
    pub null_count: u32,
    /// Minimum and maximum of the values, if recorded.
    pub statistics: Option<Statistics>,
}

/// A read-only view of a Trosna file.
///
/// A reader sees the file as it was when opened: later commits by a writer
/// are not visible, and the data it has seen is never changed underneath it.
///
/// ```no_run
/// let reader = trosna::Reader::open("metrics.trosna")?;
/// let batch = reader.query("vm01")?.columns(["cpu"]).time_range(1_000..=2_000).collect()?;
/// println!("{} rows", batch.len());
/// # Ok::<(), trosna::Error>(())
/// ```
#[derive(Debug)]
pub struct Reader {
    path: PathBuf,
    file: File,
    catalog: Catalog,
    finalized: bool,
    version: (u8, u8),
    len: u64,
    verify: bool,
}

impl Reader {
    /// Opens a file.
    pub fn open(path: impl AsRef<Path>) -> Result<Reader> {
        Self::open_with(path, ReadOptions::default())
    }

    /// Opens a file with options.
    pub fn open_with(path: impl AsRef<Path>, options: ReadOptions) -> Result<Reader> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < FILE_HEADER_LEN {
            return Err(Error::NotTrosna);
        }
        let header = read_vec_at(&file, 0, FILE_HEADER_LEN as usize)?;
        let version = format::check_file_header(&header)?;
        let (catalog, finalized) = match scan::load_index(&file, len) {
            Ok(Some(catalog)) => (catalog, true),
            Ok(None) if options.strict => return Err(Error::NotFinalized),
            Err(e) if options.strict || !e.is_corruption() => return Err(e),
            _ => (scan::scan(&file, len, false, |_| Ok(()))?.catalog, false),
        };
        Ok(Reader {
            path: path.to_owned(),
            file,
            catalog,
            finalized,
            version,
            len,
            verify: options.verify_checksums,
        })
    }

    /// Path of the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// False if the file was not closed properly and its contents were
    /// recovered by scanning (the writer crashed or is still writing).
    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Format version from the file header, `(major, minor)`.
    pub fn format_version(&self) -> (u8, u8) {
        self.version
    }

    /// Size of the file in bytes when it was opened.
    pub fn file_size(&self) -> u64 {
        self.len
    }

    /// File metadata of the latest version.
    pub fn metadata(&self) -> BTreeMap<String, String> {
        self.catalog.metadata_at(u64::MAX)
    }

    /// File metadata as of a commit.
    pub fn metadata_as_of(&self, commit: u64) -> Result<BTreeMap<String, String>> {
        Ok(self.catalog.metadata_at(self.catalog.limit(Some(commit))?))
    }

    /// Schemas of all devices.
    pub fn devices(&self) -> impl ExactSizeIterator<Item = &DeviceSchema> + '_ {
        self.catalog.devices.iter().map(|d| d.schema.as_ref())
    }

    /// Schema of a device.
    pub fn device(&self, name: &str) -> Result<&DeviceSchema> {
        Ok(&self.catalog.device(self.catalog.device_id(name)?).schema)
    }

    /// All commits, oldest first.
    pub fn commits(&self) -> Vec<CommitInfo> {
        (0..self.catalog.commits.len())
            .map(|i| CommitInfo::from_catalog(&self.catalog, i))
            .collect()
    }

    /// A commit by number.
    pub fn commit(&self, number: u64) -> Result<CommitInfo> {
        if number == 0 || number as usize > self.catalog.commits.len() {
            return Err(Error::UnknownCommit(number));
        }
        Ok(CommitInfo::from_catalog(&self.catalog, number as usize - 1))
    }

    /// The latest commit.
    pub fn head(&self) -> Option<CommitInfo> {
        self.catalog
            .commits
            .len()
            .checked_sub(1)
            .map(|i| CommitInfo::from_catalog(&self.catalog, i))
    }

    /// The last commit made at or before `time` (nanoseconds since the Unix
    /// epoch), i.e. the version of the file at that moment; `0` if none.
    pub fn commit_at(&self, time: i64) -> u64 {
        self.catalog
            .commits
            .iter()
            .filter(|c| c.record.time <= time)
            .map(|c| c.record.number)
            .max()
            .unwrap_or(0)
    }

    /// True if the device existed in version `commit` of the file.
    pub fn device_exists_at(&self, name: &str, commit: u64) -> bool {
        let (Ok(id), Ok(limit)) = (
            self.catalog.device_id(name),
            self.catalog.limit(Some(commit)),
        ) else {
            return false;
        };
        self.catalog.device(id).offset < limit
    }

    /// Starts a query of a device. All columns, all time stamps and the latest
    /// version are selected until restricted.
    pub fn query(&self, device: &str) -> Result<Query<'_>> {
        Ok(Query {
            reader: self,
            device_id: self.catalog.device_id(device)?,
            columns: None,
            bounds: Some((i64::MIN, i64::MAX)),
            as_of: None,
        })
    }

    /// Runs a query whose batches hold a shared handle to the reader instead
    /// of borrowing it, so the iterator can be stored or sent to another
    /// thread (as language bindings need); `configure` restricts the query.
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use trosna::Reader;
    ///
    /// let reader = Arc::new(Reader::open("metrics.trosna")?);
    /// let batches = Reader::query_shared(&reader, "vm01", |q| q.columns(["cpu"]))?;
    /// drop(reader); // the iterator keeps the file open
    /// for batch in batches {
    ///     println!("{} rows", batch?.len());
    /// }
    /// # Ok::<(), trosna::Error>(())
    /// ```
    pub fn query_shared(
        reader: &Arc<Reader>,
        device: &str,
        configure: impl FnOnce(Query<'_>) -> Query<'_>,
    ) -> Result<Batches<'static>> {
        let plan = configure(reader.query(device)?).plan(false)?;
        Ok(Batches {
            reader: ReaderRef::Shared(Arc::clone(reader)),
            plan,
            next: 0,
        })
    }

    /// Reads all columns of a device.
    pub fn read(&self, device: &str) -> Result<Batch> {
        self.query(device)?.collect()
    }

    /// Annotations of one device or of all devices, as of a commit or the latest.
    pub fn annotations(&self, device: Option<&str>, as_of: Option<u64>) -> Result<Vec<Annotation>> {
        let device_id = device.map(|d| self.catalog.device_id(d)).transpose()?;
        let limit = self.catalog.limit(as_of)?;
        Ok(self
            .catalog
            .annotations_at(limit)
            .iter()
            .filter(|(_, s)| device_id.is_none_or(|d| s.device_id == d))
            .map(|(&id, s)| Annotation::from_state(&self.catalog, id, s))
            .collect())
    }

    /// Differences of a device between commit `from` and commit `to`
    /// (the latest if `None`). Commit 0 is the empty file.
    pub fn diff(&self, device: &str, from: u64, to: Option<u64>) -> Result<Diff> {
        let device_id = self.catalog.device_id(device)?;
        let to = to.unwrap_or(self.catalog.head().0);
        history::diff(&self.file, &self.catalog, device_id, from, to)
    }

    /// The stored blocks of a device, in file order.
    pub fn blocks(&self, device: &str) -> Result<Vec<BlockInfo>> {
        let id = self.catalog.device_id(device)?;
        let info = self.catalog.device(id);
        Ok(info
            .data
            .iter()
            .map(|&i| {
                let entry = &self.catalog.data[i];
                let segments = entry
                    .header
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(k, s)| SegmentInfo {
                        column: if k == 0 {
                            info.schema.time_name.clone()
                        } else {
                            info.schema.columns[k - 1].name.clone()
                        },
                        data_type: k.checked_sub(1).map(|c| info.types[c]),
                        encoding: s.encoding,
                        codec: s.codec,
                        stored_bytes: s.stored_len,
                        encoded_bytes: s.raw_len,
                        null_count: s.null_count,
                        statistics: s.stats,
                    })
                    .collect();
                BlockInfo {
                    offset: entry.offset,
                    commit: self.catalog.commit_of(entry.offset),
                    rows: entry.header.row_count,
                    t_min: entry.header.t_min,
                    t_max: entry.header.t_max,
                    segments,
                }
            })
            .collect())
    }
}

/// A query of one device; see [`Reader::query`].
#[derive(Debug, Clone)]
pub struct Query<'r> {
    reader: &'r Reader,
    device_id: u32,
    columns: Option<Vec<String>>,
    bounds: Option<(i64, i64)>,
    as_of: Option<u64>,
}

impl<'r> Query<'r> {
    /// Selects value columns by name (the time column is always included).
    pub fn columns<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.columns = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Restricts the time stamps to a range, e.g. `a..=b`, `a..`, `..b`.
    pub fn time_range(mut self, range: impl RangeBounds<i64>) -> Self {
        self.bounds = inclusive_bounds(range);
        self
    }

    /// Reads the version of the file as of a commit (`0` is the empty file).
    pub fn as_of(mut self, commit: u64) -> Self {
        self.as_of = Some(commit);
        self
    }

    /// Schema of the queried device.
    pub fn device(&self) -> &'r DeviceSchema {
        &self.reader.catalog.device(self.device_id).schema
    }

    fn plan(&self, count_only: bool) -> Result<Plan> {
        let schema = self.device();
        let columns = if count_only {
            Vec::new()
        } else {
            match &self.columns {
                None => (0..schema.columns.len()).collect(),
                Some(names) => names
                    .iter()
                    .map(|name| {
                        schema
                            .column_index(name)
                            .ok_or_else(|| Error::UnknownColumn {
                                device: schema.name.clone(),
                                column: name.clone(),
                            })
                    })
                    .collect::<Result<_>>()?,
            }
        };
        let (lo, hi) = self.bounds.unwrap_or((1, 0));
        let spec = ReadSpec {
            device_id: self.device_id,
            columns,
            lo,
            hi,
            limit: self.reader.catalog.limit(self.as_of)?,
            verify: self.reader.verify,
        };
        Ok(read::plan(&self.reader.catalog, spec))
    }

    /// Returns the result as a stream of batches in time order, roughly one per stored block.
    pub fn batches(&self) -> Result<Batches<'r>> {
        Ok(Batches {
            reader: ReaderRef::Borrowed(self.reader),
            plan: self.plan(false)?,
            next: 0,
        })
    }

    /// Returns the whole result as one batch.
    pub fn collect(&self) -> Result<Batch> {
        read::execute_all(&self.reader.file, &self.reader.catalog, &self.plan(false)?)
    }

    /// Counts the points without reading value columns.
    pub fn count(&self) -> Result<u64> {
        let plan = self.plan(true)?;
        let mut total = 0;
        for cluster in &plan.clusters {
            if let Some(batch) =
                read::execute(&self.reader.file, &self.reader.catalog, &plan, cluster)?
            {
                total += batch.len() as u64;
            }
        }
        Ok(total)
    }
}

/// A borrowed or shared reader.
#[derive(Debug)]
enum ReaderRef<'r> {
    Borrowed(&'r Reader),
    Shared(Arc<Reader>),
}

impl Deref for ReaderRef<'_> {
    type Target = Reader;

    fn deref(&self) -> &Reader {
        match self {
            ReaderRef::Borrowed(r) => r,
            ReaderRef::Shared(r) => r,
        }
    }
}

/// Iterator over the batches of a query.
#[derive(Debug)]
pub struct Batches<'r> {
    reader: ReaderRef<'r>,
    plan: Plan,
    next: usize,
}

impl Batches<'_> {
    /// Names of the selected value columns.
    pub fn column_names(&self) -> &[String] {
        &self.plan.names
    }

    /// Types of the selected value columns.
    pub fn column_types(&self) -> &[DataType] {
        &self.plan.types
    }

    /// Schema of the queried device.
    pub fn device(&self) -> &DeviceSchema {
        &self.reader.catalog.device(self.plan.spec.device_id).schema
    }

    /// An empty batch with the selected columns.
    pub fn empty_batch(&self) -> Batch {
        Batch::empty(self.plan.names.clone(), &self.plan.types)
    }
}

impl Iterator for Batches<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.plan.clusters.len() {
            let cluster = &self.plan.clusters[self.next];
            self.next += 1;
            match read::execute(&self.reader.file, &self.reader.catalog, &self.plan, cluster) {
                Ok(None) => continue,
                Ok(Some(batch)) => return Some(Ok(batch)),
                Err(e) => {
                    self.next = self.plan.clusters.len();
                    return Some(Err(e));
                }
            }
        }
        None
    }
}
