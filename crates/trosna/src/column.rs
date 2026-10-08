//! In-memory columnar data.
//!
//! The physical layout matches Apache Arrow (value buffers, LSB-first
//! validity bitmaps, `i32` string offsets), so conversion to Arrow moves the
//! buffers instead of copying the values.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use crate::bytes::ceil8;
use crate::error::{Error, Result};
use crate::types::DataType;

/// A packed sequence of bits, least significant bit first (Arrow layout).
/// Padding bits of the last byte are always zero.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Bitmap {
    bytes: Vec<u8>,
    len: usize,
}

impl Bitmap {
    /// Creates an empty bitmap.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty bitmap with room for `bits` bits.
    pub fn with_capacity(bits: usize) -> Self {
        Bitmap {
            bytes: Vec::with_capacity(ceil8(bits)),
            len: 0,
        }
    }

    /// Creates a bitmap of `len` bits, all set to `value`.
    pub fn new_filled(len: usize, value: bool) -> Self {
        let mut bitmap = Bitmap {
            bytes: vec![if value { 0xff } else { 0 }; ceil8(len)],
            len,
        };
        bitmap.clear_padding();
        bitmap
    }

    /// Creates a bitmap from packed bytes. Extra bytes are dropped and padding
    /// bits are cleared.
    pub fn from_bytes(mut bytes: Vec<u8>, len: usize) -> Result<Self> {
        let needed = ceil8(len);
        if bytes.len() < needed {
            return Err(Error::InvalidArgument(format!(
                "a bitmap of {len} bits needs {needed} bytes, got {}",
                bytes.len()
            )));
        }
        bytes.truncate(needed);
        let mut bitmap = Bitmap { bytes, len };
        bitmap.clear_padding();
        Ok(bitmap)
    }

    fn clear_padding(&mut self) {
        let used = self.len % 8;
        if used != 0
            && let Some(last) = self.bytes.last_mut()
        {
            *last &= (1u8 << used) - 1;
        }
    }

    /// Number of bits.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if the bitmap holds no bits.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns bit `i`.
    ///
    /// # Panics
    /// Panics if `i >= len`.
    #[inline]
    pub fn get(&self, i: usize) -> bool {
        assert!(
            i < self.len,
            "bit index {i} out of range for length {}",
            self.len
        );
        self.bytes[i >> 3] & (1 << (i & 7)) != 0
    }

    /// Sets bit `i`.
    ///
    /// # Panics
    /// Panics if `i >= len`.
    #[inline]
    pub fn set(&mut self, i: usize, value: bool) {
        assert!(
            i < self.len,
            "bit index {i} out of range for length {}",
            self.len
        );
        if value {
            self.bytes[i >> 3] |= 1 << (i & 7);
        } else {
            self.bytes[i >> 3] &= !(1 << (i & 7));
        }
    }

    /// Appends a bit.
    #[inline]
    pub fn push(&mut self, value: bool) {
        if self.len.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if value {
            self.bytes[self.len >> 3] |= 1 << (self.len & 7);
        }
        self.len += 1;
    }

    /// Number of set bits.
    pub fn count_ones(&self) -> usize {
        self.bytes.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// The packed bytes (`ceil(len / 8)` of them).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the bitmap and returns the packed bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Iterates over the bits.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = bool> + '_ {
        (0..self.len).map(move |i| self.bytes[i >> 3] & (1 << (i & 7)) != 0)
    }

    /// Appends the bits `range` of `other`.
    pub(crate) fn extend_from(&mut self, other: &Bitmap, range: Range<usize>) {
        if self.len.is_multiple_of(8) && range.start.is_multiple_of(8) {
            // Byte-aligned fast path.
            let n = range.len();
            self.bytes
                .extend_from_slice(&other.bytes[range.start / 8..range.start / 8 + ceil8(n)]);
            self.len += n;
            self.clear_padding();
        } else {
            for i in range {
                self.push(other.get(i));
            }
        }
    }
}

impl FromIterator<bool> for Bitmap {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let mut bitmap = Bitmap::with_capacity(iter.size_hint().0);
        for bit in iter {
            bitmap.push(bit);
        }
        bitmap
    }
}

impl fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bitmap[{}; ", self.len)?;
        for bit in self.iter().take(64) {
            f.write_str(if bit { "1" } else { "0" })?;
        }
        if self.len > 64 {
            f.write_str("…")?;
        }
        f.write_str("]")
    }
}

