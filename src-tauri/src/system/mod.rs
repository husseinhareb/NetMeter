//! Process-level concerns: clocks, paths, locking, and the service lifecycle.
pub mod lifecycle;
pub mod autostart;
pub mod paths;
pub mod power;
pub mod state;

pub use lifecycle::MonitorService;
pub use paths::InstanceLock;
pub use state::AppState;
