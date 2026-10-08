//! Native part of the `trosna` Python package (`trosna._trosna`).
//!
//! The Python layer (`trosna/__init__.py`) converts times and data frames;
//! this module works with raw time stamps in each device's unit and with
//! Arrow streams. Data is read and written batch by batch, so memory use does
//! not grow with the size of a file.

use std::ops::Bound as Limit;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pyo3::IntoPyObjectExt;
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyFileExistsError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use pyo3_arrow::PyRecordBatchReader;
use pyo3_arrow::input::AnyRecordBatch;
use trosna::{
    Annotation, CommitInfo, CommitOptions, DataType, DeviceSchema, Reader, TimeUnit, Value,
    WriteOptions, Writer,
};
use trosna_arrow::ArrowReader;

create_exception!(
    _trosna,
    TrosnaError,
    PyException,
    "An error reported by the Trosna library."
);
create_exception!(
    _trosna,
    CorruptedError,
    TrosnaError,
    "The file is damaged or is not a valid Trosna file."
);

fn err(e: trosna::Error) -> PyErr {
    match e {
        trosna::Error::Io(e) => e.into(),
        e @ (trosna::Error::InvalidArgument(_) | trosna::Error::TypeMismatch { .. }) => {
            PyValueError::new_err(e.to_string())
        }
        e @ (trosna::Error::Corrupted { .. } | trosna::Error::NotTrosna) => {
            CorruptedError::new_err(e.to_string())
        }
        e => TrosnaError::new_err(e.to_string()),
    }
}

fn arrow_err(e: trosna_arrow::Error) -> PyErr {
    match e {
        trosna_arrow::Error::Trosna(e) => err(e),
        e => PyValueError::new_err(e.to_string()),
    }
}

fn bound(v: Option<i64>) -> Limit<i64> {
    v.map_or(Limit::Unbounded, Limit::Included)
}

fn value_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    match v {
        Value::Null => Ok(py.None().into_bound(py)),
        Value::Bool(b) => b.into_bound_py_any(py),
        Value::Int32(x) => x.into_bound_py_any(py),
        Value::Int64(x) => x.into_bound_py_any(py),
        Value::Float32(x) => x.into_bound_py_any(py),
        Value::Float64(x) => x.into_bound_py_any(py),
        Value::Utf8(s) => s.as_str().into_bound_py_any(py),
    }
}

fn value_from_py(obj: &Bound<'_, PyAny>, data_type: DataType, column: &str) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    let value = match data_type {
        DataType::Bool => obj.extract::<bool>().map(Value::Bool),
        DataType::Int32 => obj.extract::<i32>().map(Value::Int32),
        DataType::Int64 => obj.extract::<i64>().map(Value::Int64),
        DataType::Float32 => obj.extract::<f32>().map(Value::Float32),
        DataType::Float64 => obj.extract::<f64>().map(Value::Float64),
        DataType::Utf8 => obj.extract::<String>().map(Value::Utf8),
    };
    value.map_err(|_| {
        let given = obj
            .get_type()
            .name()
            .map_or_else(|_| "?".to_owned(), |n| n.to_string());
        PyTypeError::new_err(format!(
            "column '{column}' has type {data_type}; a {given} value cannot be stored in it"
        ))
    })
}

fn device_to_py<'py>(py: Python<'py>, d: &DeviceSchema) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("name", &d.name)?;
    dict.set_item("time_unit", d.time_unit.name())?;
    dict.set_item("timezone", d.timezone.as_deref())?;
    dict.set_item("time_name", &d.time_name)?;
    let columns = PyList::empty(py);
    for c in &d.columns {
        columns.append((c.name.as_str(), c.data_type.name()))?;
    }
    dict.set_item("columns", columns)?;
    dict.set_item("metadata", d.metadata.clone())?;
    Ok(dict)
}

fn commit_to_py<'py>(py: Python<'py>, c: &CommitInfo) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("number", c.number)?;
    dict.set_item("time", c.time)?;
    dict.set_item("author", c.author.as_deref())?;
    dict.set_item("message", c.message.as_deref())?;
    dict.set_item("hash", c.hash_hex())?;
    dict.set_item("prev_hash", trosna::hash_hex(&c.prev_hash))?;
    Ok(dict)
}