/// UTF-8 strings stored as an offsets buffer and a data buffer (Arrow `Utf8`).
#[derive(Clone, PartialEq, Eq)]
pub struct Utf8Values {
    offsets: Vec<i32>,
    data: Vec<u8>,
}

impl Default for Utf8Values {
    fn default() -> Self {
        Self::new()
    }
}

impl Utf8Values {
    /// Creates an empty sequence.
    pub fn new() -> Self {
        Utf8Values {
            offsets: vec![0],
            data: Vec::new(),
        }
    }

    /// Creates an empty sequence with room for `strings` strings of `bytes` bytes in total.
    pub fn with_capacity(strings: usize, bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(strings + 1);
        offsets.push(0);
        Utf8Values {
            offsets,
            data: Vec::with_capacity(bytes),
        }
    }

    /// Builds a sequence from Arrow-style buffers, validating them.
    pub fn from_parts(offsets: Vec<i32>, data: Vec<u8>) -> Result<Self> {
        let bad = |msg: &str| Error::InvalidArgument(format!("invalid string buffers: {msg}"));
        let text = std::str::from_utf8(&data).map_err(|_| bad("data is not valid UTF-8"))?;
        if offsets.first() != Some(&0) {
            return Err(bad("the first offset must be 0"));
        }
        if usize::try_from(*offsets.last().unwrap_or(&0)).ok() != Some(data.len()) {
            return Err(bad("the last offset must equal the data length"));
        }
        for pair in offsets.windows(2) {
            if pair[0] > pair[1] {
                return Err(bad("offsets must not decrease"));
            }
        }
        for &offset in &offsets {
            if !text.is_char_boundary(offset as usize) {
                return Err(bad("an offset splits a UTF-8 character"));
            }
        }
        Ok(Utf8Values { offsets, data })
    }

    /// Number of strings.
    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    /// True if there are no strings.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends a string.
    pub fn push(&mut self, s: &str) -> Result<()> {
        let end = i32::try_from(self.data.len() + s.len()).map_err(|_| {
            Error::LimitExceeded("a string column exceeds 2 GiB; read it in smaller batches".into())
        })?;
        self.data.extend_from_slice(s.as_bytes());
        self.offsets.push(end);
        Ok(())
    }

    /// Returns string `i`.
    ///
    /// # Panics
    /// Panics if `i >= len`.
    pub fn get(&self, i: usize) -> &str {
        let range = self.offsets[i] as usize..self.offsets[i + 1] as usize;
        // Every constructor validates UTF-8 and character boundaries.
        std::str::from_utf8(&self.data[range]).expect("Utf8Values holds valid UTF-8")
    }

    /// Iterates over the strings.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }

    /// The offsets buffer (`len + 1` entries, starting at 0).
    pub fn offsets(&self) -> &[i32] {
        &self.offsets
    }

    /// The data buffer.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the sequence and returns `(offsets, data)`.
    pub fn into_parts(self) -> (Vec<i32>, Vec<u8>) {
        (self.offsets, self.data)
    }
}

impl fmt::Debug for Utf8Values {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The values buffer of a column. Null slots hold an arbitrary value
/// (zero, `false` or the empty string).
#[derive(Debug, Clone)]
pub enum Values {
    /// Booleans, bit-packed.
    Bool(Bitmap),
    /// 32-bit integers.
    Int32(Vec<i32>),
    /// 64-bit integers.
    Int64(Vec<i64>),
    /// 32-bit floats.
    Float32(Vec<f32>),
    /// 64-bit floats.
    Float64(Vec<f64>),
    /// UTF-8 strings.
    Utf8(Utf8Values),
}

impl Values {
    /// Type of the values.
    pub fn data_type(&self) -> DataType {
        match self {
            Values::Bool(_) => DataType::Bool,
            Values::Int32(_) => DataType::Int32,
            Values::Int64(_) => DataType::Int64,
            Values::Float32(_) => DataType::Float32,
            Values::Float64(_) => DataType::Float64,
            Values::Utf8(_) => DataType::Utf8,
        }
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        match self {
            Values::Bool(v) => v.len(),
            Values::Int32(v) => v.len(),
            Values::Int64(v) => v.len(),
            Values::Float32(v) => v.len(),
            Values::Float64(v) => v.len(),
            Values::Utf8(v) => v.len(),
        }
    }

