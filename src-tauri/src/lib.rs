//! NetMeter: a lightweight network usage monitor for Linux.
//!
//! The backend is one Rust crate inside one Tauri project, organised as a
//! one-way dependency chain:
//!
//! ```text
//!   measurement  ->  processing  ->  persistence  ->  API  ->  visualization
//!    monitor/         core/           storage/        api/       (frontend)
//! ```
//!
//! * [`core`] holds the domain types and the pure logic -- delta arithmetic,
//!   counter-reset rules, calendar boundaries. It knows nothing of Tauri,
//!   SQLite or Linux.
//! * [`monitor`] reads the kernel's own byte counters. No shell commands, no
//!   packet capture, no raw sockets, no privileges.
//! * [`storage`] is the only module containing SQL.
//! * [`api`] is the only module that knows a GUI exists.
//! * [`system`] holds process-level concerns: clocks, paths, the
//!   single-instance lock, and the service lifecycle.
//!
//! Nothing in `monitor` or `storage` references Tauri, and the engine
//! communicates outward only through [`api::events::EventSink`]. Moving the
//! monitor into a standalone `netmeterd` is therefore a matter of adding a
//! second binary that constructs the same [`monitor::Engine`] with a different
//! sink -- not a rewrite.

pub mod api;
pub mod core;
pub mod monitor;
pub mod storage;
pub mod system;

#[cfg(feature = "gui")]
pub mod gui;

/// Install the tracing subscriber.
///
/// Quiet by default -- startup, shutdown, interface changes, counter resets and
/// failures, but nothing per tick. `RUST_LOG` overrides it for troubleshooting.
pub fn init_logging(level: &str) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(format!("netmeter={level},netmeter_lib={level},warn")))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` rather than `init`: a second call (a test harness, a restart)
    // must not panic.
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .with(filter)
        .try_init();
}
