//! Schema types: value types, time units, columns and devices.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Maximum number of value columns of a device.
pub const MAX_COLUMNS: usize = 65_535;

/// Type of the values of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DataType {
    /// Boolean.
    Bool,
    /// 32-bit signed integer.
    Int32,
    /// 64-bit signed integer.
    Int64,
    /// 32-bit IEEE 754 floating-point number.
    Float32,
    /// 64-bit IEEE 754 floating-point number.
    Float64,
    /// UTF-8 string.
    Utf8,
}

impl DataType {
    /// All value types, in the order of their codes.
    pub const ALL: [DataType; 6] = [
        DataType::Bool,
        DataType::Int32,
        DataType::Int64,
        DataType::Float32,
        DataType::Float64,
        DataType::Utf8,
    ];

    pub(crate) fn code(self) -> u8 {
        match self {
            DataType::Bool => 0,
            DataType::Int32 => 1,
            DataType::Int64 => 2,
            DataType::Float32 => 3,
            DataType::Float64 => 4,
            DataType::Utf8 => 5,
        }
    }

    pub(crate) fn from_code(code: u8) -> Result<Self> {
        Self::ALL
            .get(usize::from(code))
            .copied()
            .ok_or_else(|| Error::Unsupported(format!("column type code {code}")))
    }

    /// The name used in the CLI and the Python API (`"float64"`, …).
    pub fn name(self) -> &'static str {
        match self {
            DataType::Bool => "bool",
            DataType::Int32 => "int32",
            DataType::Int64 => "int64",
            DataType::Float32 => "float32",
            DataType::Float64 => "float64",
            DataType::Utf8 => "string",
        }
    }

    /// Size of one value in `Plain` encoding (strings: without the length prefix).
    pub(crate) fn plain_width(self) -> usize {
        match self {
            DataType::Bool => 1,
            DataType::Int32 | DataType::Float32 => 4,
            DataType::Int64 | DataType::Float64 => 8,
            DataType::Utf8 => 0,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for DataType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "bool" | "boolean" => DataType::Bool,
            "int32" | "i32" | "int" => DataType::Int32,
            "int64" | "i64" | "long" => DataType::Int64,
            "float32" | "f32" | "float" => DataType::Float32,
            "float64" | "f64" | "double" => DataType::Float64,
            "string" | "str" | "utf8" | "text" => DataType::Utf8,
            _ => return Err(Error::InvalidArgument(format!("unknown data type '{s}'"))),
        })
    }
}

/// Unit of the time stamps of a device. Time stamps are `i64` counts of this
/// unit since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TimeUnit {
    /// Seconds.
    Second,
    /// Milliseconds.
    Millisecond,
    /// Microseconds.
    Microsecond,
    /// Nanoseconds.
    Nanosecond,
}

impl TimeUnit {
    /// All time units, from the coarsest to the finest.
    pub const ALL: [TimeUnit; 4] = [
        TimeUnit::Second,
        TimeUnit::Millisecond,
        TimeUnit::Microsecond,
        TimeUnit::Nanosecond,
    ];

    pub(crate) fn code(self) -> u8 {
        match self {
            TimeUnit::Second => 0,
            TimeUnit::Millisecond => 1,
            TimeUnit::Microsecond => 2,
            TimeUnit::Nanosecond => 3,
        }
    }

    pub(crate) fn from_code(code: u8) -> Result<Self> {
        Self::ALL
            .get(usize::from(code))
            .copied()
            .ok_or_else(|| Error::Unsupported(format!("time unit code {code}")))
    }

    /// Short name: `"s"`, `"ms"`, `"us"` or `"ns"`.
    pub fn name(self) -> &'static str {
        match self {
            TimeUnit::Second => "s",
            TimeUnit::Millisecond => "ms",
            TimeUnit::Microsecond => "us",
            TimeUnit::Nanosecond => "ns",
        }
    }

    /// Number of units in one second.
    pub fn per_second(self) -> i64 {
        match self {
            TimeUnit::Second => 1,
            TimeUnit::Millisecond => 1_000,
            TimeUnit::Microsecond => 1_000_000,
            TimeUnit::Nanosecond => 1_000_000_000,
        }
    }

    /// Converts nanoseconds since the epoch to this unit, rounding towards
    /// negative infinity.
    pub fn from_nanos(self, nanos: i64) -> i64 {
        nanos.div_euclid(1_000_000_000 / self.per_second())
    }

    /// Converts a time stamp in this unit to nanoseconds, or `None` on overflow.
    pub fn to_nanos(self, value: i64) -> Option<i64> {
        value.checked_mul(1_000_000_000 / self.per_second())
    }
}

impl fmt::Display for TimeUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for TimeUnit {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "s" | "sec" | "second" | "seconds" => TimeUnit::Second,
            "ms" | "millisecond" | "milliseconds" => TimeUnit::Millisecond,
            "us" | "µs" | "microsecond" | "microseconds" => TimeUnit::Microsecond,
            "ns" | "nanosecond" | "nanoseconds" => TimeUnit::Nanosecond,
            _ => return Err(Error::InvalidArgument(format!("unknown time unit '{s}'"))),
        })
    }
}