    /// True if there are no values.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn empty(data_type: DataType, capacity: usize) -> Values {
        match data_type {
            DataType::Bool => Values::Bool(Bitmap::with_capacity(capacity)),
            DataType::Int32 => Values::Int32(Vec::with_capacity(capacity)),
            DataType::Int64 => Values::Int64(Vec::with_capacity(capacity)),
            DataType::Float32 => Values::Float32(Vec::with_capacity(capacity)),
            DataType::Float64 => Values::Float64(Vec::with_capacity(capacity)),
            DataType::Utf8 => Values::Utf8(Utf8Values::with_capacity(capacity, 0)),
        }
    }
}

/// A single value, used by the row-oriented parts of the API.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Missing value.
    Null,
    /// Boolean.
    Bool(bool),
    /// 32-bit integer.
    Int32(i32),
    /// 64-bit integer.
    Int64(i64),
    /// 32-bit float.
    Float32(f32),
    /// 64-bit float.
    Float64(f64),
    /// String.
    Utf8(String),
}

impl Value {
    /// Type of the value, or `None` for [`Value::Null`].
    pub fn data_type(&self) -> Option<DataType> {
        Some(match self {
            Value::Null => return None,
            Value::Bool(_) => DataType::Bool,
            Value::Int32(_) => DataType::Int32,
            Value::Int64(_) => DataType::Int64,
            Value::Float32(_) => DataType::Float32,
            Value::Float64(_) => DataType::Float64,
            Value::Utf8(_) => DataType::Utf8,
        })
    }

    /// True for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Exact equality: floating-point values are compared bit by bit, so NaN
    /// equals NaN and `0.0` differs from `-0.0`.
    pub fn same_as(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Float32(a), Value::Float32(b)) => a.to_bits() == b.to_bits(),
            (Value::Float64(a), Value::Float64(b)) => a.to_bits() == b.to_bits(),
            (a, b) => a == b,
        }
    }

    /// Converts the value to `data_type` if this is lossless (e.g. an integer
    /// to a float that represents it exactly).
    pub fn cast(self, data_type: DataType) -> Result<Value> {
        let from = match self.data_type() {
            None => return Ok(Value::Null),
            Some(t) if t == data_type => return Ok(self),
            Some(t) => t,
        };
        let lossy = || Error::TypeMismatch {
            column: String::new(),
            expected: data_type,
            actual: from,
        };
        let as_i64 = match &self {
            Value::Int32(v) => Some(i64::from(*v)),
            Value::Int64(v) => Some(*v),
            _ => None,
        };
        let as_f64 = match &self {
            Value::Float32(v) => Some(f64::from(*v)),
            Value::Float64(v) => Some(*v),
            _ => None,
        };
        Ok(match data_type {
            DataType::Int32 => Value::Int32(
                as_i64
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or_else(lossy)?,
            ),
            DataType::Int64 => Value::Int64(as_i64.ok_or_else(lossy)?),
            DataType::Float64 => match (as_i64, as_f64) {
                (Some(i), _) if (i as f64) as i64 == i && i.unsigned_abs() <= 1 << 53 => {
                    Value::Float64(i as f64)
                }
                (_, Some(f)) => Value::Float64(f),
                _ => return Err(lossy()),
            },
            DataType::Float32 => match (as_i64, as_f64) {
                (Some(i), _) if i.unsigned_abs() <= 1 << 24 => Value::Float32(i as f32),
                (_, Some(f)) if f64::from(f as f32).to_bits() == f.to_bits() || f.is_nan() => {
                    Value::Float32(f as f32)
                }
                _ => return Err(lossy()),
            },
            DataType::Bool | DataType::Utf8 => return Err(lossy()),
        })
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Int32(v) => write!(f, "{v}"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Float32(v) => write!(f, "{v}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::Utf8(v) => f.write_str(v),
        }
    }
}

macro_rules! value_from {
    ($($t:ty => $variant:ident),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::$variant(v.into())
            }
        }
    )*};
}
value_from!(bool => Bool, i32 => Int32, i64 => Int64, f32 => Float32, f64 => Float64,
            String => Utf8, &str => Utf8);

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// A column: values plus an optional validity bitmap (bit set = not null).
#[derive(Debug, Clone)]
pub struct Column {
    values: Values,
    validity: Option<Bitmap>,
}

