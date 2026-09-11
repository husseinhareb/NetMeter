//! Request and response types for the Tauri commands.
//!
//! Two wire conventions, applied everywhere:
//!
//! * **Calendar keys are zero-padded local strings** -- `"2026"`, `"2026-09"`,
//!   `"2026-09-11"`, `"2026-09-11T14"` -- and ranges are **inclusive on both
//!   ends**. Strings rather than instants because a day-granularity query over
//!   an instant is ambiguous on the DST fall-back hour, where the same instant
//!   maps to two local wall times.
//! * **Every instant is `i64` epoch milliseconds UTC**, named with a
//!   `_utc_ms` suffix so it cannot be confused with a calendar key.
//!
//! Byte counts are `u64` and are never `Option`: an empty period is zeros.
//! Rates are `Option<f64>` and *are* nullable, because "no rate yet" is a real
//! state distinct from a measured zero.

use crate::core::types::{
    Granularity, InterfaceKind, MonitorStatus, Traffic, UsageSeries,
};
use serde::{Deserialize, Serialize};

/// Which interfaces a query should total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Apply the configured interface policy. This is the user's usage total.
    #[default]
    Included,
    /// Every recorded interface. Larger than reality by construction -- see
    /// `docs/ACCOUNTING.md` -- so it is a diagnostic view, not a total.
    All,
}

/// A usage query.
///
/// One request type covers today, yesterday, last 7 days, last 30 days, a
/// month, a year, an arbitrary range and the per-interface breakdown. Six
/// separately named endpoints would hard-code the UI's vocabulary into the Rust
/// API, so "last 90 days" would need a backend change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageQuery {
    pub granularity: Granularity,
    /// First bucket, inclusive. Must match `granularity`.
    pub from: String,
    /// Last bucket, inclusive.
    pub to: String,
    #[serde(default)]
    pub scope: Scope,
    /// Include the per-interface split inside every bucket.
    #[serde(default)]
    pub include_breakdown: bool,
}

/// An interface as the UI sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceInfo {
    pub id: Option<i64>,
    pub name: String,
    pub kind: InterfaceKind,
    pub mac: Option<String>,
    /// Whether the kernel currently lists this interface. `false` means it is
    /// historical -- an unplugged dongle, a removed container -- so the UI can
    /// grey it rather than implying it is live.
    pub present: bool,
    /// Whether its bytes are part of the usage total under the current policy.
    pub included: bool,
    pub first_seen_utc_ms: Option<i64>,
    pub last_seen_utc_ms: Option<i64>,
}

/// The live readout.
pub type LiveRates = crate::api::events::LiveUsage;

/// What `set_config` returns, so the UI never has to call back to find out
/// what actually took effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigApplied {
    pub config: crate::core::config::Config,
    /// The interfaces the new policy counts, resolved server-side. Keeps glob
    /// matching in exactly one place.
    pub included_interfaces: Vec<String>,
}

/// Aliases so the command signatures read plainly.
pub type UsageResponse = UsageSeries;
pub type StatusResponse = MonitorStatus;

/// Totals for a period, without the per-bucket series. The shape behind the
/// "Today" and "This month" tiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub key_from: String,
    pub key_to: String,
    pub included: Traffic,
    pub observed: Traffic,
    pub offline: Traffic,
    pub includes_pending: bool,
}

impl From<&UsageSeries> for UsageTotals {
    fn from(s: &UsageSeries) -> Self {
        Self {
            key_from: s.buckets.first().map(|b| b.key.clone()).unwrap_or_default(),
            key_to: s.buckets.last().map(|b| b.key.clone()).unwrap_or_default(),
            included: s.total.included,
            observed: s.total.observed,
            offline: s.offline_total.included,
            includes_pending: s.includes_pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usage_query_round_trips_through_json_with_snake_case_names() {
        let q = UsageQuery {
            granularity: Granularity::Day,
            from: "2026-09-05".into(),
            to: "2026-09-11".into(),
            scope: Scope::Included,
            include_breakdown: true,
        };
        let s = serde_json::to_string(&q).expect("serializes");
        assert!(s.contains("\"granularity\":\"day\""));
        assert!(s.contains("\"scope\":\"included\""));
        assert_eq!(serde_json::from_str::<UsageQuery>(&s).expect("parses"), q);
    }

    #[test]
    fn scope_and_breakdown_default_so_the_frontend_can_omit_them() {
        let q: UsageQuery = serde_json::from_str(
            r#"{"granularity":"month","from":"2026-01","to":"2026-09"}"#,
        )
        .expect("parses without the optional fields");
        assert_eq!(q.scope, Scope::Included);
        assert!(!q.include_breakdown);
    }

    #[test]
    fn byte_counts_serialize_as_numbers_not_strings() {
        // u64 stays exact below 2^53, which is ~9 petabytes -- far beyond any
        // real usage -- so JSON numbers are safe and strings are unnecessary.
        let t = Traffic::new(4_440_857_267, 1_037_860_334);
        let v = serde_json::to_value(t).expect("serializes");
        assert_eq!(v["rx_bytes"], 4_440_857_267u64);
        assert!(v["rx_bytes"].is_number());
    }
}
