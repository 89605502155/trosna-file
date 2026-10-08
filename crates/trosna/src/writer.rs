//! Creating, appending to and editing Trosna files (DESIGN §7).

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::catalog::Catalog;
use crate::codec::Codec;
use crate::column::{Batch, Column, ColumnBuilder, Value};
use crate::encoding::EncodingPolicy;
use crate::error::{Error, Result};
use crate::fileio::read_vec_at;
use crate::format::{
    self, AnnotationOp, CommitRecord, DataHeader, FILE_HEADER_LEN, FrameKind, HASH_LEN, Hash,
    MAX_ROWS,
};
use crate::history::CommitInfo;
use crate::read::{self, ReadSpec};
use crate::scan::{self, Stop};
use crate::segment::{self, EncodeOptions};
use crate::types::DeviceSchema;

/// Options for [`Writer`].
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// General-purpose codec for segments (default: [`Codec::Zstd`]).
    pub codec: Codec,
    /// How column encodings are chosen (default: [`EncodingPolicy::Adaptive`]).
    pub encoding: EncodingPolicy,
    /// Maximum number of rows per block (default 65 536, at most 2^24).
    pub rows_per_block: usize,
    /// A device's buffer is written out once it holds about this many bytes
    /// (default 64 MiB).
    pub max_block_bytes: usize,
    /// Call `fsync` when committing (default `true`).
    pub sync: bool,
    /// [`Writer::create_with`] replaces an existing file (default `false`).
    pub overwrite: bool,
    /// [`Writer::open_with`] truncates a damaged file even if valid frames
    /// follow the damage, discarding them (default `false`).
    pub repair_corruption: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            codec: Codec::Zstd,
            encoding: EncodingPolicy::Adaptive,
            rows_per_block: 65_536,
            max_block_bytes: 64 << 20,
            sync: true,
            overwrite: false,
            repair_corruption: false,
        }
    }
}

/// Description of a commit.
#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    /// Commit message.
    pub message: Option<String>,
    /// Author of the changes.
    pub author: Option<String>,
    /// Commit time in nanoseconds since the Unix epoch (default: now).
    pub time: Option<i64>,
}

impl CommitOptions {
    /// A commit with a message.
    pub fn message(message: impl Into<String>) -> Self {
        CommitOptions {
            message: Some(message.into()),
            ..Self::default()
        }
    }

    /// Sets the author.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Sets the commit time (nanoseconds since the Unix epoch).
    pub fn with_time(mut self, time: i64) -> Self {
        self.time = Some(time);
        self
    }
}

/// What happened when a file that was not closed properly was opened for writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Bytes removed from the end of the file.
    pub truncated_bytes: u64,
    /// Complete frames of the interrupted transaction that were discarded.
    pub discarded_frames: u32,
    /// Why the scan of the file stopped.
    pub reason: String,
}

#[derive(Debug)]
struct DeviceBuffer {
    time: Vec<i64>,
    columns: Vec<ColumnBuilder>,
}

impl DeviceBuffer {
    fn new(schema: &DeviceSchema) -> Self {
        DeviceBuffer {
            time: Vec::new(),
            columns: schema
                .columns
                .iter()
                .map(|c| ColumnBuilder::new(c.data_type, 0))
                .collect(),
        }
    }

    fn byte_size(&self) -> usize {
        self.time.len() * 8
            + self
                .columns
                .iter()
                .map(ColumnBuilder::byte_size)
                .sum::<usize>()
    }
}

/// Converts a range of time stamps to inclusive bounds; `None` if it is empty.
pub(crate) fn inclusive_bounds(range: impl RangeBounds<i64>) -> Option<(i64, i64)> {
    let lo = match range.start_bound() {
        Bound::Included(&t) => t,
        Bound::Excluded(&t) => t.checked_add(1)?,
        Bound::Unbounded => i64::MIN,
    };
    let hi = match range.end_bound() {
        Bound::Included(&t) => t,
        Bound::Excluded(&t) => t.checked_sub(1)?,
        Bound::Unbounded => i64::MAX,
    };
    (lo <= hi).then_some((lo, hi))
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn lock(file: &File) -> Result<()> {
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => Error::Locked,
        std::fs::TryLockError::Error(e) => Error::Io(e),
    })
}