impl Column {
    /// Creates a column. A validity bitmap without nulls is dropped.
    pub fn new(values: Values, validity: Option<Bitmap>) -> Result<Self> {
        if let Some(v) = &validity
            && v.len() != values.len()
        {
            return Err(Error::InvalidArgument(format!(
                "validity bitmap has {} bits for {} values",
                v.len(),
                values.len()
            )));
        }
        let validity = validity.filter(|v| v.count_ones() != v.len());
        Ok(Column { values, validity })
    }

    /// A column of `len` nulls.
    pub fn nulls(data_type: DataType, len: usize) -> Column {
        let mut builder = ColumnBuilder::new(data_type, len);
        for _ in 0..len {
            builder.push_null();
        }
        builder.finish()
    }

    /// Builds a column from values, which must have the given type or be null.
    pub fn from_values(data_type: DataType, values: &[Value]) -> Result<Column> {
        let mut builder = ColumnBuilder::new(data_type, values.len());
        for v in values {
            builder.push_value(v)?;
        }
        Ok(builder.finish())
    }

    /// Type of the values.
    pub fn data_type(&self) -> DataType {
        self.values.data_type()
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True if the column has no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of null rows.
    pub fn null_count(&self) -> usize {
        self.validity
            .as_ref()
            .map_or(0, |v| v.len() - v.count_ones())
    }

    /// True if row `i` is not null.
    pub fn is_valid(&self, i: usize) -> bool {
        self.validity.as_ref().is_none_or(|v| v.get(i))
    }

    /// The values buffer.
    pub fn values(&self) -> &Values {
        &self.values
    }

    /// The validity bitmap, `None` if there are no nulls.
    pub fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    /// Consumes the column and returns its buffers.
    pub fn into_parts(self) -> (Values, Option<Bitmap>) {
        (self.values, self.validity)
    }

    /// Returns row `i` as a [`Value`].
    ///
    /// # Panics
    /// Panics if `i >= len`.
    pub fn value(&self, i: usize) -> Value {
        if !self.is_valid(i) {
            return Value::Null;
        }
        match &self.values {
            Values::Bool(v) => Value::Bool(v.get(i)),
            Values::Int32(v) => Value::Int32(v[i]),
            Values::Int64(v) => Value::Int64(v[i]),
            Values::Float32(v) => Value::Float32(v[i]),
            Values::Float64(v) => Value::Float64(v[i]),
            Values::Utf8(v) => Value::Utf8(v.get(i).to_owned()),
        }
    }

    /// Boolean values, if this is a `bool` column.
    pub fn as_bool(&self) -> Option<&Bitmap> {
        match &self.values {
            Values::Bool(v) => Some(v),
            _ => None,
        }
    }

    /// Values, if this is an `int32` column.
    pub fn as_i32(&self) -> Option<&[i32]> {
        match &self.values {
            Values::Int32(v) => Some(v),
            _ => None,
        }
    }

    /// Values, if this is an `int64` column.
    pub fn as_i64(&self) -> Option<&[i64]> {
        match &self.values {
            Values::Int64(v) => Some(v),
            _ => None,
        }
    }

    /// Values, if this is a `float32` column.
    pub fn as_f32(&self) -> Option<&[f32]> {
        match &self.values {
            Values::Float32(v) => Some(v),
            _ => None,
        }
    }

    /// Values, if this is a `float64` column.
    pub fn as_f64(&self) -> Option<&[f64]> {
        match &self.values {
            Values::Float64(v) => Some(v),
            _ => None,
        }
    }

    /// Values, if this is a `string` column.
    pub fn as_utf8(&self) -> Option<&Utf8Values> {
        match &self.values {
            Values::Utf8(v) => Some(v),
            _ => None,
        }
    }

    /// Copies the rows `range` into a new column.
    pub fn slice(&self, range: Range<usize>) -> Column {
        let mut builder = ColumnBuilder::new(self.data_type(), range.len());
        builder
            .extend_from(self, range)
            .expect("a slice of a column never exceeds its limits");
        builder.finish()
    }
}

/// Columns are equal if they have the same type and length, the same null
/// positions, and the same non-null values (floats compared bit by bit).
impl PartialEq for Column {
    fn eq(&self, other: &Self) -> bool {
        if self.data_type() != other.data_type() || self.len() != other.len() {
            return false;
        }
        (0..self.len()).all(|i| self.value(i).same_as(&other.value(i)))
    }
}

macro_rules! column_from_vec {
    ($($t:ty => $variant:ident),*) => {$(
        impl From<Vec<$t>> for Column {
            fn from(v: Vec<$t>) -> Self {
                Column { values: Values::$variant(v), validity: None }
            }
        }
        impl From<Vec<Option<$t>>> for Column {
            fn from(v: Vec<Option<$t>>) -> Self {
                let validity: Bitmap = v.iter().map(Option::is_some).collect();
                let values = v.into_iter().map(Option::unwrap_or_default).collect();
                Column::new(Values::$variant(values), Some(validity))
                    .expect("lengths match by construction")
            }
        }
    )*};
}
column_from_vec!(i32 => Int32, i64 => Int64, f32 => Float32, f64 => Float64);

impl From<Vec<bool>> for Column {
    fn from(v: Vec<bool>) -> Self {
        Column {
            values: Values::Bool(v.into_iter().collect()),
            validity: None,
        }
    }
}

impl From<Vec<Option<bool>>> for Column {
    fn from(v: Vec<Option<bool>>) -> Self {
        let validity: Bitmap = v.iter().map(Option::is_some).collect();
        let values: Bitmap = v.into_iter().map(Option::unwrap_or_default).collect();
        Column::new(Values::Bool(values), Some(validity)).expect("lengths match by construction")
    }
}

fn strings_column<'a>(items: impl ExactSizeIterator<Item = Option<&'a str>>) -> Column {
    let mut builder = ColumnBuilder::new(DataType::Utf8, items.len());
    for item in items {
        match item {
            Some(s) => builder
                .push_str(s)
                .expect("string column exceeds 2 GiB; split the data into smaller batches"),
            None => builder.push_null(),
        }
    }
    builder.finish()
}

