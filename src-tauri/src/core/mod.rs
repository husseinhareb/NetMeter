//! Domain types and pure business logic.
//!
//! This module is deliberately free of Tauri, SQLite and Linux-specific APIs.
//! Everything here is either a plain data type or a deterministic function, so
//! the parts of NetMeter that are easy to get wrong -- delta arithmetic,
//! counter-reset detection, calendar boundaries across DST -- can be unit
//! tested without a database, a kernel or a GUI.

pub mod config;
pub mod errors;
pub mod statistics;
pub mod time;
pub mod types;

pub use config::{Config, InterfacePolicy, RetentionPolicy};
pub use errors::{Error, Result};
pub use types::{
    CounterSample, DataRate, Granularity, InterfaceKind, InterfaceState, MonitorState,
    MonitorStatus, NetworkCounters, NetworkInterface, Traffic, TrafficDelta, UsagePeriod,
    UsageSummary,
};
