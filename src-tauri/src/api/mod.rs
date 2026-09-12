//! The boundary the frontend talks to. Nothing below this module knows a GUI
//! exists, and nothing above it knows SQL or `/proc` exist.
#[cfg(feature = "gui")]
pub mod commands;
pub mod events;
pub mod helper;
pub mod ipc;
pub mod models;

pub use events::{EventSink, InterfaceChanged, LiveUsage, NullEventSink};
