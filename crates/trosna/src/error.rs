//! Error type of the crate.

use crate::types::DataType;

/// Result type used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors produced while reading, writing or verifying Trosna files.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An operating-system level I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The file does not start with the Trosna magic bytes.
    #[error("not a Trosna file (bad magic bytes)")]
    NotTrosna,

    /// The file was written with an incompatible major version of the format.
    #[error("unsupported format version {major}.{minor}; this library reads version 1.x")]
    UnsupportedVersion {
        /// Major version found in the file header.
        major: u8,
        /// Minor version found in the file header.
        minor: u8,
    },

    /// The file contains malformed or damaged data.
    #[error("{}", match offset {
        Some(offset) => format!("corrupted data at offset {offset}: {reason}"),
        None => format!("corrupted data: {reason}"),
    })]
    Corrupted {
        /// Offset of the damaged frame, if known.
        offset: Option<u64>,
        /// Human-readable description of the problem.
        reason: String,
    },

    /// The file uses a feature (frame kind, encoding, codec) unknown to this version.
    #[error("unsupported feature: {0}")]
    Unsupported(String),

    /// The file was not closed properly and strict opening was requested.
    #[error(
        "the file is not finalized (its writer crashed or is still writing); \
         open it in non-strict mode or run `trosna recover`"
    )]
    NotFinalized,

    /// Another process holds the writer lock on the file.
    #[error("the file is locked by another writer")]
    Locked,

    /// The named device does not exist.
    #[error("unknown device '{0}'")]
    UnknownDevice(String),

    /// A device with this name already exists.
    #[error("device '{0}' already exists")]
    DeviceExists(String),

    /// The named column does not exist in the device.
    #[error("unknown column '{column}' in device '{device}'")]
    UnknownColumn {
        /// Device name.
        device: String,
        /// Column name.
        column: String,
    },

    /// A column was given values of the wrong type.
    #[error("column '{column}' has type {expected}, but {actual} values were given")]
    TypeMismatch {
        /// Column name.
        column: String,
        /// Type declared in the schema.
        expected: DataType,
        /// Type of the supplied values.
        actual: DataType,
    },

    /// A schema is invalid or does not match the existing one.
    #[error("schema error: {0}")]
    Schema(String),

    /// There is no point at the given time stamp.
    #[error("device '{device}' has no point at time {time}")]
    PointNotFound {
        /// Device name.
        device: String,
        /// Time stamp in the unit of the device.
        time: i64,
    },

    /// There is no annotation with the given identifier.
    #[error("unknown annotation {0}")]
    UnknownAnnotation(u64),

    /// There is no commit with the given number.
    #[error("unknown commit {0}")]
    UnknownCommit(u64),

    /// An argument passed to the API is invalid.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A size limit of the format was exceeded.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    /// The writer was used after a failed operation left it in an unusable state.
    #[error("the writer is poisoned by an earlier I/O error and must be dropped")]
    Poisoned,
}

impl Error {
    /// Creates a [`Error::Corrupted`] error without an offset.
    pub(crate) fn corrupted(reason: impl Into<String>) -> Self {
        Error::Corrupted {
            offset: None,
            reason: reason.into(),
        }
    }

    /// Attaches a file offset to a [`Error::Corrupted`] error that has none.
    pub(crate) fn at(self, offset: u64) -> Self {
        match self {
            Error::Corrupted {
                offset: None,
                reason,
            } => Error::Corrupted {
                offset: Some(offset),
                reason,
            },
            other => other,
        }
    }

    /// Returns true if the error indicates damaged or malformed file content.
    pub fn is_corruption(&self) -> bool {
        matches!(self, Error::Corrupted { .. } | Error::NotTrosna)
    }
}
