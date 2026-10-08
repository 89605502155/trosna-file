//! # trosna-arrow
//!
//! Conversion between [Trosna](https://docs.rs/trosna) files and
//! [Apache Arrow](https://arrow.apache.org) record batches.
//!
//! Reading moves Trosna's buffers into Arrow arrays without copying the
//! values; the result can be used with DataFusion, Polars (through the Arrow
//! C data interface) and any other Arrow-based tool.
//!
//! | Trosna     | Arrow                      |
//! |------------|----------------------------|
//! | time       | `Timestamp(unit, timezone)`|
//! | `bool`     | `Boolean`                  |
//! | `int32`    | `Int32`                    |
//! | `int64`    | `Int64`                    |
//! | `float32`  | `Float32`                  |
//! | `float64`  | `Float64`                  |
//! | `string`   | `Utf8`                     |
//!
//! When writing, narrower integer types (`Int8`, `UInt16`, …), `Float16`,
//! `LargeUtf8`, `Utf8View` and dictionary-encoded strings are converted
//! losslessly; an integer column that does not fit causes an error rather
//! than silent truncation.
//!
//! ```no_run
//! use trosna::Reader;
//! use trosna_arrow::ArrowReader;
//!
//! let reader = Reader::open("metrics.trosna")?;
//! let query = reader.query("vm01")?.columns(["cpu"]);
//! for batch in ArrowReader::new(&query)? {
//!     let batch = batch?;
//!     println!("{} rows", batch.num_rows());
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float32Type, Float64Type, Int32Type, Int64Type, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType,
};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array,
    PrimitiveArray, RecordBatch, RecordBatchOptions, RecordBatchReader, StringArray, UInt64Array,
};
use arrow_buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow_cast::cast::{CastOptions, cast_with_options};
use arrow_schema::{
    ArrowError, DataType as ArrowType, Field, Schema, SchemaRef, TimeUnit as ArrowUnit,
};
use trosna::{
    Annotation, Batch, Batches, Bitmap, Column, ColumnSchema, DataType, DeviceSchema, Query,
    TimeUnit, Utf8Values, Values, Writer,
};