fn annotation_to_py<'py>(py: Python<'py>, a: &Annotation) -> PyResult<Bound<'py, PyAny>> {
    (
        a.id,
        a.device.as_str(),
        a.start,
        a.end,
        a.label.as_str(),
        a.note.as_deref(),
    )
        .into_bound_py_any(py)
}

/// Devices, metadata and state of a file.
#[pyfunction]
fn info<'py>(py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyDict>> {
    let reader = Reader::open(&path).map_err(err)?;
    let dict = PyDict::new(py);
    dict.set_item("finalized", reader.is_finalized())?;
    dict.set_item("format_version", reader.format_version())?;
    dict.set_item("size", reader.file_size())?;
    dict.set_item("metadata", reader.metadata())?;
    let devices = PyList::empty(py);
    for d in reader.devices() {
        devices.append(device_to_py(py, d)?)?;
    }
    dict.set_item("devices", devices)?;
    Ok(dict)
}

/// Streams the points of a device as Arrow record batches.
#[pyfunction]
#[pyo3(signature = (path, device, columns=None, start=None, end=None, as_of=None))]
fn read(
    path: PathBuf,
    device: &str,
    columns: Option<Vec<String>>,
    start: Option<i64>,
    end: Option<i64>,
    as_of: Option<u64>,
) -> PyResult<PyRecordBatchReader> {
    let reader = Arc::new(Reader::open(&path).map_err(err)?);
    let batches = Reader::query_shared(&reader, device, |mut q| {
        if let Some(columns) = columns {
            q = q.columns(columns);
        }
        q = q.time_range((bound(start), bound(end)));
        if let Some(v) = as_of {
            q = q.as_of(v);
        }
        q
    })
    .map_err(err)?;
    let arrow = ArrowReader::from_batches(batches).map_err(arrow_err)?;
    Ok(PyRecordBatchReader::new(Box::new(arrow)))
}

/// Counts the points of a device.
#[pyfunction]
#[pyo3(signature = (path, device, start=None, end=None, as_of=None))]
fn count(
    path: PathBuf,
    device: &str,
    start: Option<i64>,
    end: Option<i64>,
    as_of: Option<u64>,
) -> PyResult<u64> {
    let reader = Reader::open(&path).map_err(err)?;
    let mut q = reader
        .query(device)
        .map_err(err)?
        .time_range((bound(start), bound(end)));
    if let Some(v) = as_of {
        q = q.as_of(v);
    }
    q.count().map_err(err)
}

fn commit_options(message: Option<String>, author: Option<String>) -> CommitOptions {
    CommitOptions {
        message,
        author,
        ..CommitOptions::default()
    }
}

/// Undoes a failed operation: uncommitted changes are rolled back and a
/// temporary file, if any, is removed. A file that existed before is never
/// deleted.
fn discard(mut writer: Writer, temp: Option<&Path>) {
    let _ = writer.rollback();
    drop(writer);
    if let Some(temp) = temp {
        let _ = std::fs::remove_file(temp);
    }
}

/// A hidden file next to `target` that replaces it once complete.
fn temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map_or_else(|| "output".into(), |n| n.to_string_lossy().into_owned());
    target.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Writes an Arrow stream to a device in one commit; returns the commit number.
