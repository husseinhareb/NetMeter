//! Error types, one per layer boundary.
//!
//! The layers are kept separate so a caller can tell "the kernel counters could
//! not be read" from "the database is read-only" -- those need different
//! recovery. They all funnel into [`Error`], which is what crosses the Tauri
//! command boundary.

use std::fmt;

/// Failure reading kernel-provided network statistics.
#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not in the expected format: {detail}")]
    Format { path: String, detail: String },
    #[error("monitor is already running")]
    AlreadyRunning,
    #[error("monitor is not running")]
    NotRunning,
    #[error("another NetMeter instance already owns the database")]
    AlreadyLocked,
}

/// Failure in the persistence layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot prepare the data directory {path}: {source}")]
    DataDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("migration {version} failed: {source}")]
    Migration {
        version: i32,
        #[source]
        source: rusqlite::Error,
    },
    #[error("the database is read-only; usage is not being recorded")]
    ReadOnly,
    #[error("the database file is full; usage is not being recorded")]
    DiskFull,
}

impl StorageError {
    /// Classify a rusqlite error into the cases the engine reacts to
    /// differently: transient (retry next flush) vs. fatal-until-fixed.
    pub fn from_sqlite(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode::*;
        if let rusqlite::Error::SqliteFailure(f, _) = &e {
            return match f.code {
                DiskFull => StorageError::DiskFull,
                ReadOnly => StorageError::ReadOnly,
                _ => StorageError::Sqlite(e),
            };
        }
        StorageError::Sqlite(e)
    }

    /// True when retrying the same work later is likely to succeed and is safe.
    pub fn is_transient(&self) -> bool {
        use rusqlite::ErrorCode::*;
        match self {
            StorageError::DiskFull | StorageError::ReadOnly => true,
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(f, _)) => {
                matches!(f.code, DatabaseBusy | DatabaseLocked | SystemIoFailure)
            }
            _ => false,
        }
    }
}

/// Failure in configuration handling.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config at {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write config at {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config is not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid interface pattern {pattern:?}: {detail}")]
    Pattern { pattern: String, detail: String },
    #[error("unknown timezone {0:?}")]
    Timezone(String),
    #[error("{field} must be between {min} and {max}, got {value}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        value: u64,
    },
}

/// The error type that crosses the API boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Monitor(#[from] MonitorError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0}")]
    BadRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Storage(StorageError::from_sqlite(e))
    }
}

/// A machine-readable tag so the frontend can branch on the kind of failure
/// without parsing the message.
impl Error {
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Monitor(_) => "monitor",
            Error::Storage(StorageError::ReadOnly) => "storage_read_only",
            Error::Storage(StorageError::DiskFull) => "storage_disk_full",
            Error::Storage(_) => "storage",
            Error::Config(_) => "config",
            Error::BadRequest(_) => "bad_request",
            Error::Internal(_) => "internal",
        }
    }
}

/// Tauri requires command errors to be serializable. A tagged object keeps the
/// frontend from string-matching on the message.
impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Error", 2)?;
        st.serialize_field("kind", self.kind())?;
        st.serialize_field("message", &self.to_string())?;
        st.end()
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Rate-limited logging for conditions that repeat every tick (a malformed
/// `/proc/net/dev` line, an interface that will not classify). Without this a
/// persistent fault writes a log line every few seconds forever.
pub struct Throttle {
    last: std::sync::Mutex<Option<std::time::Instant>>,
    every: std::time::Duration,
}

impl Throttle {
    pub const fn new(every: std::time::Duration) -> Self {
        Self {
            last: std::sync::Mutex::new(None),
            every,
        }
    }

    /// True at most once per `every`.
    pub fn allow(&self) -> bool {
        // A poisoned lock here means a previous caller panicked while holding
        // it; the guarded value is a timestamp, so recovering is safe.
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        match *g {
            Some(t) if now.duration_since(t) < self.every => false,
            _ => {
                *g = Some(now);
                true
            }
        }
    }
}

impl fmt::Debug for Throttle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Throttle")
    }
}