/// Errors of the Arrow adapter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An error from the Trosna library.
    #[error(transparent)]
    Trosna(#[from] trosna::Error),
    /// An error from Arrow.
    #[error(transparent)]
    Arrow(#[from] ArrowError),
    /// The Arrow data cannot be represented in a Trosna file.
    #[error("{0}")]
    Unsupported(String),
}

/// Result type of the adapter.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Schema metadata key holding the device name.
pub const DEVICE_KEY: &str = "trosna.device";

fn arrow_unit(unit: TimeUnit) -> ArrowUnit {
    match unit {
        TimeUnit::Second => ArrowUnit::Second,
        TimeUnit::Millisecond => ArrowUnit::Millisecond,
        TimeUnit::Microsecond => ArrowUnit::Microsecond,
        TimeUnit::Nanosecond => ArrowUnit::Nanosecond,
    }
}

fn trosna_unit(unit: ArrowUnit) -> TimeUnit {
    match unit {
        ArrowUnit::Second => TimeUnit::Second,
        ArrowUnit::Millisecond => TimeUnit::Millisecond,
        ArrowUnit::Microsecond => TimeUnit::Microsecond,
        ArrowUnit::Nanosecond => TimeUnit::Nanosecond,
    }
}

/// The Arrow type of a Trosna column type.
pub fn arrow_type(data_type: DataType) -> ArrowType {
    match data_type {
        DataType::Bool => ArrowType::Boolean,
        DataType::Int32 => ArrowType::Int32,
        DataType::Int64 => ArrowType::Int64,
        DataType::Float32 => ArrowType::Float32,
        DataType::Float64 => ArrowType::Float64,
        DataType::Utf8 => ArrowType::Utf8,
    }
}

/// The Trosna type that stores an Arrow type losslessly, if any.
pub fn trosna_type(arrow: &ArrowType) -> Option<DataType> {
    Some(match arrow {
        ArrowType::Boolean => DataType::Bool,
        ArrowType::Int8
        | ArrowType::Int16
        | ArrowType::Int32
        | ArrowType::UInt8
        | ArrowType::UInt16 => DataType::Int32,
        ArrowType::Int64 | ArrowType::UInt32 | ArrowType::UInt64 => DataType::Int64,
        ArrowType::Float16 | ArrowType::Float32 => DataType::Float32,
        ArrowType::Float64 => DataType::Float64,
        ArrowType::Utf8 | ArrowType::LargeUtf8 | ArrowType::Utf8View => DataType::Utf8,
        ArrowType::Dictionary(_, value) if trosna_type(value) == Some(DataType::Utf8) => {
            DataType::Utf8
        }
        _ => return None,
    })
}

fn time_field(device: &DeviceSchema) -> Field {
    Field::new(
        &device.time_name,
        ArrowType::Timestamp(
            arrow_unit(device.time_unit),
            device.timezone.as_deref().map(Arc::from),
        ),
        false,
    )
}

/// The Arrow schema of a device: the time column, then the value columns
/// (all of them, or the named ones in the given order).
pub fn arrow_schema(device: &DeviceSchema, columns: Option<&[String]>) -> Result<SchemaRef> {
    let mut fields = vec![time_field(device)];
    let selected: Vec<&ColumnSchema> = match columns {
        None => device.columns.iter().collect(),
        Some(names) => names
            .iter()
            .map(|n| {
                device.columns.iter().find(|c| &c.name == n).ok_or_else(|| {
                    trosna::Error::UnknownColumn {
                        device: device.name.clone(),
                        column: n.clone(),
                    }
                })
            })
            .collect::<Result<_, _>>()?,
    };
    for column in selected {
        let metadata: HashMap<String, String> = column.metadata.clone().into_iter().collect();
        fields.push(
            Field::new(&column.name, arrow_type(column.data_type), true).with_metadata(metadata),
        );
    }
    let mut metadata: HashMap<String, String> = device.metadata.clone().into_iter().collect();
    metadata.insert(DEVICE_KEY.to_owned(), device.name.clone());
    Ok(Arc::new(Schema::new_with_metadata(fields, metadata)))
}

fn null_buffer(validity: Option<Bitmap>) -> Option<NullBuffer> {
    validity.map(|v| {
        let len = v.len();
        NullBuffer::new(BooleanBuffer::new(Buffer::from_vec(v.into_bytes()), 0, len))
    })
}

fn time_array(unit: TimeUnit, timezone: Option<&str>, values: Vec<i64>) -> ArrayRef {
    let buffer = ScalarBuffer::from(values);
    match unit {
        TimeUnit::Second => Arc::new(
            PrimitiveArray::<TimestampSecondType>::new(buffer, None).with_timezone_opt(timezone),
        ),
        TimeUnit::Millisecond => Arc::new(
            PrimitiveArray::<TimestampMillisecondType>::new(buffer, None)
                .with_timezone_opt(timezone),
        ),
        TimeUnit::Microsecond => Arc::new(
            PrimitiveArray::<TimestampMicrosecondType>::new(buffer, None)
                .with_timezone_opt(timezone),
        ),
        TimeUnit::Nanosecond => Arc::new(
            PrimitiveArray::<TimestampNanosecondType>::new(buffer, None)
                .with_timezone_opt(timezone),
        ),
    }
}

/// Converts a column into an Arrow array, moving its buffers.
pub fn column_to_arrow(column: Column) -> Result<ArrayRef> {
    let (values, validity) = column.into_parts();
    let nulls = null_buffer(validity);
    Ok(match values {
        Values::Bool(bits) => {
            let len = bits.len();
            Arc::new(BooleanArray::new(
                BooleanBuffer::new(Buffer::from_vec(bits.into_bytes()), 0, len),
                nulls,
            ))
        }
        Values::Int32(v) => Arc::new(Int32Array::new(ScalarBuffer::from(v), nulls)),
        Values::Int64(v) => Arc::new(Int64Array::new(ScalarBuffer::from(v), nulls)),
        Values::Float32(v) => Arc::new(Float32Array::new(ScalarBuffer::from(v), nulls)),
        Values::Float64(v) => Arc::new(Float64Array::new(ScalarBuffer::from(v), nulls)),
        Values::Utf8(s) => {
            let (offsets, data) = s.into_parts();
            Arc::new(StringArray::try_new(
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                Buffer::from_vec(data),
                nulls,
            )?)
        }
    })
}

/// Converts a batch of `device` into a record batch with the given schema
/// (from [`arrow_schema`]), moving the buffers.
pub fn batch_to_arrow(
    batch: Batch,
    device: &DeviceSchema,
    schema: SchemaRef,
) -> Result<RecordBatch> {
    let rows = batch.len();
    let (time, _, columns) = batch.into_parts();
    let mut arrays = Vec::with_capacity(columns.len() + 1);
    arrays.push(time_array(
        device.time_unit,
        device.timezone.as_deref(),
        time,
    ));
    for column in columns {
        arrays.push(column_to_arrow(column)?);
    }
    let options = RecordBatchOptions::new().with_row_count(Some(rows));
    Ok(RecordBatch::try_new_with_options(schema, arrays, &options)?)
}

/// Streams the result of a query as Arrow record batches.
///
/// Implements [`RecordBatchReader`], so it can be handed to any Arrow
/// consumer.
#[derive(Debug)]
pub struct ArrowReader<'r> {
    batches: Batches<'r>,
    device: DeviceSchema,
    schema: SchemaRef,
}