/// Writes and edits a Trosna file.
///
/// Changes are grouped into commits. Data written with [`Writer::write`] is
/// buffered and becomes durable and visible to readers when
/// [`Writer::commit`] (or [`Writer::close`]) returns. Dropping a writer
/// commits pending changes and finalizes the file, ignoring errors; call
/// [`Writer::close`] to observe them.
///
/// ```no_run
/// use trosna::{Batch, Column, DataType, DeviceSchema, TimeUnit, Writer};
///
/// let mut w = Writer::create("metrics.trosna")?;
/// w.create_device(
///     DeviceSchema::new("vm01", TimeUnit::Millisecond).with_column("cpu", DataType::Float64),
/// )?;
/// let batch = Batch::new(vec![1_000, 2_000], vec![("cpu", Column::from(vec![0.31, 0.35]))])?;
/// w.write("vm01", &batch)?;
/// w.close()?;
/// # Ok::<(), trosna::Error>(())
/// ```
#[derive(Debug)]
pub struct Writer {
    path: PathBuf,
    file: File,
    out: BufWriter<File>,
    pos: u64,
    catalog: Catalog,
    buffers: Vec<DeviceBuffer>,
    pending_tombstones: HashMap<u32, Vec<(i64, i64)>>,
    hasher: Sha256,
    tx_start: u64,
    /// `prev_hash` of the first commit when the file has no commits yet.
    chain_origin: Hash,
    dirty: bool,
    options: WriteOptions,
    recovery: Option<RecoveryReport>,
    poisoned: bool,
    closed: bool,
}

