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

use api::events::{names, EventSink, InterfaceChanged, LiveUsage};
use core::types::MonitorStatus;
use system::state::{AppState, DynSink};
use tauri::{Emitter, Manager};

/// Publishes engine events to the webview.
///
/// The only Tauri-aware implementation of [`EventSink`]. Emission failures are
/// logged and swallowed: a closed window (the app minimised to a tray, or a
/// webview mid-reload) must never stop monitoring.
struct TauriEventSink {
    app: tauri::AppHandle,
}

impl TauriEventSink {
    fn emit<T: serde::Serialize + Clone>(&self, event: &str, payload: &T) {
        if let Err(e) = self.app.emit(event, payload.clone()) {
            tracing::debug!(event, error = %e, "event not delivered");
        }
    }
}

impl EventSink for TauriEventSink {
    fn usage(&self, p: &LiveUsage) {
        self.emit(names::USAGE_UPDATED, p);
    }
    fn interface_added(&self, p: &InterfaceChanged) {
        self.emit(names::INTERFACE_ADDED, p);
    }
    fn interface_removed(&self, p: &InterfaceChanged) {
        self.emit(names::INTERFACE_REMOVED, p);
    }
    fn status(&self, p: &MonitorStatus) {
        self.emit(names::MONITOR_STATUS_CHANGED, p);
    }
}

/// Install the tracing subscriber.
///
/// Quiet by default -- startup, shutdown, interface changes, counter resets and
/// failures, but nothing per tick. `RUST_LOG` overrides it for troubleshooting.
fn init_logging(level: &str) {
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

/// Flush and stop the monitor on process signals.
///
/// Tauri's `RunEvent::Exit` covers a window close, but not the case that
/// matters most: `SIGTERM` at logout or reboot, which is exactly when the
/// unflushed traffic is largest (a sync or an update finishing).
fn install_signal_handlers(app: tauri::AppHandle) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TERMINATING: AtomicBool = AtomicBool::new(false);

    extern "C" fn handler(_sig: libc::c_int) {
        // Async-signal-safe: a single atomic store, nothing else.
        TERMINATING.store(true, Ordering::SeqCst);
    }

    // SAFETY: `handler` does nothing but store to an atomic, which is
    // async-signal-safe.
    let h = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
        // Without this, writing to a closed pipe would kill the process.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    std::thread::Builder::new()
        .name("netmeter-signals".into())
        .spawn(move || loop {
            if TERMINATING.load(Ordering::SeqCst) {
                tracing::info!("termination signal received; flushing");
                if let Some(state) = app.try_state::<AppState>() {
                    state.shutdown();
                }
                app.exit(0);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        })
        .ok();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // Tauri resolves these per-platform and per-identifier; they are
            // not guessed here.
            let data_dir = app.path().app_data_dir()?;
            let config_dir = app.path().app_config_dir()?;

            let level = system::state::load_config(
                &config_dir.join(system::paths::CONFIG_FILE),
            )
            .map(|c| c.logging_level)
            .unwrap_or_else(|_| "info".into());
            init_logging(&level);
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                data = %data_dir.display(),
                "NetMeter starting"
            );

            let state = AppState::new(
                &data_dir,
                &config_dir,
                DynSink::new(TauriEventSink {
                    app: handle.clone(),
                }),
            )?;

            // Monitoring starts with the app: a usage meter that only counts
            // while its window is open is not a usage meter.
            if let Err(e) = state.start_monitor() {
                tracing::error!(error = %e, "monitor did not start");
            }

            app.manage(state);
            install_signal_handlers(handle);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            api::commands::get_monitor_status,
            api::commands::start_monitor,
            api::commands::stop_monitor,
            api::commands::get_interfaces,
            api::commands::get_live_rates,
            api::commands::get_usage_series,
            api::commands::get_usage_totals,
            api::commands::get_today_usage,
            api::commands::get_config,
            api::commands::set_config,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the NetMeter application")
        .run(|app, event| {
            // Flush on the way out, synchronously. A detached flush would race
            // the process exiting.
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<AppState>() {
                    state.shutdown();
                }
                tracing::info!("NetMeter stopped");
            }
        });
}