/// Definition of a value column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    /// Column name, unique within the device.
    pub name: String,
    /// Type of the values.
    pub data_type: DataType,
    /// Free-form metadata, e.g. `unit = "MB"`.
    pub metadata: BTreeMap<String, String>,
}

impl ColumnSchema {
    /// Creates a column without metadata.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        ColumnSchema {
            name: name.into(),
            data_type,
            metadata: BTreeMap::new(),
        }
    }

    /// Adds a metadata entry.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Definition of a device: a time column shared by a set of value columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSchema {
    /// Device name, unique within the file.
    pub name: String,
    /// Unit of the time stamps.
    pub time_unit: TimeUnit,
    /// Optional time zone, passed through to Arrow (IANA name or `+HH:MM`).
    pub timezone: Option<String>,
    /// Name of the time column (`"time"` by default).
    pub time_name: String,
    /// Value columns.
    pub columns: Vec<ColumnSchema>,
    /// Free-form metadata.
    pub metadata: BTreeMap<String, String>,
}

impl DeviceSchema {
    /// Creates a device without columns; add them with [`DeviceSchema::with_column`].
    pub fn new(name: impl Into<String>, time_unit: TimeUnit) -> Self {
        DeviceSchema {
            name: name.into(),
            time_unit,
            timezone: None,
            time_name: "time".to_owned(),
            columns: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    /// Adds a value column.
    pub fn with_column(mut self, name: impl Into<String>, data_type: DataType) -> Self {
        self.columns.push(ColumnSchema::new(name, data_type));
        self
    }

    /// Adds a value column with metadata.
    pub fn with_column_schema(mut self, column: ColumnSchema) -> Self {
        self.columns.push(column);
        self
    }

    /// Sets the time zone.
    pub fn with_timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = Some(timezone.into());
        self
    }

    /// Sets the name of the time column.
    pub fn with_time_name(mut self, name: impl Into<String>) -> Self {
        self.time_name = name.into();
        self
    }

    /// Adds a metadata entry.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Position of the value column with the given name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Checks the constraints of SPEC §5.2.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(Error::Schema("device name must not be empty".into()));
        }
        if self.time_name.is_empty() {
            return Err(Error::Schema("time column name must not be empty".into()));
        }
        if self.columns.is_empty() {
            return Err(Error::Schema(format!(
                "device '{}' must have at least one value column",
                self.name
            )));
        }
        if self.columns.len() > MAX_COLUMNS {
            return Err(Error::Schema(format!(
                "device '{}' has {} columns; at most {MAX_COLUMNS} are allowed",
                self.name,
                self.columns.len()
            )));
        }
        let mut seen = HashSet::with_capacity(self.columns.len() + 1);
        seen.insert(self.time_name.as_str());
        for column in &self.columns {
            if column.name.is_empty() {
                return Err(Error::Schema("column names must not be empty".into()));
            }
            if !seen.insert(column.name.as_str()) {
                return Err(Error::Schema(format!(
                    "duplicate column name '{}' in device '{}'",
                    column.name, self.name
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for t in DataType::ALL {
            assert_eq!(DataType::from_code(t.code()).unwrap(), t);
            assert_eq!(t.name().parse::<DataType>().unwrap(), t);
        }
        for u in TimeUnit::ALL {
            assert_eq!(TimeUnit::from_code(u.code()).unwrap(), u);
            assert_eq!(u.name().parse::<TimeUnit>().unwrap(), u);
        }
        assert!(DataType::from_code(6).is_err());
        assert!(TimeUnit::from_code(4).is_err());
    }

    #[test]
    fn time_conversions() {
        assert_eq!(TimeUnit::Millisecond.from_nanos(1_500_000), 1);
        assert_eq!(TimeUnit::Millisecond.from_nanos(-1), -1);
        assert_eq!(TimeUnit::Second.to_nanos(2), Some(2_000_000_000));
        assert_eq!(TimeUnit::Second.to_nanos(i64::MAX), None);
    }

    #[test]
    fn validation() {
        let ok = DeviceSchema::new("vm01", TimeUnit::Millisecond)
            .with_column("cpu", DataType::Float64)
            .with_column("состояние", DataType::Utf8);
        ok.validate().unwrap();
        assert_eq!(ok.column_index("состояние"), Some(1));

        let no_columns = DeviceSchema::new("x", TimeUnit::Second);
        assert!(no_columns.validate().is_err());
        let duplicate = DeviceSchema::new("x", TimeUnit::Second)
            .with_column("a", DataType::Bool)
            .with_column("a", DataType::Bool);
        assert!(duplicate.validate().is_err());
        let clash = DeviceSchema::new("x", TimeUnit::Second).with_column("time", DataType::Bool);
        assert!(clash.validate().is_err());
    }
}
