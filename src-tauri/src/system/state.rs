//! The state Tauri manages, and the only place the layers are wired together.

use crate::api::events::{EventSink, LiveUsage};
use crate::api::models::ConfigApplied;
use crate::core::config::Config;
use crate::core::errors::{ConfigError, Error, Result};
use crate::core::types::{DataRate, MonitorState, MonitorStatus, Traffic, UsageSummary};
use crate::system::lifecycle::MonitorService;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// Locks a mutex, recovering from poisoning.
///
/// Every value guarded in this file is plain data, so a previous holder's panic
/// leaves it coherent. Propagating the poison instead would brick every command
/// for the life of the process.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Everything the commands need.
pub struct AppState {
    config: RwLock<Arc<Config>>,
    config_path: PathBuf,
    db_path: PathBuf,
    service: Mutex<MonitorService<DynSink>>,
    /// Held for the process lifetime. Dropping it releases the single-instance
    /// lock, so it lives here rather than in a local.
    _instance_lock: Option<crate::system::paths::InstanceLock>,
}

/// Type-erased sink so `AppState` is not generic over the GUI.
pub struct DynSink(Box<dyn EventSink>);

impl DynSink {
    pub fn new<S: EventSink>(sink: S) -> Self {
        Self(Box::new(sink))
    }
}

impl EventSink for DynSink {
    fn usage(&self, p: &LiveUsage) {
        self.0.usage(p)
    }
    fn interface_added(&self, p: &crate::api::events::InterfaceChanged) {
        self.0.interface_added(p)
    }
    fn interface_removed(&self, p: &crate::api::events::InterfaceChanged) {
        self.0.interface_removed(p)
    }
    fn status(&self, p: &MonitorStatus) {
        self.0.status(p)
    }
}