impl From<Vec<&str>> for Column {
    fn from(v: Vec<&str>) -> Self {
        strings_column(v.into_iter().map(Some))
    }
}

impl From<Vec<String>> for Column {
    fn from(v: Vec<String>) -> Self {
        strings_column(v.iter().map(|s| Some(s.as_str())))
    }
}

impl From<Vec<Option<&str>>> for Column {
    fn from(v: Vec<Option<&str>>) -> Self {
        strings_column(v.into_iter())
    }
}

impl From<Vec<Option<String>>> for Column {
    fn from(v: Vec<Option<String>>) -> Self {
        strings_column(v.iter().map(Option::as_deref))
    }
}

/// Incrementally builds a [`Column`].
#[derive(Debug)]
pub(crate) struct ColumnBuilder {
    values: Values,
    validity: Option<Bitmap>,
}

impl ColumnBuilder {
    pub(crate) fn new(data_type: DataType, capacity: usize) -> Self {
        ColumnBuilder {
            values: Values::empty(data_type, capacity),
            validity: None,
        }
    }

    pub(crate) fn data_type(&self) -> DataType {
        self.values.data_type()
    }

    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    fn mark(&mut self, valid: bool) {
        match &mut self.validity {
            Some(bitmap) => bitmap.push(valid),
            None if valid => {}
            None => {
                let mut bitmap = Bitmap::new_filled(self.values.len(), true);
                bitmap.push(false);
                self.validity = Some(bitmap);
            }
        }
    }

    pub(crate) fn push_null(&mut self) {
        self.mark(false);
        match &mut self.values {
            Values::Bool(v) => v.push(false),
            Values::Int32(v) => v.push(0),
            Values::Int64(v) => v.push(0),
            Values::Float32(v) => v.push(0.0),
            Values::Float64(v) => v.push(0.0),
            Values::Utf8(v) => {
                // An empty string never overflows the offsets.
                let _ = v.push("");
            }
        }
    }

    pub(crate) fn push_str(&mut self, s: &str) -> Result<()> {
        match &mut self.values {
            Values::Utf8(v) => v.push(s)?,
            other => {
                return Err(Error::TypeMismatch {
                    column: String::new(),
                    expected: other.data_type(),
                    actual: DataType::Utf8,
                });
            }
        }
        self.mark(true);
        Ok(())
    }