impl<'r> ArrowReader<'r> {
    /// Executes a query.
    pub fn new(query: &Query<'r>) -> Result<Self> {
        Self::from_batches(query.batches()?)
    }

    /// Wraps batches of a query, e.g. from [`trosna::Reader::query_shared`],
    /// which gives an `ArrowReader<'static>` that can be sent to other threads.
    pub fn from_batches(batches: Batches<'r>) -> Result<Self> {
        let device = batches.device().clone();
        let schema = arrow_schema(&device, Some(batches.column_names()))?;
        Ok(ArrowReader {
            batches,
            device,
            schema,
        })
    }

    /// Reads everything into memory.
    pub fn read_all(self) -> Result<(SchemaRef, Vec<RecordBatch>)> {
        let schema = self.schema.clone();
        let batches = self.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((schema, batches))
    }
}

impl Iterator for ArrowReader<'_> {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        let batch = self.batches.next()?;
        Some(
            batch
                .map_err(Error::from)
                .and_then(|b| batch_to_arrow(b, &self.device, self.schema.clone()))
                .map_err(|e| match e {
                    Error::Arrow(e) => e,
                    other => ArrowError::ExternalError(Box::new(other)),
                }),
        )
    }
}

impl RecordBatchReader for ArrowReader<'_> {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

fn bitmap_from_buffer(buffer: &BooleanBuffer) -> Result<Bitmap> {
    let (offset, len) = (buffer.offset(), buffer.len());
    if offset % 8 == 0 {
        let start = offset / 8;
        let bytes = buffer.values()[start..start + len.div_ceil(8)].to_vec();
        Ok(Bitmap::from_bytes(bytes, len)?)
    } else {
        Ok(buffer.iter().collect())
    }
}

fn cast(array: &dyn Array, to: &ArrowType) -> Result<ArrayRef> {
    let options = CastOptions {
        safe: false,
        ..CastOptions::default()
    };
    Ok(cast_with_options(array, to, &options)?)
}

/// Converts an Arrow array into a column, widening or casting it losslessly.
pub fn column_from_arrow(name: &str, array: &dyn Array) -> Result<Column> {
    let target = trosna_type(array.data_type()).ok_or_else(|| {
        Error::Unsupported(format!(
            "column '{name}' has Arrow type {}, which Trosna cannot store",
            array.data_type()
        ))
    })?;
    if matches!(array.data_type(), ArrowType::UInt64) {
        let max = array
            .as_primitive::<arrow_array::types::UInt64Type>()
            .iter()
            .flatten()
            .max();
        if max.is_some_and(|m| m > i64::MAX as u64) {
            return Err(Error::Unsupported(format!(
                "column '{name}' has values above i64::MAX"
            )));
        }
    }
    let array = cast(array, &arrow_type(target))?;
    let validity = array
        .nulls()
        .map(|n| bitmap_from_buffer(n.inner()))
        .transpose()?;
    let values = match target {
        DataType::Bool => Values::Bool(bitmap_from_buffer(array.as_boolean().values())?),
        DataType::Int32 => Values::Int32(array.as_primitive::<Int32Type>().values().to_vec()),
        DataType::Int64 => Values::Int64(array.as_primitive::<Int64Type>().values().to_vec()),
        DataType::Float32 => Values::Float32(array.as_primitive::<Float32Type>().values().to_vec()),
        DataType::Float64 => Values::Float64(array.as_primitive::<Float64Type>().values().to_vec()),
        DataType::Utf8 => {
            let strings = array.as_string::<i32>();
            let offsets = strings.value_offsets();
            let (first, last) = (offsets[0], offsets[offsets.len() - 1]);
            let data = strings.value_data()[first as usize..last as usize].to_vec();
            let offsets = offsets.iter().map(|o| o - first).collect();
            Values::Utf8(Utf8Values::from_parts(offsets, data)?)
        }
    };
    Ok(Column::new(values, validity)?)
}

