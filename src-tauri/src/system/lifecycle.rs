//! Starting and stopping the monitoring service.
//!
//! The GUI drives this, but nothing here depends on the GUI: the engine is
//! started with a provider, a repository and an event sink, none of which know
//! Tauri exists. That is the whole of the work needed to host the same service
//! in a `netmeterd` process later.

use crate::api::events::EventSink;
use crate::core::config::Config;
use crate::core::errors::{MonitorError, Result};
use crate::monitor::engine::{Engine, EngineConfig, EngineShared};
use crate::monitor::LinuxNetworkStatsProvider;
use crate::storage::repository::SqliteRepository;
use std::path::Path;
use std::sync::Arc;

/// A monitoring service that can be started and stopped repeatedly.
pub struct MonitorService<S: EventSink> {
    engine: Option<Engine>,
    /// Kept after a stop so status and buffered totals remain readable.
    shared: Option<Arc<EngineShared>>,
    sink: Arc<S>,
}

impl<S: EventSink> MonitorService<S> {
    pub fn new(sink: Arc<S>) -> Self {
        Self {
            engine: None,
            shared: None,
            sink,
        }
    }

    pub fn is_running(&self) -> bool {
        self.engine.is_some()
    }

    pub fn shared(&self) -> Option<Arc<EngineShared>> {
        self.shared.clone()
    }

    /// Start sampling.
    ///
    /// Idempotent rather than an error on a second call: the frontend
    /// re-invoking on a window reload is ordinary, and spawning a second
    /// sampler would credit every kernel byte twice.
    pub fn start(&mut self, db_path: &Path, config: &Config) -> Result<()> {
        if self.engine.is_some() {
            return Ok(());
        }
        let (repository, quarantined) = SqliteRepository::open(db_path)?;
        if let Some(backup) = quarantined {
            tracing::error!(
                backup = %backup.display(),
                "previous database was unusable; history has been reset"
            );
        }

        let engine = Engine::start(
            LinuxNetworkStatsProvider::new(),
            repository,
            Arc::clone(&self.sink),
            EngineConfig::from(config),
            crate::system::power::boot_id(),
        );
        self.shared = Some(engine.shared());
        self.engine = Some(engine);
        tracing::info!(
            database = %db_path.display(),
            interval_s = config.sampling_interval_seconds,
            "monitor started"
        );
        Ok(())
    }

    /// Stop sampling, flushing what is buffered.
    ///
    /// Blocks until the engine's own thread has written the buffer, because a
    /// flush from this thread would be a second writer against a connection it
    /// does not own.
    pub fn stop(&mut self) {
        if let Some(engine) = self.engine.take() {
            engine.stop();
            tracing::info!("monitor stopped");
        }
    }

    /// Apply a new configuration to a running engine.
    pub fn reconfigure(&self, config: &Config) -> std::result::Result<(), MonitorError> {
        match &self.engine {
            Some(e) => {
                e.reconfigure(EngineConfig::from(config));
                Ok(())
            }
            None => Err(MonitorError::NotRunning),
        }
    }

    /// Ask for an immediate flush without stopping.
    pub fn request_flush(&self) {
        if let Some(e) = &self.engine {
            e.request_flush();
        }
    }
}

impl<S: EventSink> Drop for MonitorService<S> {
    fn drop(&mut self) {
        self.stop();
    }
}