    /// Appends a value of the builder's type or a null.
    pub(crate) fn push_value(&mut self, value: &Value) -> Result<()> {
        let mismatch = |expected: DataType| Error::TypeMismatch {
            column: String::new(),
            expected,
            actual: value.data_type().unwrap_or(expected),
        };
        match (&mut self.values, value) {
            (_, Value::Null) => {
                self.push_null();
                return Ok(());
            }
            (Values::Bool(v), Value::Bool(x)) => v.push(*x),
            (Values::Int32(v), Value::Int32(x)) => v.push(*x),
            (Values::Int64(v), Value::Int64(x)) => v.push(*x),
            (Values::Float32(v), Value::Float32(x)) => v.push(*x),
            (Values::Float64(v), Value::Float64(x)) => v.push(*x),
            (Values::Utf8(v), Value::Utf8(x)) => v.push(x)?,
            (values, _) => return Err(mismatch(values.data_type())),
        }
        self.mark(true);
        Ok(())
    }

    /// Appends row `row` of `column`, which must have the builder's type.
    pub(crate) fn push_from(&mut self, column: &Column, row: usize) -> Result<()> {
        if !column.is_valid(row) {
            self.push_null();
            return Ok(());
        }
        match (&mut self.values, &column.values) {
            (Values::Bool(d), Values::Bool(s)) => d.push(s.get(row)),
            (Values::Int32(d), Values::Int32(s)) => d.push(s[row]),
            (Values::Int64(d), Values::Int64(s)) => d.push(s[row]),
            (Values::Float32(d), Values::Float32(s)) => d.push(s[row]),
            (Values::Float64(d), Values::Float64(s)) => d.push(s[row]),
            (Values::Utf8(d), Values::Utf8(s)) => d.push(s.get(row))?,
            (d, s) => {
                return Err(Error::TypeMismatch {
                    column: String::new(),
                    expected: d.data_type(),
                    actual: s.data_type(),
                });
            }
        }
        self.mark(true);
        Ok(())
    }

    /// Appends the rows `range` of `column`, which must have the builder's type.
    pub(crate) fn extend_from(&mut self, column: &Column, range: Range<usize>) -> Result<()> {
        let start_len = self.len();
        match (&mut self.values, &column.values) {
            (Values::Bool(d), Values::Bool(s)) => d.extend_from(s, range.clone()),
            (Values::Int32(d), Values::Int32(s)) => d.extend_from_slice(&s[range.clone()]),
            (Values::Int64(d), Values::Int64(s)) => d.extend_from_slice(&s[range.clone()]),
            (Values::Float32(d), Values::Float32(s)) => d.extend_from_slice(&s[range.clone()]),
            (Values::Float64(d), Values::Float64(s)) => d.extend_from_slice(&s[range.clone()]),
            (Values::Utf8(d), Values::Utf8(s)) => {
                for i in range.clone() {
                    d.push(s.get(i))?;
                }
            }
            (d, s) => {
                return Err(Error::TypeMismatch {
                    column: String::new(),
                    expected: d.data_type(),
                    actual: s.data_type(),
                });
            }
        }
        match (&mut self.validity, &column.validity) {
            (None, None) => {}
            (Some(d), None) => {
                d.extend_from(&Bitmap::new_filled(range.len(), true), 0..range.len())
            }
            (Some(d), Some(s)) => d.extend_from(s, range),
            (None, Some(s)) => {
                let mut bitmap = Bitmap::new_filled(start_len, true);
                bitmap.extend_from(s, range);
                self.validity = Some(bitmap);
            }
        }
        Ok(())
    }

    /// Returns row `i` as a [`Value`].
    pub(crate) fn value(&self, i: usize) -> Value {
        if self.validity.as_ref().is_some_and(|v| !v.get(i)) {
            return Value::Null;
        }
        match &self.values {
            Values::Bool(v) => Value::Bool(v.get(i)),
            Values::Int32(v) => Value::Int32(v[i]),
            Values::Int64(v) => Value::Int64(v[i]),
            Values::Float32(v) => Value::Float32(v[i]),
            Values::Float64(v) => Value::Float64(v[i]),
            Values::Utf8(v) => Value::Utf8(v.get(i).to_owned()),
        }
    }

    /// Approximate size of the buffered data in bytes.
    pub(crate) fn byte_size(&self) -> usize {
        match &self.values {
            Values::Bool(v) => v.as_bytes().len(),
            Values::Utf8(v) => v.data().len() + 4 * v.len(),
            other => other.len() * other.data_type().plain_width(),
        }
    }