///
/// `mode` is "w" (create or replace the file), "x" (create; fail if it
/// exists) or "a" (append; create if missing). New and replacing files are
/// written to a temporary file that takes the place of `path` only when
/// complete, so a failure leaves an existing file untouched.
#[pyfunction]
#[pyo3(signature = (path, data, device, time_column, unit=None, mode="a", message=None, author=None))]
#[allow(clippy::too_many_arguments)]
fn write(
    py: Python<'_>,
    path: PathBuf,
    data: AnyRecordBatch,
    device: String,
    time_column: String,
    unit: Option<String>,
    mode: &str,
    message: Option<String>,
    author: Option<String>,
) -> PyResult<Option<u64>> {
    let int_unit = unit
        .map(|u| u.parse::<TimeUnit>())
        .transpose()
        .map_err(err)?;
    let exists = path.exists();
    let append = match mode {
        "w" => false,
        "x" if exists => {
            return Err(PyFileExistsError::new_err(format!(
                "'{}' already exists",
                path.display()
            )));
        }
        "x" => false,
        "a" => exists,
        other => {
            return Err(PyValueError::new_err(format!(
                "mode must be 'w', 'x' or 'a', not '{other}'"
            )));
        }
    };
    let temp = (!append).then(|| temp_path(&path));
    let mut writer = match &temp {
        None => Writer::open(&path),
        Some(temp) => Writer::create(temp),
    }
    .map_err(err)?;
    let stream = match data.into_reader() {
        Ok(stream) => stream,
        Err(e) => {
            discard(writer, temp.as_deref());
            return Err(e);
        }
    };
    // The batches are converted without the GIL; a Python-backed stream
    // re-acquires it as needed.
    let result = py.detach(|| -> PyResult<Option<u64>> {
        let schema = stream.schema();
        for batch in stream {
            let batch = batch.map_err(|e| PyValueError::new_err(e.to_string()))?;
            trosna_arrow::write_arrow(&mut writer, &device, &time_column, &batch, int_unit)
                .map_err(arrow_err)?;
        }
        if writer.device(&device).is_err() {
            // No rows at all: the device is still created, with the stream's schema.
            let schema = trosna_arrow::device_from_arrow(&device, &schema, &time_column, int_unit)
                .map_err(arrow_err)?;
            writer.create_device(schema).map_err(err)?;
        }
        let commit = writer
            .commit_with(commit_options(message, author))
            .map_err(err)?;
        Ok(commit.map(|c| c.number))
    });
    let number = match result {
        Ok(number) => number,
        Err(e) => {
            discard(writer, temp.as_deref());
            return Err(e);
        }
    };
    let finished = writer.close().map_err(err).and_then(|()| match &temp {
        None => Ok(()),
        Some(_) if mode == "x" && path.exists() => Err(PyFileExistsError::new_err(format!(
            "'{}' was created by someone else meanwhile",
            path.display()
        ))),
        Some(temp) => std::fs::rename(temp, &path).map_err(PyErr::from),
    });
    if finished.is_err()
        && let Some(temp) = &temp
    {
        let _ = std::fs::remove_file(temp);
    }
    finished.map(|()| number)
}