/// Extracts time stamps from an Arrow column. Timestamps keep their unit and
/// time zone; integer columns are interpreted in `int_unit`.
pub fn time_from_arrow(
    array: &dyn Array,
    int_unit: Option<TimeUnit>,
) -> Result<(Vec<i64>, TimeUnit, Option<String>)> {
    if array.null_count() > 0 {
        return Err(Error::Unsupported("the time column contains nulls".into()));
    }
    let (unit, timezone) = match array.data_type() {
        ArrowType::Timestamp(unit, tz) => (trosna_unit(*unit), tz.as_deref().map(str::to_owned)),
        ArrowType::Int8
        | ArrowType::Int16
        | ArrowType::Int32
        | ArrowType::Int64
        | ArrowType::UInt8
        | ArrowType::UInt16
        | ArrowType::UInt32
        | ArrowType::UInt64 => {
            let unit = int_unit.ok_or_else(|| {
                Error::Unsupported("an integer time column needs an explicit time unit".into())
            })?;
            (unit, None)
        }
        ArrowType::Date32 | ArrowType::Date64 => (TimeUnit::Millisecond, None),
        other => {
            return Err(Error::Unsupported(format!(
                "cannot use a column of type {other} as time"
            )));
        }
    };
    let values = match array.data_type() {
        ArrowType::Date32 | ArrowType::Date64 => {
            let ms = cast(array, &ArrowType::Date64)?;
            cast(&ms, &ArrowType::Int64)?
        }
        _ => cast(array, &ArrowType::Int64)?,
    };
    Ok((
        values.as_primitive::<Int64Type>().values().to_vec(),
        unit,
        timezone,
    ))
}

/// Derives a device schema from an Arrow schema whose `time_column` holds the
/// time stamps; integer time columns need `int_unit`.
pub fn device_from_arrow(
    name: &str,
    schema: &Schema,
    time_column: &str,
    int_unit: Option<TimeUnit>,
) -> Result<DeviceSchema> {
    let time = schema.field_with_name(time_column)?;
    let (unit, timezone) = match time.data_type() {
        ArrowType::Timestamp(unit, tz) => (trosna_unit(*unit), tz.as_deref().map(str::to_owned)),
        ArrowType::Date32 | ArrowType::Date64 => (TimeUnit::Millisecond, None),
        t if t.is_integer() => (
            int_unit.ok_or_else(|| {
                Error::Unsupported("an integer time column needs an explicit time unit".into())
            })?,
            None,
        ),
        other => {
            return Err(Error::Unsupported(format!(
                "cannot use a column of type {other} as time"
            )));
        }
    };
    let mut device = DeviceSchema::new(name, unit).with_time_name(time_column);
    device.timezone = timezone;
    for field in schema.fields().iter().filter(|f| f.name() != time_column) {
        let data_type = trosna_type(field.data_type()).ok_or_else(|| {
            Error::Unsupported(if field.data_type() == &ArrowType::Null {
                format!(
                    "column '{}' contains only nulls, so its type is unknown; \
                     create the device with an explicit schema first",
                    field.name()
                )
            } else {
                format!(
                    "column '{}' has Arrow type {}, which Trosna cannot store",
                    field.name(),
                    field.data_type()
                )
            })
        })?;
        let mut column = ColumnSchema::new(field.name(), data_type);
        column.metadata = field.metadata().clone().into_iter().collect();
        device = device.with_column_schema(column);
    }
    device.validate()?;
    Ok(device)
}