impl AppState {
    /// Wire everything together.
    ///
    /// `data_dir` holds the database and the lock; `config_dir` holds the
    /// config file. A failure to take the instance lock is not fatal -- the app
    /// still opens and can show history -- but the monitor is refused, because
    /// two samplers on one database double every recorded byte.
    pub fn new(data_dir: &Path, config_dir: &Path, sink: DynSink) -> Result<Self> {
        crate::system::paths::ensure_dir(data_dir)?;
        crate::system::paths::ensure_dir(config_dir)?;

        let config_path = config_dir.join(crate::system::paths::CONFIG_FILE);
        let config = load_config(&config_path).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "using default configuration");
            Config::default()
        });

        let db_path = config
            .database_path
            .clone()
            .unwrap_or_else(|| data_dir.join(crate::system::paths::DATABASE_FILE));

        let instance_lock = match crate::system::paths::InstanceLock::acquire(data_dir) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::error!(error = %e, "another instance owns the database; monitoring disabled");
                None
            }
        };

        Ok(Self {
            config: RwLock::new(Arc::new(config)),
            config_path,
            db_path,
            service: Mutex::new(MonitorService::new(Arc::new(sink))),
            _instance_lock: instance_lock,
        })
    }

    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|e| Arc::clone(&e.into_inner()))
    }

    pub fn db_path(&self) -> PathBuf {
        self.db_path.clone()
    }

    pub fn engine_shared(&self) -> Option<Arc<crate::monitor::engine::EngineShared>> {
        lock(&self.service).shared()
    }

    pub fn start_monitor(&self) -> Result<()> {
        if self._instance_lock.is_none() {
            return Err(Error::Monitor(
                crate::core::errors::MonitorError::AlreadyLocked,
            ));
        }
        let config = self.config();
        lock(&self.service).start(&self.db_path, &config)
    }

    pub fn stop_monitor(&self) {
        lock(&self.service).stop();
    }

    /// Flush now and wait for the engine thread to finish. Called on exit.
    pub fn shutdown(&self) {
        lock(&self.service).stop();
    }

    pub fn status(&self) -> MonitorStatus {
        match self.engine_shared() {
            Some(s) => s.status(),
            None => MonitorStatus {
                state: MonitorState::Stopped,
                started_at_utc_ms: None,
                last_tick_utc_ms: None,
                last_flush_utc_ms: None,
                sampling_interval_seconds: self.config().sampling_interval_seconds,
                interfaces_observed: 0,
                interfaces_included: 0,
                restarts: 0,
                parse_errors: 0,
                pending: Traffic::ZERO,
                degraded_reason: self._instance_lock.is_none().then(|| {
                    "another NetMeter instance owns the database".to_string()
                }),
            },
        }
    }

    /// The most recent live sample, or an empty reading if none has been taken.
    pub fn live_rates(&self) -> LiveUsage {
        self.engine_shared()
            .and_then(|s| lock(&s.live).clone())
            .unwrap_or(LiveUsage {
                sampled_at_utc_ms: crate::core::time::now_utc_ms(),
                interval_ms: None,
                total: DataRate::UNKNOWN,
                by_interface: Vec::new(),
                today: UsageSummary::ZERO,
                time_anomaly: false,
            })
    }

    /// Interfaces the engine has seen this session, with their metadata.
    ///
    /// These are the ones a fresh NIC appears in before the first flush has
    /// written it to the database, so `get_interfaces` can list it immediately.
    pub fn live_interfaces(&self) -> HashMap<String, crate::monitor::engine::SeenInterface> {
        self.engine_shared()
            .map(|s| lock(&s.pending).seen.clone())
            .unwrap_or_default()
    }

    /// Validate, persist and apply a new configuration.
    pub fn set_config(&self, config: Config) -> Result<ConfigApplied> {
        config.validate()?;
        save_config(&self.config_path, &config)?;

        let policy = config.resolve();
        let included: Vec<String> = self
            .live_interfaces()
            .into_iter()
            .filter(|(n, i)| policy.counts(n, i.kind))
            .map(|(n, _)| n)
            .collect();

        {
            let service = lock(&self.service);
            if service.is_running() {
                let _ = service.reconfigure(&config);
            }
        }

        let applied = ConfigApplied {
            config: config.clone(),
            included_interfaces: {
                let mut v = included;
                v.sort();
                v
            },
        };
        if let Ok(mut g) = self.config.write() {
            *g = Arc::new(config);
        }
        Ok(applied)
    }
}

/// Read the config file, falling back to defaults when it does not exist.
pub fn load_config(path: &Path) -> std::result::Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let config: Config = serde_json::from_str(&text)?;
    config.validate()?;
    Ok(config)
}

/// Write the config file atomically: a crash mid-write must not leave a
/// truncated file that fails to parse on the next start.
pub fn save_config(path: &Path, config: &Config) -> std::result::Result<(), ConfigError> {
    let text = serde_json::to_string_pretty(config)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|source| ConfigError::Write {
        path: tmp.display().to_string(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_file_yields_defaults_rather_than_an_error() {
        let d = tempfile::tempdir().expect("tempdir");
        let c = load_config(&d.path().join("nope.json")).expect("defaults");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn config_survives_a_save_load_round_trip() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("config.json");
        let c = Config {
            sampling_interval_seconds: 10,
            excluded_interfaces: vec!["lo".into(), "veth*".into()],
            ..Config::default()
        };
        save_config(&p, &c).expect("save");
        assert_eq!(load_config(&p).expect("load"), c);
        // Written atomically, so no stray temp file is left behind.
        assert!(!p.with_extension("json.tmp").exists());
    }

    #[test]
    fn an_invalid_stored_config_is_rejected_rather_than_applied() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("config.json");
        std::fs::write(&p, r#"{"sampling_interval_seconds": 0}"#).expect("write");
        assert!(
            load_config(&p).is_err(),
            "a zero interval would spin the sampling loop"
        );
    }

    #[test]
    fn a_corrupt_config_file_is_reported_not_silently_ignored() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("config.json");
        std::fs::write(&p, "{ not json").expect("write");
        assert!(matches!(load_config(&p), Err(ConfigError::Parse(_))));
    }
}