    pub(crate) fn finish(self) -> Column {
        Column::new(self.values, self.validity).expect("builder keeps lengths in sync")
    }
}

/// Rows of one device: a time column and named value columns.
#[derive(Debug, Clone)]
pub struct Batch {
    time: Vec<i64>,
    names: Arc<[String]>,
    columns: Vec<Column>,
}

impl Batch {
    /// Creates a batch from time stamps and named columns of equal length.
    pub fn new<S: Into<String>>(time: Vec<i64>, columns: Vec<(S, Column)>) -> Result<Self> {
        let (names, columns): (Vec<String>, Vec<Column>) =
            columns.into_iter().map(|(n, c)| (n.into(), c)).unzip();
        Self::try_from_parts(time, names.into(), columns)
    }

    pub(crate) fn try_from_parts(
        time: Vec<i64>,
        names: Arc<[String]>,
        columns: Vec<Column>,
    ) -> Result<Self> {
        if names.len() != columns.len() {
            return Err(Error::InvalidArgument(
                "one name per column is required".into(),
            ));
        }
        for (name, column) in names.iter().zip(&columns) {
            if column.len() != time.len() {
                return Err(Error::InvalidArgument(format!(
                    "column '{name}' has {} rows, the time column has {}",
                    column.len(),
                    time.len()
                )));
            }
        }
        for (i, name) in names.iter().enumerate() {
            if names[..i].contains(name) {
                return Err(Error::InvalidArgument(format!("duplicate column '{name}'")));
            }
        }
        Ok(Batch {
            time,
            names,
            columns,
        })
    }

    /// An empty batch with the given column names and types.
    pub fn empty(names: Arc<[String]>, types: &[DataType]) -> Batch {
        Batch {
            time: Vec::new(),
            columns: types.iter().map(|&t| Column::nulls(t, 0)).collect(),
            names,
        }
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.time.len()
    }

    /// True if the batch has no rows.
    pub fn is_empty(&self) -> bool {
        self.time.is_empty()
    }

    /// Time stamps, in the unit of the device.
    pub fn time(&self) -> &[i64] {
        &self.time
    }

    /// Column names.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Value columns.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The column with the given name.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.names
            .iter()
            .position(|n| n == name)
            .map(|i| &self.columns[i])
    }

    /// Row `i` as values, in column order.
    ///
    /// # Panics
    /// Panics if `i >= len`.
    pub fn row(&self, i: usize) -> Vec<Value> {
        self.columns.iter().map(|c| c.value(i)).collect()
    }

    /// Consumes the batch and returns `(time, names, columns)`.
    pub fn into_parts(self) -> (Vec<i64>, Vec<String>, Vec<Column>) {
        (self.time, self.names.to_vec(), self.columns)
    }

    /// Copies the rows `range` into a new batch.
    pub fn slice(&self, range: Range<usize>) -> Batch {
        Batch {
            time: self.time[range.clone()].to_vec(),
            names: self.names.clone(),
            columns: self
                .columns
                .iter()
                .map(|c| c.slice(range.clone()))
                .collect(),
        }
    }

    /// Concatenates batches with identical column names and types.
    pub fn concat(batches: &[Batch]) -> Result<Batch> {
        let Some(first) = batches.first() else {
            return Err(Error::InvalidArgument(
                "cannot concatenate zero batches".into(),
            ));
        };
        let total: usize = batches.iter().map(Batch::len).sum();
        let mut time = Vec::with_capacity(total);
        let mut builders: Vec<ColumnBuilder> = first
            .columns
            .iter()
            .map(|c| ColumnBuilder::new(c.data_type(), total))
            .collect();
        for batch in batches {
            if batch.names != first.names
                || batch
                    .columns
                    .iter()
                    .zip(&builders)
                    .any(|(c, b)| c.data_type() != b.data_type())
            {
                return Err(Error::InvalidArgument(
                    "batches to concatenate must have the same columns".into(),
                ));
            }
            time.extend_from_slice(&batch.time);
            for (builder, column) in builders.iter_mut().zip(&batch.columns) {
                builder.extend_from(column, 0..column.len())?;
            }
        }
        Ok(Batch {
            time,
            names: first.names.clone(),
            columns: builders.into_iter().map(ColumnBuilder::finish).collect(),
        })
    }
}