impl Writer {
    /// Creates a new file. Fails if it already exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Writer> {
        Self::create_with(path, WriteOptions::default())
    }

    /// Creates a new file with options.
    pub fn create_with(path: impl AsRef<Path>, options: WriteOptions) -> Result<Writer> {
        Self::check_options(&options)?;
        let path = path.as_ref();
        let file = if options.overwrite {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)?
        };
        lock(&file)?;
        file.set_len(0)?;
        let mut out = BufWriter::new(file.try_clone()?);
        out.write_all(&format::file_header())?;
        let mut writer = Self::new(path, file, out, FILE_HEADER_LEN, Catalog::new(), options);
        writer.dirty = true;
        Ok(writer)
    }

    /// Opens an existing file for appending and editing.
    ///
    /// If the file was not closed properly, its uncommitted tail is removed
    /// (see [`Writer::recovery`]). If damaged frames are followed by valid
    /// ones, the file is not modified and an error is returned unless
    /// [`WriteOptions::repair_corruption`] is set.
    pub fn open(path: impl AsRef<Path>) -> Result<Writer> {
        Self::open_with(path, WriteOptions::default())
    }

    /// Opens an existing file for appending and editing, with options.
    pub fn open_with(path: impl AsRef<Path>, options: WriteOptions) -> Result<Writer> {
        Self::check_options(&options)?;
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        lock(&file)?;
        let len = file.metadata()?.len();
        let header = read_vec_at(&file, 0, (FILE_HEADER_LEN.min(len)) as usize)?;
        format::check_file_header(&header)?;
        let indexed = match scan::load_index(&file, len) {
            Ok(found) => found,
            Err(e) if e.is_corruption() => None,
            Err(e) => return Err(e),
        };
        let (catalog, end, recovery) = match indexed {
            Some(catalog) => (catalog, len, None),
            None => {
                let outcome = scan::scan(&file, len, true, |_| Ok(()))?;
                if outcome.stop != Stop::EndOfFile
                    && !options.repair_corruption
                    && let Some(next) =
                        scan::find_valid_frame_after(&file, len, outcome.stopped_at)?
                {
                    return Err(Error::Corrupted {
                        offset: Some(outcome.stopped_at),
                        reason: format!(
                            "damaged frame followed by valid data at offset {next}; \
                                 refusing to truncate (use repair to discard everything after the damage)"
                        ),
                    });
                }
                let reason = match &outcome.stop {
                    Stop::EndOfFile => "the file was not finalized".to_owned(),
                    Stop::Truncated => "the last frame is incomplete".to_owned(),
                    Stop::Invalid(reason) => reason.clone(),
                };
                let report = RecoveryReport {
                    truncated_bytes: len - outcome.committed_end,
                    discarded_frames: outcome.discarded_frames,
                    reason,
                };
                if outcome.committed_end < len {
                    file.set_len(outcome.committed_end)?;
                    file.sync_all()?;
                }
                (outcome.catalog, outcome.committed_end, Some(report))
            }
        };
        let mut out = BufWriter::new(file.try_clone()?);
        out.seek(SeekFrom::Start(end))?;
        let mut writer = Self::new(path, file, out, end, catalog, options);
        writer.dirty = recovery.is_some();
        writer.recovery = recovery;
        Ok(writer)
    }

    fn check_options(options: &WriteOptions) -> Result<()> {
        if options.rows_per_block == 0 || options.rows_per_block > MAX_ROWS as usize {
            return Err(Error::InvalidArgument(format!(
                "rows_per_block must be between 1 and {MAX_ROWS}"
            )));
        }
        Ok(())
    }

    fn new(
        path: &Path,
        file: File,
        out: BufWriter<File>,
        pos: u64,
        catalog: Catalog,
        options: WriteOptions,
    ) -> Writer {
        let buffers = catalog
            .devices
            .iter()
            .map(|d| DeviceBuffer::new(&d.schema))
            .collect();
        Writer {
            path: path.to_owned(),
            file,
            out,
            pos,
            catalog,
            buffers,
            pending_tombstones: HashMap::new(),
            hasher: Sha256::new(),
            tx_start: pos,
            chain_origin: [0; HASH_LEN],
            dirty: false,
            options,
            recovery: None,
            poisoned: false,
            closed: false,
        }
    }

    /// Path of the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What was repaired when the file was opened, if anything.
    pub fn recovery(&self) -> Option<&RecoveryReport> {
        self.recovery.as_ref()
    }

    /// Schemas of all devices, including those created in the current transaction.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceSchema> + '_ {
        self.catalog.devices.iter().map(|d| d.schema.as_ref())
    }

    /// Schema of a device.
    pub fn device(&self, name: &str) -> Result<&DeviceSchema> {
        let id = self.catalog.device_id(name)?;
        Ok(&self.catalog.device(id).schema)
    }

    /// Number of the last commit (0 if there is none).
    pub fn head(&self) -> u64 {
        self.catalog.head().0
    }

    fn check_usable(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Runs an I/O step, poisoning the writer if it fails.
    fn guarded<T>(&mut self, step: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let result = step(self);
        if matches!(result, Err(Error::Io(_))) {
            self.poisoned = true;
        }
        result
    }

    /// Appends a frame to the file. `catalog_body` is what the catalogue
    /// records (the payload, or the header of a `Data` frame).
    fn write_frame(&mut self, kind: FrameKind, payload: &[u8], catalog_body: &[u8]) -> Result<u64> {
        let frame = format::encode_frame(kind, payload)?;
        let offset = self.pos;
        if let Err(e) = self.out.write_all(&frame) {
            self.poisoned = true;
            return Err(e.into());
        }
        self.pos += frame.len() as u64;
        if !kind.is_derived() && kind != FrameKind::Commit {
            self.hasher.update(&frame);
        }
        if !kind.is_derived() {
            self.catalog
                .apply(kind, offset, catalog_body, Some(payload.len() as u64))?;
        }
        Ok(offset)
    }

    // ------------------------------------------------------------ devices

    /// Creates a device.
    pub fn create_device(&mut self, schema: DeviceSchema) -> Result<()> {
        self.check_usable()?;
        schema.validate()?;
        if self.catalog.device_id(&schema.name).is_ok() {
            return Err(Error::DeviceExists(schema.name));
        }
        let id = self.catalog.devices.len() as u32;
        let payload = format::encode_device(id, &schema);
        self.guarded(|w| w.write_frame(FrameKind::Device, &payload, &payload))?;
        self.buffers.push(DeviceBuffer::new(&schema));
        Ok(())
    }

    /// Creates the device if it does not exist; otherwise checks that the
    /// existing device has the same time unit and columns.
    pub fn ensure_device(&mut self, schema: &DeviceSchema) -> Result<()> {
        match self.catalog.device_id(&schema.name) {
            Err(_) => self.create_device(schema.clone()),
            Ok(id) => {
                let existing = &self.catalog.device(id).schema;
                let same_columns = existing.columns.len() == schema.columns.len()
                    && existing
                        .columns
                        .iter()
                        .zip(&schema.columns)
                        .all(|(a, b)| a.name == b.name && a.data_type == b.data_type);
                if existing.time_unit != schema.time_unit || !same_columns {
                    return Err(Error::Schema(format!(
                        "device '{}' already exists with a different schema",
                        schema.name
                    )));
                }
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------ writing

    /// Writes rows. Columns are matched by name; columns of the device that
    /// the batch lacks are null. Rows may have any time stamps; a row with the
    /// time stamp of an existing point replaces it.
    pub fn write(&mut self, device: &str, batch: &Batch) -> Result<()> {
        self.check_usable()?;
        let id = self.catalog.device_id(device)?;
        let schema = self.catalog.device(id).schema.clone();
        let mut mapping: Vec<Option<&Column>> = vec![None; schema.columns.len()];
        for (name, column) in batch.names().iter().zip(batch.columns()) {
            let index = schema
                .column_index(name)
                .ok_or_else(|| Error::UnknownColumn {
                    device: device.to_owned(),
                    column: name.clone(),
                })?;
            let expected = schema.columns[index].data_type;
            if column.data_type() != expected {
                return Err(Error::TypeMismatch {
                    column: name.clone(),
                    expected,
                    actual: column.data_type(),
                });
            }
            mapping[index] = Some(column);
        }
        if batch.is_empty() {
            return Ok(());
        }
        let incoming: usize = batch.time().len() * 8
            + batch
                .columns()
                .iter()
                .map(|c| match c.as_utf8() {
                    Some(s) => s.data().len() + 4 * s.len(),
                    None => c.len() * c.data_type().plain_width(),
                })
                .sum::<usize>();
        let buffer = &self.buffers[id as usize];
        if !buffer.time.is_empty() && buffer.byte_size() + incoming > self.options.max_block_bytes {
            self.flush_device(id)?;
        }
        let buffer = &mut self.buffers[id as usize];
        let len = batch.len();
        for (builder, source) in buffer.columns.iter_mut().zip(&mapping) {
            match source {
                Some(column) => builder.extend_from(column, 0..len)?,
                None => (0..len).for_each(|_| builder.push_null()),
            }
        }
        buffer.time.extend_from_slice(batch.time());
        self.maybe_flush(id)
    }

    /// Writes one row. Values are given by column name and converted to the
    /// column type if that is lossless; missing columns are null.
    pub fn write_row(&mut self, device: &str, time: i64, values: &[(&str, Value)]) -> Result<()> {
        self.check_usable()?;
        let id = self.catalog.device_id(device)?;
        let row = self.row_from_named(
            id,
            vec![Value::Null; self.catalog.device(id).types.len()],
            values,
        )?;
        self.push_row(id, time, &row)
    }

    fn row_from_named(
        &self,
        id: u32,
        mut row: Vec<Value>,
        values: &[(&str, Value)],
    ) -> Result<Vec<Value>> {
        let schema = &self.catalog.device(id).schema;
        for (name, value) in values {
            let index = schema
                .column_index(name)
                .ok_or_else(|| Error::UnknownColumn {
                    device: schema.name.clone(),
                    column: (*name).to_owned(),
                })?;
            let data_type = schema.columns[index].data_type;
            row[index] = value
                .clone()
                .cast(data_type)
                .map_err(|_| Error::TypeMismatch {
                    column: (*name).to_owned(),
                    expected: data_type,
                    actual: value.data_type().unwrap_or(data_type),
                })?;
        }
        Ok(row)
    }

    fn push_row(&mut self, id: u32, time: i64, row: &[Value]) -> Result<()> {
        let buffer = &mut self.buffers[id as usize];
        // Type-check everything before modifying the buffer.
        for (builder, value) in buffer.columns.iter().zip(row) {
            if let Some(t) = value.data_type()
                && t != builder.data_type()
            {
                return Err(Error::TypeMismatch {
                    column: String::new(),
                    expected: builder.data_type(),
                    actual: t,
                });
            }
        }
        for (builder, value) in buffer.columns.iter_mut().zip(row) {
            builder.push_value(value)?;
        }
        buffer.time.push(time);
        self.maybe_flush(id)
    }

    fn maybe_flush(&mut self, id: u32) -> Result<()> {
        let buffer = &self.buffers[id as usize];
        if buffer.time.len() >= self.options.rows_per_block
            || buffer.byte_size() >= self.options.max_block_bytes
        {
            self.flush_device(id)?;
        }
        Ok(())
    }

    /// Writes the pending tombstones and buffered rows of a device.
    fn flush_device(&mut self, id: u32) -> Result<()> {
        if let Some(ranges) = self.pending_tombstones.remove(&id) {
            let payload = format::encode_tombstone(id, &ranges);
            self.guarded(|w| w.write_frame(FrameKind::Tombstone, &payload, &payload))?;
        }
        let schema = self.catalog.device(id).schema.clone();
        let buffer = std::mem::replace(&mut self.buffers[id as usize], DeviceBuffer::new(&schema));
        if buffer.time.is_empty() {
            return Ok(());
        }
        let (time, columns) = sort_rows(buffer)?;
        let options = EncodeOptions {
            policy: self.options.encoding,
            codec: self.options.codec,
        };
        let step = self.options.rows_per_block;
        let mut start = 0;
        while start < time.len() {
            let end = (start + step).min(time.len());
            let whole = start == 0 && end == time.len();
            let block_time = &time[start..end];
            let mut headers = Vec::with_capacity(columns.len() + 1);
            let mut segments = Vec::with_capacity(columns.len() + 1);
            let (h, s) = segment::encode_time(block_time, options)?;
            headers.push(h);
            segments.push(s);
            for column in &columns {
                let (h, s) = if whole {
                    segment::encode_column(column, options)?
                } else {
                    segment::encode_column(&column.slice(start..end), options)?
                };
                headers.push(h);
                segments.push(s);
            }
            let header = DataHeader {
                device_id: id,
                row_count: block_time.len() as u32,
                t_min: block_time[0],
                t_max: block_time[block_time.len() - 1],
                segments: headers,
            };
            let header_bytes = header.encode();
            let payload = format::encode_data_payload(&header, &segments);
            self.guarded(|w| w.write_frame(FrameKind::Data, &payload, &header_bytes))?;
            start = end;
        }
        Ok(())
    }

    // ------------------------------------------------------------ editing

    /// The current row at `time` (buffer, then file including uncommitted frames).
    fn current_row(&mut self, id: u32, time: i64) -> Result<Option<Vec<Value>>> {
        let buffer = &self.buffers[id as usize];
        if let Some(i) = buffer.time.iter().rposition(|&t| t == time) {
            return Ok(Some(buffer.columns.iter().map(|c| c.value(i)).collect()));
        }
        let deleted = self
            .pending_tombstones
            .get(&id)
            .is_some_and(|ranges| ranges.iter().any(|&(lo, hi)| lo <= time && time <= hi));
        if deleted {
            return Ok(None);
        }
        self.guarded(|w| Ok(w.out.flush()?))?;
        let columns = (0..self.catalog.device(id).types.len()).collect();
        let spec = ReadSpec {
            device_id: id,
            columns,
            lo: time,
            hi: time,
            limit: u64::MAX,
            verify: true,
        };
        let plan = read::plan(&self.catalog, spec);
        let batch = read::execute_all(&self.file, &self.catalog, &plan)?;
        Ok((!batch.is_empty()).then(|| batch.row(0)))
    }

    /// Changes values of the existing point at `time`. Fails with
    /// [`Error::PointNotFound`] if there is none.
    pub fn update(&mut self, device: &str, time: i64, values: &[(&str, Value)]) -> Result<()> {
        self.check_usable()?;
        let id = self.catalog.device_id(device)?;
        let current = self
            .current_row(id, time)?
            .ok_or_else(|| Error::PointNotFound {
                device: device.to_owned(),
                time,
            })?;
        let row = self.row_from_named(id, current, values)?;
        self.push_row(id, time, &row)
    }

    /// Reads the current values of the point at `time`, including changes
    /// not yet committed.
    pub fn get(&mut self, device: &str, time: i64) -> Result<Option<Vec<Value>>> {
        self.check_usable()?;
        let id = self.catalog.device_id(device)?;
        self.current_row(id, time)
    }

    /// Deletes the point at `time`, if any.
    pub fn delete(&mut self, device: &str, time: i64) -> Result<()> {
        self.delete_range(device, time..=time)
    }

    /// Deletes all points of a device inside a time range.
    pub fn delete_range(&mut self, device: &str, range: impl RangeBounds<i64>) -> Result<()> {
        self.check_usable()?;
        let id = self.catalog.device_id(device)?;
        let Some(bounds) = inclusive_bounds(range) else {
            return Ok(());
        };
        if !self.buffers[id as usize].time.is_empty() {
            // Rows written before the deletion must precede the tombstone in the log.
            self.flush_device(id)?;
        }
        self.pending_tombstones.entry(id).or_default().push(bounds);
        Ok(())
    }

    // ------------------------------------------------------------ annotations

    fn write_annotation_op(&mut self, op: AnnotationOp) -> Result<()> {
        let payload = format::encode_annotation_ops(std::slice::from_ref(&op));
        self.guarded(|w| w.write_frame(FrameKind::Annotation, &payload, &payload))?;
        Ok(())
    }

    fn check_interval(start: i64, end: i64) -> Result<()> {
        if start > end {
            return Err(Error::InvalidArgument(format!(
                "annotation start {start} is after its end {end}"
            )));
        }
        Ok(())
    }

    /// Adds an annotation `[start, end]` to a device and returns its identifier.
    pub fn annotate(
        &mut self,
        device: &str,
        start: i64,
        end: i64,
        label: &str,
        note: Option<&str>,
    ) -> Result<u64> {
        self.check_usable()?;
        Self::check_interval(start, end)?;
        let device_id = self.catalog.device_id(device)?;
        let id = self.catalog.max_annotation_id + 1;
        self.write_annotation_op(AnnotationOp::Upsert {
            id,
            device_id,
            start,
            end,
            label: label.to_owned(),
            note: note.map(str::to_owned),
        })?;
        Ok(id)
    }

    /// Replaces the interval, label and note of an annotation.
    pub fn update_annotation(
        &mut self,
        id: u64,
        start: i64,
        end: i64,
        label: &str,
        note: Option<&str>,
    ) -> Result<()> {
        self.check_usable()?;
        Self::check_interval(start, end)?;
        let device_id = self
            .catalog
            .live_annotation(id)
            .ok_or(Error::UnknownAnnotation(id))?
            .device_id;
        self.write_annotation_op(AnnotationOp::Upsert {
            id,
            device_id,
            start,
            end,
            label: label.to_owned(),
            note: note.map(str::to_owned),
        })
    }

    /// Removes an annotation.
    pub fn remove_annotation(&mut self, id: u64) -> Result<()> {
        self.check_usable()?;
        if self.catalog.live_annotation(id).is_none() {
            return Err(Error::UnknownAnnotation(id));
        }
        self.write_annotation_op(AnnotationOp::Remove { id })
    }

    /// Re-creates an annotation with a given identifier (used by compaction).
    pub(crate) fn restore_annotation(
        &mut self,
        id: u64,
        device: &str,
        start: i64,
        end: i64,
        label: &str,
        note: Option<&str>,
    ) -> Result<()> {
        let device_id = self.catalog.device_id(device)?;
        self.write_annotation_op(AnnotationOp::Upsert {
            id,
            device_id,
            start,
            end,
            label: label.to_owned(),
            note: note.map(str::to_owned),
        })
    }

    // ------------------------------------------------------------ metadata

    /// Current file metadata, including uncommitted changes.
    pub fn metadata(&self) -> BTreeMap<String, String> {
        self.catalog.metadata_at(u64::MAX)
    }

    /// Sets a file metadata entry.
    pub fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        self.check_usable()?;
        let mut metadata = self.metadata();
        metadata.insert(key.to_owned(), value.to_owned());
        self.write_metadata(&metadata)
    }

    /// Removes a file metadata entry.
    pub fn remove_metadata(&mut self, key: &str) -> Result<()> {
        self.check_usable()?;
        let mut metadata = self.metadata();
        if metadata.remove(key).is_some() {
            self.write_metadata(&metadata)?;
        }
        Ok(())
    }

    fn write_metadata(&mut self, metadata: &BTreeMap<String, String>) -> Result<()> {
        let payload = format::encode_meta(metadata);
        self.guarded(|w| w.write_frame(FrameKind::Meta, &payload, &payload))?;
        Ok(())
    }

    /// Sets the `prev_hash` of the first commit (provenance of compacted files).
    pub(crate) fn set_chain_origin(&mut self, origin: Hash) -> Result<()> {
        if !self.catalog.commits.is_empty() {
            return Err(Error::InvalidArgument(
                "the file already has commits".into(),
            ));
        }
        self.chain_origin = origin;
        Ok(())
    }

    // ------------------------------------------------------------ transactions

    /// Commits pending changes. Returns `None` if there was nothing to commit.
    pub fn commit(&mut self) -> Result<Option<CommitInfo>> {
        self.commit_with(CommitOptions::default())
    }

    /// Commits pending changes with a message, author or time.
    pub fn commit_with(&mut self, options: CommitOptions) -> Result<Option<CommitInfo>> {
        self.check_usable()?;
        for id in 0..self.buffers.len() as u32 {
            self.flush_device(id)?;
        }
        let frame_count = self.catalog.pending_frames();
        if frame_count == 0 {
            return Ok(None);
        }
        let sync = self.options.sync;
        self.guarded(|w| {
            w.out.flush()?;
            if sync {
                w.file.sync_data()?;
            }
            Ok(())
        })?;
        let (head, head_hash) = self.catalog.head();
        let first_offset = self.catalog.pending_first_offset().unwrap_or(self.pos);
        let record = CommitRecord {
            number: head + 1,
            time: options.time.unwrap_or_else(now_nanos),
            first_offset,
            frame_count,
            prev_hash: if head == 0 {
                self.chain_origin
            } else {
                head_hash
            },
            content_hash: std::mem::take(&mut self.hasher).finalize().into(),
            author: options.author,
            message: options.message,
            hash: [0; HASH_LEN],
        }
        .seal();
        let payload = record.encode();
        self.guarded(|w| w.write_frame(FrameKind::Commit, &payload, &payload))?;
        self.guarded(|w| {
            w.out.flush()?;
            if sync {
                w.file.sync_data()?;
            }
            Ok(())
        })?;
        self.tx_start = self.pos;
        self.dirty = true;
        Ok(Some(CommitInfo::from_catalog(
            &self.catalog,
            self.catalog.commits.len() - 1,
        )))
    }

    /// Discards all changes since the last commit.
    pub fn rollback(&mut self) -> Result<()> {
        for (id, device) in self.catalog.devices.iter().enumerate() {
            self.buffers[id] = DeviceBuffer::new(&device.schema);
        }
        self.pending_tombstones.clear();
        let start = self.tx_start;
        self.guarded(|w| {
            w.out.flush()?;
            w.file.set_len(start)?;
            w.out.seek(SeekFrom::Start(start))?;
            Ok(())
        })?;
        self.pos = start;
        self.catalog.truncate(start);
        self.buffers.truncate(self.catalog.devices.len());
        self.hasher = Sha256::new();
        Ok(())
    }

    fn write_index(&mut self) -> Result<()> {
        let entries = self.catalog.index_entries();
        let (head, head_hash) = self.catalog.head();
        let payload = format::encode_index(head, &head_hash, &entries);
        let index_offset = self.pos;
        self.guarded(|w| {
            w.write_frame(FrameKind::Index, &payload, &[])?;
            w.write_frame(FrameKind::Footer, &format::encode_footer(index_offset), &[])?;
            w.out.flush()?;
            w.file.sync_all()?;
            Ok(())
        })
    }

    /// Commits pending changes, writes the index and closes the file.
    pub fn close(mut self) -> Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.check_usable()?;
        self.commit()?;
        if self.dirty {
            self.write_index()?;
        }
        self.closed = true;
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.closed || self.poisoned {
            return;
        }
        if std::thread::panicking() {
            // Do not commit the work of a transaction that was interrupted by a panic.
            let _ = self.rollback();
        }
        let _ = self.finish();
    }
}

/// Sorts buffered rows by time; for duplicate time stamps the last write wins.
fn sort_rows(buffer: DeviceBuffer) -> Result<(Vec<i64>, Vec<Column>)> {
    let DeviceBuffer { time, columns } = buffer;
    let columns: Vec<Column> = columns.into_iter().map(ColumnBuilder::finish).collect();
    if time.windows(2).all(|w| w[0] < w[1]) {
        return Ok((time, columns));
    }
    let mut order: Vec<usize> = (0..time.len()).collect();
    order.sort_by_key(|&i| time[i]);
    // Keep the last of each run of equal time stamps (the sort is stable).
    let mut keep: Vec<usize> = Vec::with_capacity(order.len());
    for (k, &i) in order.iter().enumerate() {
        if order.get(k + 1).is_none_or(|&next| time[next] != time[i]) {
            keep.push(i);
        }
    }
    let sorted_time = keep.iter().map(|&i| time[i]).collect();
    let mut sorted_columns = Vec::with_capacity(columns.len());
    for column in &columns {
        let mut builder = ColumnBuilder::new(column.data_type(), keep.len());
        for &i in &keep {
            builder.push_from(column, i)?;
        }
        sorted_columns.push(builder.finish());
    }
    Ok((sorted_time, sorted_columns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_conversion() {
        assert_eq!(inclusive_bounds(1..5), Some((1, 4)));
        assert_eq!(inclusive_bounds(1..=5), Some((1, 5)));
        assert_eq!(inclusive_bounds(..), Some((i64::MIN, i64::MAX)));
        assert_eq!(inclusive_bounds(5..5), None);
        assert_eq!(
            inclusive_bounds((Bound::Excluded(i64::MAX), Bound::Unbounded)),
            None
        );
    }

    #[test]
    fn sorting_keeps_last_duplicate() {
        let schema = DeviceSchema::new("d", crate::TimeUnit::Second)
            .with_column("x", crate::DataType::Int64);
        let mut buffer = DeviceBuffer::new(&schema);
        for (t, v) in [(3, 30), (1, 10), (3, 31), (2, 20), (1, 11)] {
            buffer.time.push(t);
            buffer.columns[0].push_value(&Value::Int64(v)).unwrap();
        }
        let (time, columns) = sort_rows(buffer).unwrap();
        assert_eq!(time, vec![1, 2, 3]);
        assert_eq!(columns[0], Column::from(vec![11i64, 20, 31]));
    }
}