/// Applies a list of editing operations in one commit.
///
/// Operations are tuples whose first item names them:
/// ("insert" | "update", device, time, {column: value}),
/// ("delete", device, time), ("delete_range", device, start | None, end | None),
/// ("annotate", device, start, end, label, note | None),
/// ("update_annotation", id, start, end, label, note | None),
/// ("remove_annotation", id), ("set_metadata", key, value), ("remove_metadata", key).
/// Returns the commit number (None if nothing changed) and the identifiers of
/// the new annotations.
#[pyfunction]
#[pyo3(signature = (path, operations, message=None, author=None))]
fn edit(
    path: PathBuf,
    operations: Vec<Bound<'_, PyTuple>>,
    message: Option<String>,
    author: Option<String>,
) -> PyResult<(Option<u64>, Vec<u64>)> {
    let mut writer = Writer::open(&path).map_err(err)?;
    let mut ids = Vec::new();
    let result = (|| -> PyResult<()> {
        for op in &operations {
            apply(&mut writer, op, &mut ids)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        discard(writer, None);
        return Err(e);
    }
    let commit = writer
        .commit_with(commit_options(message, author))
        .map_err(err)?;
    writer.close().map_err(err)?;
    Ok((commit.map(|c| c.number), ids))
}

fn row_values(
    writer: &Writer,
    device: &str,
    values: &Bound<'_, PyDict>,
) -> PyResult<Vec<(String, Value)>> {
    let schema = writer.device(device).map_err(err)?;
    values
        .iter()
        .map(|(key, value)| {
            let name: String = key.extract()?;
            let column = schema
                .columns
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| {
                    err(trosna::Error::UnknownColumn {
                        device: device.to_owned(),
                        column: name.clone(),
                    })
                })?;
            let value = value_from_py(&value, column.data_type, &name)?;
            Ok((name, value))
        })
        .collect()
}

fn apply(writer: &mut Writer, op: &Bound<'_, PyTuple>, ids: &mut Vec<u64>) -> PyResult<()> {
    let name: String = op.get_item(0)?.extract()?;
    match name.as_str() {
        "insert" | "update" => {
            let (_, device, time, values): (String, String, i64, Bound<'_, PyDict>) =
                op.extract()?;
            let row = row_values(writer, &device, &values)?;
            let row: Vec<(&str, Value)> =
                row.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
            if name == "insert" {
                writer.write_row(&device, time, &row).map_err(err)
            } else {
                writer.update(&device, time, &row).map_err(err)
            }
        }
        "delete" => {
            let (_, device, time): (String, String, i64) = op.extract()?;
            writer.delete(&device, time).map_err(err)
        }
        "delete_range" => {
            let (_, device, start, end): (String, String, Option<i64>, Option<i64>) =
                op.extract()?;
            writer
                .delete_range(&device, (bound(start), bound(end)))
                .map_err(err)
        }
        "annotate" => {
            let (_, device, start, end, label, note): (
                String,
                String,
                i64,
                i64,
                String,
                Option<String>,
            ) = op.extract()?;
            let id = writer
                .annotate(&device, start, end, &label, note.as_deref())
                .map_err(err)?;
            ids.push(id);
            Ok(())
        }
        "update_annotation" => {
            let (_, id, start, end, label, note): (String, u64, i64, i64, String, Option<String>) =
                op.extract()?;
            writer
                .update_annotation(id, start, end, &label, note.as_deref())
                .map_err(err)
        }
        "remove_annotation" => {
            let (_, id): (String, u64) = op.extract()?;
            writer.remove_annotation(id).map_err(err)
        }
        "set_metadata" => {
            let (_, key, value): (String, String, String) = op.extract()?;
            writer.set_metadata(&key, &value).map_err(err)
        }
        "remove_metadata" => {
            let (_, key): (String, String) = op.extract()?;
            writer.remove_metadata(&key).map_err(err)
        }
        other => Err(PyValueError::new_err(format!(
            "unknown operation '{other}'"
        ))),
    }
}

/// The commits of a file, oldest first.
#[pyfunction]
fn commits<'py>(py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyList>> {
    let reader = Reader::open(&path).map_err(err)?;
    let list = PyList::empty(py);
    for c in reader.commits() {
        list.append(commit_to_py(py, &c)?)?;
    }
    Ok(list)
}

/// Annotations as (id, device, start, end, label, note) tuples.
#[pyfunction]
#[pyo3(signature = (path, device=None, as_of=None))]
fn annotations<'py>(
    py: Python<'py>,
    path: PathBuf,
    device: Option<&str>,
    as_of: Option<u64>,
) -> PyResult<Bound<'py, PyList>> {
    let reader = Reader::open(&path).map_err(err)?;
    let list = PyList::empty(py);
    for a in reader.annotations(device, as_of).map_err(err)? {
        list.append(annotation_to_py(py, &a)?)?;
    }
    Ok(list)
}

fn row_to_py<'py>(
    py: Python<'py>,
    columns: &[String],
    values: Option<&Vec<Value>>,
) -> PyResult<Bound<'py, PyAny>> {
    match values {
        None => Ok(py.None().into_bound(py)),
        Some(values) => {
            let dict = PyDict::new(py);
            for (c, v) in columns.iter().zip(values) {
                dict.set_item(c, value_to_py(py, v)?)?;
            }
            Ok(dict.into_any())
        }
    }
}

