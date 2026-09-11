//! The boundary the frontend talks to. Nothing below this module knows a GUI
//! exists, and nothing above it knows SQL or `/proc` exist.
pub mod commands;
pub mod events;
pub mod models;

pub use events::{EventSink, InterfaceChanged, LiveUsage, NullEventSink};
