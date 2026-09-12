//! Library face of the daemon, so its persistence layer can be tested without
//! loading BPF or needing privileges.

pub mod app;
pub mod store;