/// Differences of a device between two commits.
#[pyfunction]
#[pyo3(signature = (path, device, start, end=None))]
fn diff<'py>(
    py: Python<'py>,
    path: PathBuf,
    device: &str,
    start: u64,
    end: Option<u64>,
) -> PyResult<Bound<'py, PyDict>> {
    let reader = Reader::open(&path).map_err(err)?;
    let d = reader.diff(device, start, end).map_err(err)?;
    let dict = PyDict::new(py);
    dict.set_item("device", &d.device)?;
    dict.set_item("columns", &d.columns)?;
    dict.set_item("from", d.from)?;
    dict.set_item("to", d.to)?;
    let points = PyList::empty(py);
    for p in &d.points {
        points.append((
            p.time,
            p.kind.name(),
            row_to_py(py, &d.columns, p.before.as_ref())?,
            row_to_py(py, &d.columns, p.after.as_ref())?,
        ))?;
    }
    dict.set_item("points", points)?;
    let changes = PyList::empty(py);
    for a in &d.annotations {
        let before = a
            .before
            .as_ref()
            .map(|x| annotation_to_py(py, x))
            .transpose()?;
        let after = a
            .after
            .as_ref()
            .map(|x| annotation_to_py(py, x))
            .transpose()?;
        changes.append((a.id, a.kind.name(), before, after))?;
    }
    dict.set_item("annotations", changes)?;
    Ok(dict)
}

/// Full verification of a file.
#[pyfunction]
fn verify<'py>(py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyDict>> {
    let r = py.detach(|| trosna::verify(&path)).map_err(err)?;
    let dict = PyDict::new(py);
    dict.set_item("ok", r.is_ok())?;
    dict.set_item("frames", r.frames)?;
    dict.set_item("commits", r.commits)?;
    dict.set_item("blocks", r.blocks)?;
    dict.set_item("finalized", r.finalized)?;
    dict.set_item("head", r.head.as_ref().map(trosna::hash_hex))?;
    dict.set_item("origin", r.origin.as_ref().map(trosna::hash_hex))?;
    dict.set_item("problems", &r.problems)?;
    dict.set_item("warnings", &r.warnings)?;
    Ok(dict)
}

/// Finalizes an interrupted file; returns (removed bytes, removed frames,
/// reason) or None if the file was already finalized.
#[pyfunction]
#[pyo3(signature = (path, force=false))]
fn recover(path: PathBuf, force: bool) -> PyResult<Option<(u64, u32, String)>> {
    let r = trosna::recover(&path, force).map_err(err)?;
    Ok(r.recovery
        .map(|r| (r.truncated_bytes, r.discarded_frames, r.reason)))
}

/// Copies a version into a new file without history.
#[pyfunction]
#[pyo3(signature = (source, target, as_of=None, overwrite=false))]
fn compact<'py>(
    py: Python<'py>,
    source: PathBuf,
    target: PathBuf,
    as_of: Option<u64>,
    overwrite: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let options = WriteOptions {
        overwrite,
        ..WriteOptions::default()
    };
    let r = py
        .detach(|| trosna::compact(&source, &target, as_of, options))
        .map_err(err)?;
    let dict = PyDict::new(py);
    dict.set_item("version", r.version)?;
    dict.set_item("origin", trosna::hash_hex(&r.origin))?;
    dict.set_item("rows", r.rows)?;
    dict.set_item("annotations", r.annotations)?;
    dict.set_item("bytes_before", r.bytes_before)?;
    dict.set_item("bytes_after", r.bytes_after)?;
    Ok(dict)
}

#[pymodule]
fn _trosna(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("TrosnaError", m.py().get_type::<TrosnaError>())?;
    m.add("CorruptedError", m.py().get_type::<CorruptedError>())?;
    m.add("EXTENSION", trosna::EXTENSION)?;
    m.add_function(wrap_pyfunction!(info, m)?)?;
    m.add_function(wrap_pyfunction!(read, m)?)?;
    m.add_function(wrap_pyfunction!(count, m)?)?;
    m.add_function(wrap_pyfunction!(write, m)?)?;
    m.add_function(wrap_pyfunction!(edit, m)?)?;
    m.add_function(wrap_pyfunction!(commits, m)?)?;
    m.add_function(wrap_pyfunction!(annotations, m)?)?;
    m.add_function(wrap_pyfunction!(diff, m)?)?;
    m.add_function(wrap_pyfunction!(verify, m)?)?;
    m.add_function(wrap_pyfunction!(recover, m)?)?;
    m.add_function(wrap_pyfunction!(compact, m)?)?;
    Ok(())
}