/// Converts an Arrow array into a column of type `target`.
///
/// Integers are converted with a range check; floating-point values become
/// integers only if they are whole numbers; any number becomes a float,
/// rounded to the nearest representable value; an all-null (`Null`) array
/// becomes a column of nulls. Other conversions are refused.
pub fn column_from_arrow_as(name: &str, array: &dyn Array, target: DataType) -> Result<Column> {
    let source = array.data_type();
    if source == &ArrowType::Null {
        return Ok(Column::nulls(target, array.len()));
    }
    let numeric = source.is_integer() || source.is_floating();
    let allowed = match target {
        DataType::Bool => source == &ArrowType::Boolean,
        DataType::Int32 | DataType::Int64 | DataType::Float32 | DataType::Float64 => numeric,
        DataType::Utf8 => trosna_type(source) == Some(DataType::Utf8),
    };
    if !allowed {
        return Err(Error::Unsupported(format!(
            "column '{name}' has type {target}, so Arrow values of type {source} cannot be stored in it"
        )));
    }
    if matches!(target, DataType::Int32 | DataType::Int64) && source.is_floating() {
        let floats = cast(array, &ArrowType::Float64)?;
        if let Some(v) = floats
            .as_primitive::<Float64Type>()
            .iter()
            .flatten()
            .find(|v| v.fract() != 0.0 || !v.is_finite())
        {
            return Err(Error::Unsupported(format!(
                "column '{name}' has type {target}, but the value {v} is not a whole number"
            )));
        }
    }
    column_from_arrow(name, cast(array, &arrow_type(target))?.as_ref())
}

/// Converts a record batch into rows of `device`. The time stamps are taken
/// from `time_column` and converted to the device's unit (integer time
/// stamps are read in `int_unit`, by default the device's unit); every other
/// column is converted to the type of the device column of the same name
/// with [`column_from_arrow_as`].
pub fn batch_from_arrow(
    batch: &RecordBatch,
    time_column: &str,
    device: &DeviceSchema,
    int_unit: Option<TimeUnit>,
) -> Result<Batch> {
    let schema = batch.schema();
    let index = schema.index_of(time_column)?;
    let unit = device.time_unit;
    let column = batch.column(index);
    if let (ArrowType::Timestamp(_, None), Some(tz)) = (column.data_type(), &device.timezone) {
        // A naive time stamp is a wall-clock reading; storing it as UTC would
        // shift it by the device's offset, so the caller must localize it.
        return Err(Error::Unsupported(format!(
            "the time column '{time_column}' has no time zone, but device '{}' uses {tz}; \
             attach the time zone to the time stamps first",
            device.name
        )));
    }
    let (mut time, source_unit, _) =
        time_from_arrow(column.as_ref(), Some(int_unit.unwrap_or(unit)))?;
    if source_unit != unit {
        time = convert_unit(&time, source_unit, unit)?;
    }
    let mut columns = Vec::with_capacity(batch.num_columns().saturating_sub(1));
    for (i, field) in schema.fields().iter().enumerate() {
        if i == index {
            continue;
        }
        let target = device
            .columns
            .iter()
            .find(|c| &c.name == field.name())
            .ok_or_else(|| trosna::Error::UnknownColumn {
                device: device.name.clone(),
                column: field.name().clone(),
            })?
            .data_type;
        let column = column_from_arrow_as(field.name(), batch.column(i).as_ref(), target)?;
        columns.push((field.name().clone(), column));
    }
    Ok(Batch::new(time, columns)?)
}

/// Converts time stamps between units; going to a coarser unit must be exact.
fn convert_unit(values: &[i64], from: TimeUnit, to: TimeUnit) -> Result<Vec<i64>> {
    let (f, t) = (from.per_second(), to.per_second());
    values
        .iter()
        .map(|&v| {
            if t >= f {
                v.checked_mul(t / f).ok_or_else(|| {
                    Error::Unsupported(format!("time stamp {v} {from} overflows in {to}"))
                })
            } else if v % (f / t) == 0 {
                Ok(v / (f / t))
            } else {
                Err(Error::Unsupported(format!(
                    "time stamp {v} {from} is not a whole number of {to}"
                )))
            }
        })
        .collect()
}

/// Writes a record batch to a device. If the device does not exist yet, it is
/// created from the batch's schema (integer time columns then need
/// `int_unit`); otherwise the values are converted to the device's types with
/// [`column_from_arrow_as`].
pub fn write_arrow(
    writer: &mut Writer,
    device: &str,
    time_column: &str,
    batch: &RecordBatch,
    int_unit: Option<TimeUnit>,
) -> Result<()> {
    let schema = match writer.device(device) {
        Ok(existing) => existing.clone(),
        Err(_) => {
            let schema = device_from_arrow(device, &batch.schema(), time_column, int_unit)?;
            writer.create_device(schema.clone())?;
            schema
        }
    };
    let converted = batch_from_arrow(batch, time_column, &schema, int_unit)?;
    writer.write(device, &converted)?;
    Ok(())
}

