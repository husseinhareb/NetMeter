//! Outbound notifications, behind a trait.
//!
//! This is the seam that keeps the monitor free of Tauri. The engine publishes
//! through [`EventSink`]; today the only implementation forwards to the
//! webview, and a future `netmeterd` would implement the same trait over a Unix
//! socket without the engine noticing.

use crate::core::types::{DataRate, MonitorStatus, Traffic, UsageSummary};
use serde::{Deserialize, Serialize};

/// Event names, as the frontend subscribes to them.
pub mod names {
    pub const USAGE_UPDATED: &str = "network-usage-updated";
    pub const INTERFACE_ADDED: &str = "interface-added";
    pub const INTERFACE_REMOVED: &str = "interface-removed";
    pub const MONITOR_STATUS_CHANGED: &str = "monitor-status-changed";
    pub const QUOTA_WARNING: &str = "quota-warning";
}

/// Live rates for one interface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceRate {
    pub name: String,
    pub rate: DataRate,
    /// Bytes moved during the sampling interval this rate was measured over.
    pub traffic: Traffic,
    pub included: bool,
}

/// The payload of `network-usage-updated`.
///
/// Deliberately the same shape the `get_live_rates` command returns, so the
/// frontend has one parser and one code path whether it polls or subscribes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveUsage {
    pub sampled_at_utc_ms: i64,
    /// The measured interval. `None` when no rate could be computed.
    pub interval_ms: Option<u64>,
    /// Rate across the interfaces the policy counts.
    pub total: DataRate,
    pub by_interface: Vec<InterfaceRate>,
    /// Today's traffic that is still only in memory, not yet flushed to the
    /// database. It resets at every flush, so it is *not* the day's total --
    /// ask `get_today_usage` for that, which adds this in already.
    pub pending_today: UsageSummary,
    /// The clocks disagreed on this sample: the byte counts are good, the
    /// timing is not.
    pub time_anomaly: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceChanged {
    pub name: String,
    pub kind: crate::core::types::InterfaceKind,
    pub included: bool,
}

/// A monthly allowance has reached one of its warning thresholds.
///
/// Emitted once per threshold per calendar month, remembered in the database
/// so a restart does not warn again about a month it already warned about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaWarning {
    /// `"2026-09"`.
    pub month: String,
    /// The threshold crossed, e.g. 80.
    pub percent: u8,
    pub used_bytes: u64,
    pub limit_bytes: u64,
}

/// Where the engine publishes.
pub trait EventSink: Send + Sync + 'static {
    fn usage(&self, payload: &LiveUsage);
    fn interface_added(&self, payload: &InterfaceChanged);
    fn interface_removed(&self, payload: &InterfaceChanged);
    fn status(&self, payload: &MonitorStatus);
    fn quota(&self, payload: &QuotaWarning);
}

/// Discards everything. Used when no GUI is attached -- which is exactly the
/// situation a headless `netmeterd` would be in.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullEventSink;

impl EventSink for NullEventSink {
    fn usage(&self, _: &LiveUsage) {}
    fn interface_added(&self, _: &InterfaceChanged) {}
    fn interface_removed(&self, _: &InterfaceChanged) {}
    fn status(&self, _: &MonitorStatus) {}
    fn quota(&self, _: &QuotaWarning) {}
}

/// Records what the engine published, for assertions.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default, Clone)]
pub struct RecordingSink {
    pub usage: std::sync::Arc<std::sync::Mutex<Vec<LiveUsage>>>,
    pub added: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub removed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub status: std::sync::Arc<std::sync::Mutex<Vec<MonitorStatus>>>,
    pub quota: std::sync::Arc<std::sync::Mutex<Vec<QuotaWarning>>>,
}

#[cfg(any(test, feature = "test-support"))]
impl EventSink for RecordingSink {
    fn usage(&self, p: &LiveUsage) {
        self.usage.lock().expect("lock").push(p.clone());
    }
    fn interface_added(&self, p: &InterfaceChanged) {
        self.added.lock().expect("lock").push(p.name.clone());
    }
    fn interface_removed(&self, p: &InterfaceChanged) {
        self.removed.lock().expect("lock").push(p.name.clone());
    }
    fn status(&self, p: &MonitorStatus) {
        self.status.lock().expect("lock").push(p.clone());
    }
    fn quota(&self, p: &QuotaWarning) {
        self.quota.lock().expect("lock").push(p.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_null_sink_swallows_everything_without_panicking() {
        let s = NullEventSink;
        s.usage(&LiveUsage {
            sampled_at_utc_ms: 0,
            interval_ms: None,
            total: DataRate::UNKNOWN,
            by_interface: vec![],
            pending_today: UsageSummary::ZERO,
            time_anomaly: false,
        });
        s.interface_added(&InterfaceChanged {
            name: "wlan0".into(),
            kind: crate::core::types::InterfaceKind::Wifi,
            included: true,
        });
    }

    #[test]
    fn live_usage_round_trips_through_json() {
        let p = LiveUsage {
            sampled_at_utc_ms: 1_789_128_000_000,
            interval_ms: Some(5_000),
            total: DataRate {
                rx_bytes_per_sec: Some(1_024.0),
                tx_bytes_per_sec: None,
            },
            by_interface: vec![InterfaceRate {
                name: "wlp7s0".into(),
                rate: DataRate::UNKNOWN,
                traffic: Traffic::new(10, 20),
                included: true,
            }],
            pending_today: UsageSummary::ZERO,
            time_anomaly: false,
        };
        let s = serde_json::to_string(&p).expect("serializes");
        // An unknown rate must be an explicit null, distinguishable from 0.
        assert!(s.contains("\"tx_bytes_per_sec\":null"));
        assert_eq!(serde_json::from_str::<LiveUsage>(&s).expect("parses"), p);
    }
}
