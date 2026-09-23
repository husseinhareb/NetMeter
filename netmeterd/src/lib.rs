//! Library face of the daemon, so its persistence layer can be tested without
//! loading BPF or needing privileges.

pub mod app;
pub mod query;
pub mod server;
pub mod store;

/// The compiled BPF program and its maps. Kept here rather than in the binary
/// so the loopback check in `examples/` loads the same object the daemon does.
pub mod skel {
    #![allow(clippy::all, dead_code, non_snake_case, non_camel_case_types)]
    include!(concat!(env!("OUT_DIR"), "/netmeterd.skel.rs"));
}
