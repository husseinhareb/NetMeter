//! The Tauri layer: the only part of the crate that knows a GUI exists.
//!
//! Compiled only with the `gui` feature, so `netmeterd` -- which links this
//! crate for `core::time` and `storage` -- does not pull Tauri in. The claim
//! that `monitor` and `storage` are GUI-free is enforced by the build rather
//! than by convention.

use crate::api::events::{names, EventSink, InterfaceChanged, LiveUsage};
use crate::core::types::MonitorStatus;
use crate::init_logging;
use crate::system::state::{AppState, DynSink};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
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

    fn quota(&self, payload: &crate::api::events::QuotaWarning) {
        self.emit(names::QUOTA_WARNING, payload);
    }
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
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // Tauri resolves these per-platform and per-identifier; they are
            // not guessed here.
            let data_dir = app.path().app_data_dir()?;
            let config_dir = app.path().app_config_dir()?;

            let level = crate::system::state::load_config(
                &config_dir.join(crate::system::paths::CONFIG_FILE),
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
            install_signal_handlers(handle.clone());
            build_tray(app.handle())?;

            // `--hidden` is what the autostart entry passes: start counting,
            // show nothing. The window is created hidden either way so there
            // is no flash before it is closed again.
            if !std::env::args().any(|a| a == "--hidden") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            crate::api::commands::get_monitor_status,
            crate::api::commands::start_monitor,
            crate::api::commands::stop_monitor,
            crate::api::commands::get_interfaces,
            crate::api::commands::get_live_rates,
            crate::api::commands::get_usage_series,
            crate::api::commands::get_usage_totals,
            crate::api::commands::get_today_usage,
            crate::api::commands::get_config,
            crate::api::commands::set_config,
            crate::api::commands::get_helper_state,
            crate::api::commands::get_app_usage,
            crate::api::commands::get_autostart,
            crate::api::commands::set_autostart,
            crate::api::commands::can_install_helper,
            crate::api::commands::install_helper,
        ])
        .on_window_event(|window, event| {
            // Hide rather than close: the engine lives in this process, and
            // the user pressing X means "get out of my way", not "stop
            // measuring". Quit is on the tray menu.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
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

/// The tray icon, its menu, and the rule that closing the window does not stop
/// monitoring.
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show NetMeter", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().cloned().ok_or_else(|| {
            tauri::Error::AssetNotFound("no window icon to use for the tray".into())
        })?)
        .tooltip("NetMeter")
        .menu(&menu)
        // The menu is the whole interface on Linux: libayatana-appindicator
        // delivers no click events to the application, so a left click that
        // does not open the menu does nothing at all. `on_tray_icon_event`
        // was dead code here and is gone rather than left to mislead.
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => reveal(app),
            // The only way out. Closing the window hides it instead, because a
            // meter that stops when its window closes misses the day.
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn reveal(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