impl PartialEq for Batch {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.names == other.names && self.columns == other.columns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_basics() {
        let mut b: Bitmap = [true, false, true].into_iter().collect();
        assert_eq!(b.len(), 3);
        assert_eq!(b.count_ones(), 2);
        b.set(1, true);
        assert!(b.get(1));
        assert_eq!(b.as_bytes(), &[0b111]);
        let filled = Bitmap::new_filled(10, true);
        assert_eq!(filled.as_bytes(), &[0xff, 0b11]);
        let from = Bitmap::from_bytes(vec![0xff, 0xff, 0xff], 9).unwrap();
        assert_eq!(from.as_bytes(), &[0xff, 0x01]);
        assert!(Bitmap::from_bytes(vec![0xff], 9).is_err());
    }

    #[test]
    fn bitmap_extend_unaligned() {
        let src: Bitmap = (0..20).map(|i| i % 3 == 0).collect();
        let mut dst: Bitmap = [true].into_iter().collect();
        dst.extend_from(&src, 5..17);
        let expected: Vec<bool> = std::iter::once(true)
            .chain((5..17).map(|i| i % 3 == 0))
            .collect();
        assert_eq!(dst.iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn utf8_values_validation() {
        let mut v = Utf8Values::new();
        v.push("привет").unwrap();
        v.push("").unwrap();
        assert_eq!(v.iter().collect::<Vec<_>>(), ["привет", ""]);
        let (offsets, data) = v.into_parts();
        assert!(Utf8Values::from_parts(offsets.clone(), data.clone()).is_ok());
        assert!(Utf8Values::from_parts(vec![0, 1, 12, 12], data.clone()).is_err());
        assert!(Utf8Values::from_parts(vec![1, 12, 12], data).is_err());
    }

    #[test]
    fn column_nulls_and_equality() {
        let c: Column = vec![Some(1.0), None, Some(f64::NAN)].into();
        assert_eq!(c.null_count(), 1);
        assert_eq!(c.value(1), Value::Null);
        assert_eq!(c, c.clone());
        let d: Column = vec![Some(1.0), None, Some(f64::NAN)].into();
        assert_eq!(c, d);
        let no_nulls: Column = vec![Some(1i64), Some(2)].into();
        assert!(no_nulls.validity().is_none());
    }

    #[test]
    fn builder_mixes_nulls() {
        let mut b = ColumnBuilder::new(DataType::Utf8, 0);
        b.push_value(&Value::from("a")).unwrap();
        b.push_null();
        b.push_value(&Value::from("б")).unwrap();
        assert!(b.push_value(&Value::Int32(1)).is_err());
        let c = b.finish();
        assert_eq!(c.value(0), Value::from("a"));
        assert_eq!(c.value(1), Value::Null);
        assert_eq!(c.value(2), Value::from("б"));
    }

    #[test]
    fn batch_concat_and_slice() {
        let a = Batch::new(
            vec![1, 2],
            vec![("x", Column::from(vec![Some(1i32), None]))],
        )
        .unwrap();
        let b = Batch::new(vec![3], vec![("x", Column::from(vec![7i32]))]).unwrap();
        let c = Batch::concat(&[a, b]).unwrap();
        assert_eq!(c.time(), &[1, 2, 3]);
        assert_eq!(c.row(1), vec![Value::Null]);
        assert_eq!(c.row(2), vec![Value::Int32(7)]);
        assert_eq!(c.slice(1..3).time(), &[2, 3]);
        assert!(Batch::new(vec![1], vec![("x", Column::from(vec![1i32, 2]))]).is_err());
    }

    #[test]
    fn value_casts() {
        assert_eq!(
            Value::Int64(3).cast(DataType::Float64).unwrap(),
            Value::Float64(3.0)
        );
        assert_eq!(
            Value::Int64(3).cast(DataType::Int32).unwrap(),
            Value::Int32(3)
        );
        assert!(Value::Int64(1 << 40).cast(DataType::Int32).is_err());
        assert!(Value::Float64(0.1).cast(DataType::Float32).is_err());
        assert_eq!(
            Value::Float64(0.5).cast(DataType::Float32).unwrap(),
            Value::Float32(0.5)
        );
        assert!(Value::Float64(1.0).cast(DataType::Int64).is_err());
        assert!(Value::from("x").cast(DataType::Bool).is_err());
        assert_eq!(Value::Null.cast(DataType::Bool).unwrap(), Value::Null);
    }
}