/// Annotations as a record batch with columns `id`, `device`, `start`, `end`,
/// `label` and `note`. Times are raw values in the unit of each device.
pub fn annotations_to_arrow(annotations: &[Annotation]) -> Result<RecordBatch> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", ArrowType::UInt64, false),
        Field::new("device", ArrowType::Utf8, false),
        Field::new("start", ArrowType::Int64, false),
        Field::new("end", ArrowType::Int64, false),
        Field::new("label", ArrowType::Utf8, false),
        Field::new("note", ArrowType::Utf8, true),
    ]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from_iter_values(
            annotations.iter().map(|a| a.id),
        )),
        Arc::new(StringArray::from_iter_values(
            annotations.iter().map(|a| a.device.as_str()),
        )),
        Arc::new(Int64Array::from_iter_values(
            annotations.iter().map(|a| a.start),
        )),
        Arc::new(Int64Array::from_iter_values(
            annotations.iter().map(|a| a.end),
        )),
        Arc::new(StringArray::from_iter_values(
            annotations.iter().map(|a| a.label.as_str()),
        )),
        Arc::new(StringArray::from(
            annotations
                .iter()
                .map(|a| a.note.clone())
                .collect::<Vec<_>>(),
        )),
    ];
    Ok(RecordBatch::try_new(schema, columns)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_conversion() {
        assert_eq!(
            convert_unit(&[1, 2], TimeUnit::Second, TimeUnit::Millisecond).unwrap(),
            vec![1000, 2000]
        );
        assert_eq!(
            convert_unit(&[3000], TimeUnit::Millisecond, TimeUnit::Second).unwrap(),
            vec![3]
        );
        assert!(convert_unit(&[3001], TimeUnit::Millisecond, TimeUnit::Second).is_err());
        assert!(convert_unit(&[i64::MAX], TimeUnit::Second, TimeUnit::Nanosecond).is_err());
    }

    #[test]
    fn type_mapping() {
        assert_eq!(trosna_type(&ArrowType::UInt16), Some(DataType::Int32));
        assert_eq!(trosna_type(&ArrowType::LargeUtf8), Some(DataType::Utf8));
        assert_eq!(
            trosna_type(&ArrowType::Dictionary(
                Box::new(ArrowType::Int8),
                Box::new(ArrowType::Utf8)
            )),
            Some(DataType::Utf8)
        );
        assert_eq!(trosna_type(&ArrowType::Date32), None);
        for t in DataType::ALL {
            assert_eq!(trosna_type(&arrow_type(t)), Some(t));
        }
    }

    #[test]
    fn columns_round_trip_through_arrow() {
        let columns = vec![
            Column::from(vec![Some(true), None, Some(false)]),
            Column::from(vec![Some(1i32), None, Some(-3)]),
            Column::from(vec![1i64, 2, 3]),
            Column::from(vec![Some(1.5f32), Some(f32::NAN), None]),
            Column::from(vec![Some(0.25), None, Some(-0.0)]),
            Column::from(vec![Some("а"), None, Some("bc")]),
        ];
        for column in columns {
            let array = column_to_arrow(column.clone()).unwrap();
            assert_eq!(column_from_arrow("x", array.as_ref()).unwrap(), column);
            // sliced arrays with unaligned offsets
            let sliced = array.slice(1, 2);
            assert_eq!(
                column_from_arrow("x", sliced.as_ref()).unwrap(),
                column.slice(1..3)
            );
        }
    }

    #[test]
    fn lossy_casts_are_rejected() {
        let big = UInt64Array::from(vec![u64::MAX]);
        assert!(column_from_arrow("x", &big).is_err());
        let ok = UInt64Array::from(vec![5u64]);
        assert_eq!(
            column_from_arrow("x", &ok).unwrap(),
            Column::from(vec![5i64])
        );
        let dates = arrow_array::Date32Array::from(vec![1]);
        assert!(column_from_arrow("x", &dates).is_err());
    }
}
