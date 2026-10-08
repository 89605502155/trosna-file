//! `trosna`: command-line tool for Trosna time-series files.
//!
//! Large inputs are processed in batches of bounded size (CSV import reads
//! the file twice: once to infer the types, once to convert it), so memory use
//! does not grow with the size of the data, except for `cat --format table`,
//! which is limited by `--limit`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow_array::timezone::Tz;
use arrow_array::{Array, ArrayRef, RecordBatch, RecordBatchReader, StringArray};
use arrow_schema::{DataType as ArrowType, Field, Schema};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone};
use clap::{Args, Parser, Subcommand, ValueEnum};
use trosna::{
    Batch, ChangeKind, Codec, Column, CommitOptions, DataType, DeviceSchema, Reader, TimeUnit,
    Value, WriteOptions, Writer,
};

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

/// Rows per batch when importing CSV: bounds the memory used by conversion.
const CSV_BATCH_ROWS: usize = 65_536;

#[derive(Debug, Parser)]
#[command(
    name = "trosna",
    version,
    about = "Work with Trosna time-series files (.trosna)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Convert CSV to Trosna or Trosna to CSV (by the input's extension).
    Convert(ConvertArgs),
    /// Show the devices, columns, sizes and history of a file.
    Info {
        /// Trosna file.
        file: PathBuf,
    },
    /// Print the points of a device as CSV or as a table.
    Cat(CatArgs),
    /// Add (or overwrite) a point.
    Insert(EditPointArgs),
    /// Change some values of an existing point.
    Update(EditPointArgs),
    /// Delete a point or all points in a time range.
    Delete(DeleteArgs),
    /// Add an annotation (a labelled time interval), or change one with --id.
    Annotate(AnnotateArgs),
    /// List annotations.
    Annotations {
        /// Trosna file.
        file: PathBuf,
        /// Only annotations of this device.
        #[arg(long)]
        device: Option<String>,
        /// Show the annotations as they were after this commit.
        #[arg(long)]
        as_of: Option<u64>,
    },
    /// Remove an annotation.
    Unannotate {
        /// Trosna file.
        file: PathBuf,
        /// Annotation identifier.
        id: u64,
        #[command(flatten)]
        commit: CommitArgs,
    },
    /// Show the commit history.
    Log {
        /// Trosna file.
        file: PathBuf,
    },
    /// Show how the points and annotations of a device changed between commits.
    Diff {
        /// Trosna file.
        file: PathBuf,
        /// Device (needed if the file has several).
        #[arg(long)]
        device: Option<String>,
        /// Older commit (0 is the empty file).
        #[arg(long)]
        from: u64,
        /// Newer commit (the latest by default).
        #[arg(long)]
        to: Option<u64>,
    },
    /// Check every checksum, the hash chain and the decoding of all data.
    Verify {
        /// Trosna file.
        file: PathBuf,
    },
    /// Finalize a file whose writer was interrupted.
    Recover {
        /// Trosna file.
        file: PathBuf,
        /// Also discard everything after a damaged frame in the middle of the file.
        #[arg(long)]
        force: bool,
    },
    /// Copy the latest (or an older) version into a new file without history.
    Compact {
        /// Source file.
        source: PathBuf,
        /// New file.
        target: PathBuf,
        /// Version to keep (the latest by default).
        #[arg(long)]
        as_of: Option<u64>,
        /// Overwrite the target if it exists.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
struct CommitArgs {
    /// Commit message.
    #[arg(short, long)]
    message: Option<String>,
    /// Author recorded in the commit.
    #[arg(long)]
    author: Option<String>,
}

impl CommitArgs {
    fn options(&self) -> CommitOptions {
        CommitOptions {
            message: self.message.clone(),
            author: self.author.clone(),
            ..CommitOptions::default()
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CodecArg {
    None,
    Lz4,
    Zstd,
}

#[derive(Debug, Args)]
struct ConvertArgs {
    /// Input file (.trosna, or CSV).
    input: PathBuf,
    /// Output file (CSV, or .trosna).
    output: PathBuf,
    /// Device name (CSV → Trosna: the file name by default).
    #[arg(long)]
    device: Option<String>,
    /// Name of the time column in the CSV (the first column by default).
    #[arg(long)]
    time: Option<String>,
    /// Unit of integer time stamps in the CSV (guessed from their size by default),
    /// or the unit to store date-time text in (ms, or finer if needed, by default).
    #[arg(long)]
    unit: Option<String>,
    /// Field delimiter of the CSV.
    #[arg(long, default_value = ",")]
    delimiter: char,
    /// Add the data to an existing Trosna file.
    #[arg(long)]
    append: bool,
    /// Overwrite the output if it exists.
    #[arg(long)]
    force: bool,
    /// Compression of the Trosna file.
    #[arg(long, value_enum, default_value = "zstd")]
    codec: CodecArg,
    /// Trosna → CSV: write time stamps as integers in the device's unit.
    #[arg(long)]
    raw_time: bool,
    /// Trosna → CSV: export the data as it was after this commit.
    #[arg(long)]
    as_of: Option<u64>,
    #[command(flatten)]
    commit: CommitArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Csv,
    Table,
}

#[derive(Debug, Args)]
struct CatArgs {
    /// Trosna file.
    file: PathBuf,
    /// Device (needed if the file has several).
    #[arg(long)]
    device: Option<String>,
    /// Comma-separated value columns (all by default).
    #[arg(long, value_delimiter = ',')]
    columns: Option<Vec<String>>,
    /// First time stamp (inclusive).
    #[arg(long)]
    from: Option<String>,
    /// Last time stamp (inclusive).
    #[arg(long)]
    to: Option<String>,
    /// Show the data as it was after this commit.
    #[arg(long)]
    as_of: Option<u64>,
    /// Output format.
    #[arg(long, value_enum, default_value = "csv")]
    format: Format,
    /// Write time stamps as integers in the device's unit.
    #[arg(long)]
    raw_time: bool,
    /// Maximum number of rows (default: all for CSV, 100 for a table).
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Args)]
struct EditPointArgs {
    /// Trosna file.
    file: PathBuf,
    /// Device (needed if the file has several).
    #[arg(long)]
    device: Option<String>,
    /// Time stamp: date-time text or an integer in the device's unit.
    #[arg(long)]
    time: String,
    /// Values as column=value; an empty value is a null.
    #[arg(required = true)]
    values: Vec<String>,
    #[command(flatten)]
    commit: CommitArgs,
}

#[derive(Debug, Args)]
struct DeleteArgs {
    /// Trosna file.
    file: PathBuf,
    /// Device (needed if the file has several).
    #[arg(long)]
    device: Option<String>,
    /// Delete the point at this time.
    #[arg(long, conflicts_with_all = ["from", "to"])]
    time: Option<String>,
    /// Delete points from this time (inclusive).
    #[arg(long)]
    from: Option<String>,
    /// Delete points up to this time (inclusive).
    #[arg(long)]
    to: Option<String>,
    #[command(flatten)]
    commit: CommitArgs,
}

#[derive(Debug, Args)]
struct AnnotateArgs {
    /// Trosna file.
    file: PathBuf,
    /// Change the annotation with this identifier instead of adding one;
    /// options that are not given keep their values.
    #[arg(long)]
    id: Option<u64>,
    /// Device (needed for a new annotation if the file has several).
    #[arg(long)]
    device: Option<String>,
    /// Start of the interval.
    #[arg(long)]
    start: Option<String>,
    /// End of the interval (the start by default).
    #[arg(long)]
    end: Option<String>,
    /// Label.
    #[arg(long)]
    label: Option<String>,
    /// Free-text note.
    #[arg(long)]
    note: Option<String>,
    #[command(flatten)]
    commit: CommitArgs,
}

/// Set when standard output was closed by the reader (e.g. `trosna cat … | head`).
static BROKEN_PIPE: AtomicBool = AtomicBool::new(false);

/// Standard output that records a broken pipe, which Arrow's CSV writer would
/// otherwise report only as a generic CSV error.
struct Stdout(io::StdoutLock<'static>);

impl Stdout {
    fn new() -> Self {
        Stdout(io::stdout().lock())
    }

    fn note<T>(result: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &result
            && e.kind() == io::ErrorKind::BrokenPipe
        {
            BROKEN_PIPE.store(true, Ordering::Relaxed);
        }
        result
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Self::note(self.0.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        Self::note(self.0.flush())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => code,
        Err(e) => {
            if BROKEN_PIPE.load(Ordering::Relaxed) || is_broken_pipe(e.as_ref()) {
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn is_broken_pipe(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            return io.kind() == io::ErrorKind::BrokenPipe;
        }
        if let Some(arrow_schema::ArrowError::IoError(_, io)) = err.downcast_ref() {
            return io.kind() == io::ErrorKind::BrokenPipe;
        }
        source = err.source();
    }
    false
}

fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Convert(args) => convert(&args)?,
        Command::Info { file } => info(&file)?,
        Command::Cat(args) => cat(&args)?,
        Command::Insert(args) => edit_point(&args, false)?,
        Command::Update(args) => edit_point(&args, true)?,
        Command::Delete(args) => delete(&args)?,
        Command::Annotate(args) => annotate(&args)?,
        Command::Annotations {
            file,
            device,
            as_of,
        } => annotations(&file, device.as_deref(), as_of)?,
        Command::Unannotate { file, id, commit } => {
            let mut writer = open_writer(&file)?;
            writer.remove_annotation(id)?;
            finish(writer, &commit)?;
        }
        Command::Log { file } => log(&file)?,
        Command::Diff {
            file,
            device,
            from,
            to,
        } => diff(&file, device.as_deref(), from, to)?,
        Command::Verify { file } => return verify(&file),
        Command::Recover { file, force } => recover(&file, force)?,
        Command::Compact {
            source,
            target,
            as_of,
            force,
        } => {
            let options = WriteOptions {
                overwrite: force,
                ..WriteOptions::default()
            };
            let r = trosna::compact(&source, &target, as_of, options)?;
            println!(
                "compacted commit {} ({}) of {} into {}: {} points, {} annotations, {} → {}",
                r.version,
                &trosna::hash_hex(&r.origin)[..12],
                source.display(),
                target.display(),
                r.rows,
                r.annotations,
                human_bytes(r.bytes_before),
                human_bytes(r.bytes_after),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- helpers

fn is_trosna_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(trosna::EXTENSION))
}

/// The device to work with: the named one, or the only one in the file.
fn pick_device<'a>(
    mut devices: impl ExactSizeIterator<Item = &'a DeviceSchema>,
    name: Option<&str>,
) -> Result<&'a DeviceSchema> {
    if let Some(name) = name {
        return devices
            .find(|d| d.name == name)
            .ok_or_else(|| trosna::Error::UnknownDevice(name.to_owned()).into());
    }
    match devices.len() {
        0 => Err("the file has no devices".into()),
        1 => Ok(devices.next().expect("one device")),
        _ => {
            let names: Vec<_> = devices.map(|d| d.name.as_str()).collect();
            Err(format!(
                "the file has several devices ({}); choose one with --device",
                names.join(", ")
            )
            .into())
        }
    }
}

fn open_writer(path: &Path) -> Result<Writer> {
    let writer = Writer::open(path)?;
    if let Some(r) = writer.recovery() {
        eprintln!(
            "note: removed an unfinished transaction ({} bytes): {}",
            r.truncated_bytes, r.reason
        );
    }
    Ok(writer)
}

/// Commits the changes and closes the file.
fn finish(mut writer: Writer, commit: &CommitArgs) -> Result<()> {
    if writer.commit_with(commit.options())?.is_none() {
        eprintln!("nothing changed");
    }
    writer.close()?;
    Ok(())
}

/// "1 point", "2 points".
fn count(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------- time

/// Number of digits after the decimal point of the seconds in date-time text.
fn fraction_digits(s: &str) -> usize {
    s.find('.').map_or(0, |dot| {
        s[dot + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count()
    })
}

/// Parsed time text.
#[derive(Debug, Clone, Copy)]
enum TimeText {
    /// An integer in some unit.
    Integer(i64),
    /// A moment with a UTC offset.
    Zoned(DateTime<FixedOffset>),
    /// A date and time without an offset.
    Naive(NaiveDateTime),
}

fn parse_time_text(s: &str) -> Option<TimeText> {
    let s = s.trim();
    if let Ok(v) = s.parse::<i64>() {
        return Some(TimeText::Integer(v));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(TimeText::Zoned(t));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
    ] {
        if let Ok(t) = DateTime::parse_from_str(s, format) {
            return Some(TimeText::Zoned(t));
        }
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, format) {
            return Some(TimeText::Naive(t));
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(TimeText::Naive)
}

/// Converts time text into time stamps of one device.
#[derive(Debug, Clone)]
struct TimeParser {
    /// Unit of the stored time stamps.
    unit: TimeUnit,
    /// Unit of integers in the text.
    int_unit: TimeUnit,
    /// Time zone of text without an offset (UTC if `None`).
    tz: Option<Tz>,
}

impl TimeParser {
    fn for_device(device: &DeviceSchema) -> Result<Self> {
        Ok(TimeParser {
            unit: device.time_unit,
            int_unit: device.time_unit,
            tz: parse_tz(device.timezone.as_deref())?,
        })
    }

    fn parse(&self, s: &str, rounding: Rounding) -> Result<i64> {
        let text = parse_time_text(s).ok_or_else(|| {
            format!("cannot read '{s}' as a time (expected e.g. 2026-10-08T10:00:00+03:00, 2026-10-08 10:00:00 or an integer)")
        })?;
        let (secs, nanos) = match text {
            TimeText::Integer(v) if self.int_unit == self.unit => return Ok(v),
            TimeText::Integer(v) => {
                let per = self.int_unit.per_second();
                let step = 1_000_000_000 / per;
                (v.div_euclid(per), (v.rem_euclid(per) * step) as u32)
            }
            TimeText::Zoned(t) => (t.timestamp(), t.timestamp_subsec_nanos()),
            TimeText::Naive(t) => {
                let t = match &self.tz {
                    // In a repeated hour (clocks moved back) the earlier moment is taken.
                    Some(tz) => tz
                        .from_local_datetime(&t)
                        .earliest()
                        .map(|t| t.to_utc())
                        .ok_or_else(|| {
                            format!("time '{s}' does not exist in time zone {tz} (the clocks were moved forward)")
                        })?,
                    None => t.and_utc(),
                };
                (t.timestamp(), t.timestamp_subsec_nanos())
            }
        };
        to_unit(secs, nanos, self.unit, rounding).map_err(|why| format!("time '{s}' {why}").into())
    }
}

/// How a time finer than the device's unit is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rounding {
    /// It is an error (points must not merge silently).
    Exact,
    /// Round towards the past (upper bounds of ranges).
    Down,
    /// Round towards the future (lower bounds of ranges).
    Up,
}

/// Converts seconds and nanoseconds since the epoch into `unit` without
/// passing through `i64` nanoseconds, whose range ends in 1677 and 2262.
fn to_unit(secs: i64, nanos: u32, unit: TimeUnit, rounding: Rounding) -> Result<i64, String> {
    let per = unit.per_second();
    let step = 1_000_000_000 / per;
    let (q, r) = (i64::from(nanos) / step, i64::from(nanos) % step);
    let q = match rounding {
        _ if r == 0 => q,
        Rounding::Exact => {
            return Err(format!(
                "is more precise than the device's time unit ({unit})"
            ));
        }
        Rounding::Down => q,
        Rounding::Up => q + 1,
    };
    secs.checked_mul(per)
        .and_then(|v| v.checked_add(q))
        .ok_or_else(|| "is out of the supported range".to_owned())
}

fn parse_tz(tz: Option<&str>) -> Result<Option<Tz>> {
    tz.map(|tz| {
        tz.parse::<Tz>()
            .map_err(|e| format!("unsupported time zone '{tz}': {e}").into())
    })
    .transpose()
}

/// Formats a time stamp the way the CSV export does.
fn format_time(value: i64, unit: TimeUnit, tz: Option<&Tz>) -> String {
    let per = unit.per_second();
    let nanos = (value.rem_euclid(per) * (1_000_000_000 / per)) as u32;
    let Some(t) = DateTime::from_timestamp(value.div_euclid(per), nanos) else {
        return value.to_string();
    };
    match tz {
        Some(tz) => t
            .with_timezone(tz)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true),
        None => format!("{:?}", t.naive_utc()),
    }
}

struct TimeFormatter {
    unit: TimeUnit,
    tz: Option<Tz>,
}

impl TimeFormatter {
    fn new(device: &DeviceSchema) -> Result<Self> {
        Ok(TimeFormatter {
            unit: device.time_unit,
            tz: parse_tz(device.timezone.as_deref())?,
        })
    }

    fn format(&self, value: i64) -> String {
        format_time(value, self.unit, self.tz.as_ref())
    }
}

/// An inclusive range bound; a time between two representable time stamps
/// keeps only the points inside the range.
fn parse_bound(parser: &TimeParser, s: Option<&String>, lower: bool) -> Result<Bound<i64>> {
    let rounding = if lower { Rounding::Up } else { Rounding::Down };
    Ok(match s {
        Some(s) => Bound::Included(parser.parse(s, rounding)?),
        None => Bound::Unbounded,
    })
}

// ---------------------------------------------------------------- values

fn parse_value(column: &str, data_type: DataType, s: &str) -> Result<Value> {
    if s.is_empty() {
        return Ok(Value::Null);
    }
    let bad = || format!("column '{column}': cannot read '{s}' as {data_type}");
    Ok(match data_type {
        DataType::Bool => Value::Bool(parse_bool(s).ok_or_else(bad)?),
        DataType::Int32 => Value::Int32(s.trim().parse().map_err(|_| bad())?),
        DataType::Int64 => Value::Int64(s.trim().parse().map_err(|_| bad())?),
        DataType::Float32 => Value::Float32(s.trim().parse().map_err(|_| bad())?),
        DataType::Float64 => Value::Float64(s.trim().parse().map_err(|_| bad())?),
        DataType::Utf8 => Value::Utf8(s.to_owned()),
    })
}

fn parse_bool(s: &str) -> Option<bool> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("true") || s == "1" {
        Some(true)
    } else if s.eq_ignore_ascii_case("false") || s == "0" {
        Some(false)
    } else {
        None
    }
}

/// Parses `column=value` arguments against a device schema.
fn parse_assignments<'a>(
    device: &DeviceSchema,
    items: &'a [String],
) -> Result<Vec<(&'a str, Value)>> {
    items
        .iter()
        .map(|item| {
            let (name, text) = item
                .split_once('=')
                .ok_or_else(|| format!("expected column=value, got '{item}'"))?;
            let column = device
                .columns
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| trosna::Error::UnknownColumn {
                    device: device.name.clone(),
                    column: name.to_owned(),
                })?;
            Ok((name, parse_value(name, column.data_type, text)?))
        })
        .collect()
}

// ---------------------------------------------------------------- convert

fn convert(args: &ConvertArgs) -> Result<()> {
    if args.output.exists() && !args.append && !args.force {
        return Err(format!(
            "'{}' already exists; use --append to add to it or --force to replace it",
            args.output.display()
        )
        .into());
    }
    if args.output.exists()
        && std::fs::canonicalize(&args.input)? == std::fs::canonicalize(&args.output)?
    {
        return Err("the input and the output are the same file".into());
    }
    if is_trosna_path(&args.input) {
        if args.append {
            return Err("--append only applies when converting CSV to Trosna".into());
        }
        export_csv(args)
    } else {
        import_csv(args)
    }
}

fn delimiter(args: &ConvertArgs) -> Result<u8> {
    u8::try_from(args.delimiter)
        .ok()
        .filter(u8::is_ascii)
        .ok_or_else(|| "the delimiter must be an ASCII character".into())
}

fn export_csv(args: &ConvertArgs) -> Result<()> {
    let reader = Reader::open(&args.input)?;
    let device = pick_device(reader.devices(), args.device.as_deref())?
        .name
        .clone();
    let mut query = reader.query(&device)?;
    if let Some(v) = args.as_of {
        query = query.as_of(v);
    }
    let delimiter = delimiter(args)?;
    // The query is checked (columns, --as-of) before any file is touched, and
    // the CSV replaces the target only when it is complete.
    let rows = trosna_arrow::ArrowReader::new(&query)?;
    let temp = temp_path(&args.output);
    let result = File::create(&temp)
        .map_err(Into::into)
        .and_then(|f| write_csv(rows, BufWriter::new(f), delimiter, args.raw_time, None))
        .and_then(|()| std::fs::rename(&temp, &args.output).map_err(Into::into));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// A hidden file next to `target` that replaces it once complete.
fn temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map_or_else(|| "output".into(), |n| n.to_string_lossy().into_owned());
    target.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Replaces the time column of a record batch by its raw integer values.
fn raw_time(batch: RecordBatch) -> Result<RecordBatch> {
    let schema = batch.schema();
    let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
    let mut columns = batch.columns().to_vec();
    columns[0] = arrow_cast::cast(&columns[0], &ArrowType::Int64)?;
    fields[0] = Field::new(fields[0].name(), ArrowType::Int64, false);
    let schema = Schema::new(fields).with_metadata(schema.metadata().clone());
    Ok(RecordBatch::try_new(Arc::new(schema), columns)?)
}

/// Streams the result of a query as CSV, batch by batch.
fn write_csv(
    reader: trosna_arrow::ArrowReader<'_>,
    mut out: impl Write,
    delimiter: u8,
    raw: bool,
    limit: Option<usize>,
) -> Result<()> {
    let schema = reader.schema();
    let mut writer = arrow_csv::WriterBuilder::new()
        .with_header(true)
        .with_delimiter(delimiter)
        .build(&mut out);
    let mut left = limit.unwrap_or(usize::MAX);
    let mut wrote = false;
    for batch in reader {
        if left == 0 {
            break;
        }
        let mut batch = batch?;
        if batch.num_rows() > left {
            batch = batch.slice(0, left);
        }
        left -= batch.num_rows();
        writer.write(&if raw { raw_time(batch)? } else { batch })?;
        wrote = true;
    }
    if !wrote {
        let empty = RecordBatch::new_empty(schema);
        writer.write(&if raw { raw_time(empty)? } else { empty })?;
    }
    drop(writer);
    out.flush()?;
    Ok(())
}

/// Opens a CSV file as batches of string columns.
fn csv_batches(
    path: &Path,
    delimiter: u8,
) -> Result<(
    Vec<String>,
    impl Iterator<Item = Result<RecordBatch, arrow_schema::ArrowError>>,
)> {
    let format = arrow_csv::reader::Format::default()
        .with_header(true)
        .with_delimiter(delimiter);
    let (schema, _) = format.clone().infer_schema(File::open(path)?, Some(1))?;
    let names: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
    if names.is_empty() {
        return Err(format!("'{}' has no columns", path.display()).into());
    }
    let strings = Schema::new(
        names
            .iter()
            .map(|n| Field::new(n, ArrowType::Utf8, true))
            .collect::<Vec<_>>(),
    );
    let reader = arrow_csv::ReaderBuilder::new(Arc::new(strings))
        .with_format(format)
        .with_batch_size(CSV_BATCH_ROWS)
        .build(File::open(path)?)?;
    Ok((names, reader))
}

fn strings(array: &ArrayRef) -> &StringArray {
    array
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("CSV columns are read as strings")
}

/// What the values of a CSV column can be read as.
#[derive(Debug, Clone, Copy)]
struct ColumnGuess {
    int: bool,
    float: bool,
    bool: bool,
    seen: bool,
}

impl ColumnGuess {
    fn new() -> Self {
        ColumnGuess {
            int: true,
            float: true,
            bool: true,
            seen: false,
        }
    }

    fn observe(&mut self, s: &str) {
        self.seen = true;
        let t = s.trim();
        if self.int && t.parse::<i64>().is_err() {
            self.int = false;
        }
        if self.float && t.parse::<f64>().is_err() {
            self.float = false;
        }
        if self.bool && !(t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false")) {
            self.bool = false;
        }
    }

    fn data_type(self) -> DataType {
        match self {
            ColumnGuess { seen: false, .. } => DataType::Utf8,
            ColumnGuess { int: true, .. } => DataType::Int64,
            ColumnGuess { float: true, .. } => DataType::Float64,
            ColumnGuess { bool: true, .. } => DataType::Bool,
            _ => DataType::Utf8,
        }
    }
}

/// What the time column of a CSV file looks like.
#[derive(Debug, Default)]
struct TimeGuess {
    integers: bool,
    texts: bool,
    max_abs: u64,
    fraction_digits: usize,
    offset: Option<FixedOffset>,
}

impl TimeGuess {
    fn observe(&mut self, s: &str, row: u64) -> Result<()> {
        match parse_time_text(s) {
            Some(TimeText::Integer(v)) => {
                self.integers = true;
                self.max_abs = self.max_abs.max(v.unsigned_abs());
            }
            Some(TimeText::Zoned(t)) => {
                self.texts = true;
                self.offset.get_or_insert(*t.offset());
                self.fraction_digits = self.fraction_digits.max(fraction_digits(s));
            }
            Some(TimeText::Naive(_)) => {
                self.texts = true;
                self.fraction_digits = self.fraction_digits.max(fraction_digits(s));
            }
            None => return Err(format!("row {row}: cannot read '{s}' as a time").into()),
        }
        if self.integers && self.texts {
            return Err("the time column mixes integers and date-time text".into());
        }
        Ok(())
    }

    /// Unit of integer time stamps, guessed from their size.
    fn integer_unit(&self) -> TimeUnit {
        match self.max_abs {
            m if m >= 100_000_000_000_000_000 => TimeUnit::Nanosecond,
            m if m >= 100_000_000_000_000 => TimeUnit::Microsecond,
            m if m >= 100_000_000_000 => TimeUnit::Millisecond,
            _ => TimeUnit::Second,
        }
    }

    /// Unit able to hold date-time text exactly (at least milliseconds).
    fn text_unit(&self) -> TimeUnit {
        match self.fraction_digits {
            0..=3 => TimeUnit::Millisecond,
            4..=6 => TimeUnit::Microsecond,
            _ => TimeUnit::Nanosecond,
        }
    }
}

fn import_csv(args: &ConvertArgs) -> Result<()> {
    if !std::fs::metadata(&args.input)?.is_file() {
        return Err(format!(
            "'{}' must be a regular file: it is read twice (to infer the types, then to convert)",
            args.input.display()
        )
        .into());
    }
    let delim = delimiter(args)?;
    let explicit_unit = args
        .unit
        .as_deref()
        .map(str::parse::<TimeUnit>)
        .transpose()?;
    let (names, _) = csv_batches(&args.input, delim)?;
    let time_index = match &args.time {
        Some(t) => names
            .iter()
            .position(|n| n == t)
            .ok_or_else(|| format!("the CSV has no column '{t}'"))?,
        None => 0,
    };
    let device_name = match &args.device {
        Some(d) => d.clone(),
        None => args
            .input
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("cannot derive a device name from the file name; use --device")?
            .to_owned(),
    };

    // A new file is written next to the target and replaces it only when
    // complete, so a failed import (even with --force) never loses data.
    let temp = (!args.append).then(|| temp_path(&args.output));
    let mut writer = match &temp {
        None => open_writer(&args.output)?,
        Some(temp) => {
            let options = WriteOptions {
                codec: match args.codec {
                    CodecArg::None => Codec::None,
                    CodecArg::Lz4 => Codec::Lz4,
                    CodecArg::Zstd => Codec::Zstd,
                },
                ..WriteOptions::default()
            };
            Writer::create_with(temp, options)?
        }
    };
    let result = import_into(
        &mut writer,
        args,
        &names,
        time_index,
        &device_name,
        explicit_unit,
    );
    match result {
        Ok(()) => {
            let mut options = args.commit.options();
            if options.message.is_none() {
                options.message = Some(format!("imported {}", file_name(&args.input)));
            }
            let done = writer
                .commit_with(options)
                .and_then(|_| writer.close())
                .map_err(Into::into)
                .and_then(|()| match &temp {
                    Some(temp) => std::fs::rename(temp, &args.output).map_err(Into::into),
                    None => Ok(()),
                });
            if done.is_err()
                && let Some(temp) = &temp
            {
                let _ = std::fs::remove_file(temp);
            }
            done
        }
        Err(e) => {
            // With --append the uncommitted rows are removed; the file keeps
            // its previous commits.
            let _ = writer.rollback();
            drop(writer);
            if let Some(temp) = &temp {
                let _ = std::fs::remove_file(temp);
            }
            Err(e)
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn import_into(
    writer: &mut Writer,
    args: &ConvertArgs,
    names: &[String],
    time_index: usize,
    device_name: &str,
    explicit_unit: Option<TimeUnit>,
) -> Result<()> {
    let delim = delimiter(args)?;
    let existing = writer.device(device_name).ok().cloned();
    let (device, parser) = match existing {
        Some(device) => {
            let mut parser = TimeParser::for_device(&device)?;
            if let Some(unit) = explicit_unit {
                parser.int_unit = unit;
            }
            (device, parser)
        }
        None => {
            // First pass: infer the types without keeping the data.
            let mut time = TimeGuess::default();
            let mut guesses = vec![ColumnGuess::new(); names.len()];
            let (_, batches) = csv_batches(&args.input, delim)?;
            let mut row = 0u64;
            for batch in batches {
                let batch = batch?;
                for (i, guess) in guesses.iter_mut().enumerate() {
                    let values = strings(batch.column(i));
                    for (r, v) in values.iter().enumerate() {
                        match v {
                            Some(v) if i == time_index => time.observe(v, row + r as u64 + 2)?,
                            Some(v) => guess.observe(v),
                            None if i == time_index => {
                                return Err(format!(
                                    "row {}: the time column is empty",
                                    row + r as u64 + 2
                                )
                                .into());
                            }
                            None => {}
                        }
                    }
                }
                row += batch.num_rows() as u64;
            }
            let int_unit = explicit_unit.unwrap_or_else(|| time.integer_unit());
            let unit = if time.integers {
                int_unit
            } else {
                explicit_unit.unwrap_or_else(|| time.text_unit())
            };
            let mut device =
                DeviceSchema::new(device_name, unit).with_time_name(names[time_index].clone());
            if let Some(offset) = time.offset {
                device = device.with_timezone(offset.to_string());
            }
            for (i, name) in names.iter().enumerate() {
                if i != time_index {
                    device = device.with_column(name.clone(), guesses[i].data_type());
                }
            }
            writer.create_device(device.clone())?;
            let parser = TimeParser {
                unit,
                int_unit,
                tz: parse_tz(device.timezone.as_deref())?,
            };
            (device, parser)
        }
    };

    // Second pass: convert batch by batch.
    let columns: Vec<(usize, &str, DataType)> = names
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != time_index)
        .map(|(i, name)| {
            let column = device
                .columns
                .iter()
                .find(|c| &c.name == name)
                .ok_or_else(|| trosna::Error::UnknownColumn {
                    device: device.name.clone(),
                    column: name.clone(),
                })?;
            Ok((i, name.as_str(), column.data_type))
        })
        .collect::<Result<_>>()?;
    let (_, batches) = csv_batches(&args.input, delim)?;
    let mut first_row = 2u64;
    for batch in batches {
        let batch = batch?;
        let times = strings(batch.column(time_index));
        let time = times
            .iter()
            .enumerate()
            .map(|(r, v)| {
                let row = first_row + r as u64;
                let v = v.ok_or_else(|| format!("row {row}: the time column is empty"))?;
                parser
                    .parse(v, Rounding::Exact)
                    .map_err(|e| format!("row {row}: {e}").into())
            })
            .collect::<Result<Vec<i64>>>()?;
        let mut out = Vec::with_capacity(columns.len());
        for &(i, name, data_type) in &columns {
            let column = csv_column(strings(batch.column(i)), name, data_type, first_row)?;
            out.push((name, column));
        }
        writer.write(&device.name, &Batch::new(time, out)?)?;
        first_row += batch.num_rows() as u64;
    }
    Ok(())
}

fn csv_column(
    values: &StringArray,
    name: &str,
    data_type: DataType,
    first_row: u64,
) -> Result<Column> {
    fn collect<T>(
        values: &StringArray,
        name: &str,
        data_type: DataType,
        first_row: u64,
        pick: impl Fn(Value) -> Option<T>,
    ) -> Result<Vec<Option<T>>> {
        values
            .iter()
            .enumerate()
            .map(|(r, v)| {
                let v = v.unwrap_or("");
                let value = parse_value(name, data_type, v)
                    .map_err(|e| format!("row {}: {e}", first_row + r as u64))?;
                Ok(pick(value))
            })
            .collect()
    }
    Ok(match data_type {
        DataType::Bool => Column::from(collect(values, name, data_type, first_row, |v| match v {
            Value::Bool(b) => Some(b),
            _ => None,
        })?),
        DataType::Int32 => {
            Column::from(collect(values, name, data_type, first_row, |v| match v {
                Value::Int32(x) => Some(x),
                _ => None,
            })?)
        }
        DataType::Int64 => {
            Column::from(collect(values, name, data_type, first_row, |v| match v {
                Value::Int64(x) => Some(x),
                _ => None,
            })?)
        }
        DataType::Float32 => {
            Column::from(collect(values, name, data_type, first_row, |v| match v {
                Value::Float32(x) => Some(x),
                _ => None,
            })?)
        }
        DataType::Float64 => {
            Column::from(collect(values, name, data_type, first_row, |v| match v {
                Value::Float64(x) => Some(x),
                _ => None,
            })?)
        }
        DataType::Utf8 => Column::from(values.iter().collect::<Vec<Option<&str>>>()),
    })
}

// ---------------------------------------------------------------- reading

fn info(path: &Path) -> Result<()> {
    let reader = Reader::open(path)?;
    let (major, minor) = reader.format_version();
    println!(
        "file:     {} ({}, format {major}.{minor}, {})",
        path.display(),
        human_bytes(reader.file_size()),
        if reader.is_finalized() {
            "finalized"
        } else {
            "not finalized"
        }
    );
    match reader.head() {
        Some(head) => println!(
            "history:  {}, head {}",
            count(head.number, "commit"),
            head.hash_hex()
        ),
        None => println!("history:  no commits"),
    }
    for (key, value) in reader.metadata() {
        println!("metadata: {key} = {value}");
    }
    for device in reader.devices() {
        let fmt = TimeFormatter::new(device)?;
        let mut points = 0u64;
        let mut range: Option<(i64, i64)> = None;
        for batch in reader
            .query(&device.name)?
            .columns(Vec::<String>::new())
            .batches()?
        {
            let batch = batch?;
            let time = batch.time();
            if let (Some(&first), Some(&last)) = (time.first(), time.last()) {
                range = Some(range.map_or((first, last), |(a, _)| (a, last)));
            }
            points += time.len() as u64;
        }
        let blocks = reader.blocks(&device.name)?;
        let stored: u64 = blocks
            .iter()
            .flat_map(|b| &b.segments)
            .map(|s| u64::from(s.stored_bytes))
            .sum();
        println!();
        println!("device '{}': {}", device.name, count(points, "point"));
        if let Some((first, last)) = range {
            println!(
                "  time range:  {} … {}",
                fmt.format(first),
                fmt.format(last)
            );
        }
        println!(
            "  storage:     {}, {} of segments",
            count(blocks.len() as u64, "block"),
            human_bytes(stored)
        );
        println!(
            "  {:<20} time, unit {}, time zone {}",
            device.time_name,
            device.time_unit,
            device.timezone.as_deref().unwrap_or("none")
        );
        for column in &device.columns {
            println!("  {:<20} {}", column.name, column.data_type);
        }
        // Per-column storage: B = mean bits per point, K = plain size / stored size.
        if points > 0 {
            println!("  storage by column (all blocks, including superseded ones):");
            let names = std::iter::once((&device.time_name, None))
                .chain(device.columns.iter().map(|c| (&c.name, Some(c.data_type))));
            for (name, data_type) in names {
                let mut stored = 0u64;
                let mut plain = 0u64;
                let mut encodings: BTreeMap<String, usize> = BTreeMap::new();
                for block in &blocks {
                    for s in block.segments.iter().filter(|s| &s.column == name) {
                        stored += u64::from(s.stored_bytes);
                        let width = match data_type {
                            None | Some(DataType::Int64 | DataType::Float64) => 8,
                            Some(DataType::Int32 | DataType::Float32) => 4,
                            Some(DataType::Bool) => 1,
                            Some(DataType::Utf8) => 0,
                        };
                        plain += u64::from(block.rows) * width;
                        *encodings
                            .entry(format!("{}+{}", s.encoding, s.codec))
                            .or_default() += 1;
                    }
                }
                let bits = 8.0 * stored as f64 / points as f64;
                let ratio = if plain > 0 && stored > 0 {
                    format!(", K = {:.2}", plain as f64 / stored as f64)
                } else {
                    String::new()
                };
                let used: Vec<String> =
                    encodings.iter().map(|(e, n)| format!("{e} ×{n}")).collect();
                println!(
                    "    {:<18} {}, B = {bits:.2} bit/point{ratio}; {}",
                    name,
                    human_bytes(stored),
                    used.join(", ")
                );
            }
        }
        for (key, value) in &device.metadata {
            println!("  metadata: {key} = {value}");
        }
    }
    Ok(())
}

fn cat(args: &CatArgs) -> Result<()> {
    let reader = Reader::open(&args.file)?;
    let device = pick_device(reader.devices(), args.device.as_deref())?;
    let parser = TimeParser::for_device(device)?;
    let mut query = reader.query(&device.name)?;
    if let Some(columns) = &args.columns {
        query = query.columns(columns.iter().map(|c| c.trim().to_owned()));
    }
    query = query.time_range((
        parse_bound(&parser, args.from.as_ref(), true)?,
        parse_bound(&parser, args.to.as_ref(), false)?,
    ));
    if let Some(v) = args.as_of {
        query = query.as_of(v);
    }
    match args.format {
        Format::Csv => write_csv(
            trosna_arrow::ArrowReader::new(&query)?,
            BufWriter::new(Stdout::new()),
            b',',
            args.raw_time,
            args.limit,
        ),
        Format::Table => {
            // A table needs all its rows at once, so their number is bounded.
            let limit = args.limit.unwrap_or(100);
            let mut batches = Vec::new();
            let mut left = limit;
            let mut more = false;
            for batch in trosna_arrow::ArrowReader::new(&query)? {
                let batch = batch?;
                if left == 0 {
                    more |= batch.num_rows() > 0;
                    break;
                }
                let batch = if batch.num_rows() > left {
                    more = true;
                    batch.slice(0, left)
                } else {
                    batch
                };
                left -= batch.num_rows();
                batches.push(if args.raw_time {
                    raw_time(batch)?
                } else {
                    batch
                });
            }
            println!("{}", arrow_cast::pretty::pretty_format_batches(&batches)?);
            if more {
                println!("(only the first {limit} rows are shown; see --limit)");
            }
            Ok(())
        }
    }
}

fn annotations(path: &Path, device: Option<&str>, as_of: Option<u64>) -> Result<()> {
    let reader = Reader::open(path)?;
    let mut formatters = BTreeMap::new();
    for d in reader.devices() {
        formatters.insert(d.name.clone(), TimeFormatter::new(d)?);
    }
    if let Some(name) = device {
        reader.device(name)?;
    }
    for a in reader.annotations(device, as_of)? {
        let fmt = &formatters[&a.device];
        print!(
            "{}\t{}\t{}\t{}\t{}",
            a.id,
            a.device,
            fmt.format(a.start),
            fmt.format(a.end),
            a.label
        );
        match &a.note {
            Some(note) => println!("\t{note}"),
            None => println!(),
        }
    }
    Ok(())
}

fn log(path: &Path) -> Result<()> {
    let reader = Reader::open(path)?;
    let commits = reader.commits();
    for (i, c) in commits.iter().enumerate().rev() {
        println!("commit {} {}", c.number, c.hash_hex());
        let time = DateTime::from_timestamp_nanos(c.time).with_timezone(&chrono::Local);
        println!(
            "Date:    {}",
            time.to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        if let Some(author) = &c.author {
            println!("Author:  {author}");
        }
        if i == 0 && c.prev_hash != [0; 32] {
            println!("Origin:  {}", trosna::hash_hex(&c.prev_hash));
        }
        let ch = &c.changes;
        let mut parts = Vec::new();
        if !ch.devices_created.is_empty() {
            parts.push(format!("new devices: {}", ch.devices_created.join(", ")));
        }
        if ch.rows_written > 0 {
            parts.push(format!(
                "{} in {}",
                count(ch.rows_written, "row"),
                count(u64::from(ch.blocks_written), "block")
            ));
        }
        if ch.ranges_deleted > 0 {
            parts.push(count(u64::from(ch.ranges_deleted), "deleted range"));
        }
        if ch.annotation_ops > 0 {
            parts.push(count(u64::from(ch.annotation_ops), "annotation change"));
        }
        if ch.metadata_changed {
            parts.push("metadata".to_owned());
        }
        if !parts.is_empty() {
            println!("Changes: {}", parts.join("; "));
        }
        if let Some(message) = &c.message {
            println!();
            for line in message.lines() {
                println!("    {line}");
            }
        }
        println!();
    }
    Ok(())
}

fn diff(path: &Path, device: Option<&str>, from: u64, to: Option<u64>) -> Result<()> {
    let reader = Reader::open(path)?;
    let device = pick_device(reader.devices(), device)?;
    let fmt = TimeFormatter::new(device)?;
    let d = reader.diff(&device.name, from, to)?;
    println!("device '{}': commit {} → {}", d.device, d.from, d.to);
    let describe = |values: &[Value]| {
        d.columns
            .iter()
            .zip(values)
            .filter(|(_, v)| !v.is_null())
            .map(|(c, v)| format!("{c}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    for p in &d.points {
        let time = fmt.format(p.time);
        match (p.kind, &p.before, &p.after) {
            (ChangeKind::Added, _, Some(after)) => println!("+ {time}  {}", describe(after)),
            (ChangeKind::Removed, Some(before), _) => println!("- {time}  {}", describe(before)),
            (ChangeKind::Changed, Some(before), Some(after)) => {
                let changes: Vec<String> = d
                    .columns
                    .iter()
                    .zip(before.iter().zip(after))
                    .filter(|(_, (b, a))| !b.same_as(a))
                    .map(|(c, (b, a))| format!("{c}: {b} → {a}"))
                    .collect();
                println!("~ {time}  {}", changes.join(", "));
            }
            (kind, _, _) => println!("{} {time}", kind.name()),
        }
    }
    for a in &d.annotations {
        let shown = a.after.as_ref().or(a.before.as_ref());
        let sign = match a.kind {
            ChangeKind::Added => '+',
            ChangeKind::Removed => '-',
            ChangeKind::Changed => '~',
        };
        match shown {
            Some(s) => println!(
                "{sign} annotation {}: {} … {} {}",
                a.id,
                fmt.format(s.start),
                fmt.format(s.end),
                s.label
            ),
            None => println!("{sign} annotation {}", a.id),
        }
    }
    Ok(())
}

fn verify(path: &Path) -> Result<ExitCode> {
    let report = match trosna::verify(path) {
        Ok(report) => report,
        Err(e @ trosna::Error::Corrupted { .. }) => {
            println!("FAILED: {e}");
            return Ok(ExitCode::FAILURE);
        }
        Err(e) => return Err(e.into()),
    };
    println!("{}", if report.is_ok() { "OK" } else { "FAILED" });
    println!(
        "frames: {}, commits: {}, blocks: {}",
        report.frames, report.commits, report.blocks
    );
    println!("finalized: {}", if report.finalized { "yes" } else { "no" });
    if let Some(head) = &report.head {
        println!("head: {}", trosna::hash_hex(head));
    }
    if let Some(origin) = &report.origin {
        println!("compacted from: {}", trosna::hash_hex(origin));
    }
    for p in &report.problems {
        println!("problem: {p}");
    }
    for w in &report.warnings {
        println!("warning: {w}");
    }
    Ok(if report.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn recover(path: &Path, force: bool) -> Result<()> {
    let report = trosna::recover(path, force)?;
    match report.recovery {
        Some(r) => println!(
            "removed {} bytes ({} frames): {}; the file is finalized",
            r.truncated_bytes, r.discarded_frames, r.reason
        ),
        None => println!("the file was already finalized; nothing removed"),
    }
    Ok(())
}

// ---------------------------------------------------------------- editing

fn edit_point(args: &EditPointArgs, update: bool) -> Result<()> {
    let mut writer = open_writer(&args.file)?;
    let device = pick_device_writer(&writer, args.device.as_deref())?;
    let time = TimeParser::for_device(&device)?.parse(&args.time, Rounding::Exact)?;
    let values = parse_assignments(&device, &args.values)?;
    if update {
        writer.update(&device.name, time, &values)?;
    } else {
        writer.write_row(&device.name, time, &values)?;
    }
    finish(writer, &args.commit)
}

fn pick_device_writer(writer: &Writer, name: Option<&str>) -> Result<DeviceSchema> {
    let devices: Vec<&DeviceSchema> = writer.devices().collect();
    Ok(pick_device(devices.into_iter(), name)?.clone())
}

fn delete(args: &DeleteArgs) -> Result<()> {
    let mut writer = open_writer(&args.file)?;
    let device = pick_device_writer(&writer, args.device.as_deref())?;
    let parser = TimeParser::for_device(&device)?;
    match &args.time {
        Some(t) => writer.delete(&device.name, parser.parse(t, Rounding::Exact)?)?,
        None => {
            if args.from.is_none() && args.to.is_none() {
                return Err("give --time, or --from and/or --to".into());
            }
            writer.delete_range(
                &device.name,
                (
                    parse_bound(&parser, args.from.as_ref(), true)?,
                    parse_bound(&parser, args.to.as_ref(), false)?,
                ),
            )?;
        }
    }
    finish(writer, &args.commit)
}

fn annotate(args: &AnnotateArgs) -> Result<()> {
    match args.id {
        None => {
            let start = args
                .start
                .as_ref()
                .ok_or("a new annotation needs --start")?;
            let label = args
                .label
                .as_ref()
                .ok_or("a new annotation needs --label")?;
            let mut writer = open_writer(&args.file)?;
            let device = pick_device_writer(&writer, args.device.as_deref())?;
            let parser = TimeParser::for_device(&device)?;
            let start = parser.parse(start, Rounding::Down)?;
            let end = args
                .end
                .as_ref()
                .map_or(Ok(start), |e| parser.parse(e, Rounding::Down))?;
            let id = writer.annotate(&device.name, start, end, label, args.note.as_deref())?;
            finish(writer, &args.commit)?;
            println!("{id}");
        }
        Some(id) => {
            let current = Reader::open(&args.file)?
                .annotations(None, None)?
                .into_iter()
                .find(|a| a.id == id)
                .ok_or(trosna::Error::UnknownAnnotation(id))?;
            let mut writer = open_writer(&args.file)?;
            let device = writer.device(&current.device)?.clone();
            let parser = TimeParser::for_device(&device)?;
            let start = args
                .start
                .as_ref()
                .map_or(Ok(current.start), |s| parser.parse(s, Rounding::Down))?;
            let end = args
                .end
                .as_ref()
                .map_or(Ok(current.end), |e| parser.parse(e, Rounding::Down))?;
            let label = args.label.as_deref().unwrap_or(&current.label);
            let note = args.note.as_deref().or(current.note.as_deref());
            writer.update_annotation(id, start, end, label, note)?;
            finish(writer, &args.commit)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin() -> TimeParser {
        TimeParser {
            unit: TimeUnit::Second,
            int_unit: TimeUnit::Second,
            tz: parse_tz(Some("Europe/Berlin")).unwrap(),
        }
    }

    #[test]
    fn local_times_in_daylight_saving_transitions() {
        let p = berlin();
        let gap = p.parse("2026-03-29 02:30", Rounding::Exact).unwrap_err();
        assert!(gap.to_string().contains("does not exist"), "{gap}");
        // 02:30 happens twice on 2026-10-25; the earlier one is 00:30 UTC.
        let fold = p.parse("2026-10-25 02:30", Rounding::Exact).unwrap();
        let utc = p.parse("2026-10-25T00:30:00Z", Rounding::Exact).unwrap();
        assert_eq!(fold, utc);
    }

    #[test]
    fn unit_conversion_rounds_and_checks_ranges() {
        assert_eq!(
            to_unit(-1, 500_000_000, TimeUnit::Second, Rounding::Down),
            Ok(-1)
        );
        assert_eq!(
            to_unit(-1, 500_000_000, TimeUnit::Second, Rounding::Up),
            Ok(0)
        );
        assert!(to_unit(0, 1, TimeUnit::Millisecond, Rounding::Exact).is_err());
        assert!(to_unit(i64::MAX / 1000, 0, TimeUnit::Nanosecond, Rounding::Exact).is_err());
        assert_eq!(
            format_time(-1, TimeUnit::Millisecond, None),
            "1969-12-31T23:59:59.999"
        );
    }
}
