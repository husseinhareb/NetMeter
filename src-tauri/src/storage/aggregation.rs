//! Turning stored rows into the dense series the UI renders.
//!
//! Kept apart from both the SQL and the Tauri layer because it is pure: given
//! rows, a calendar and a policy it produces a series, deterministically.

use crate::core::config::ResolvedPolicy;
use crate::core::time;
use crate::core::types::{
    Granularity, InterfaceTraffic, OfflineWindowSummary, Traffic, UsageBucket, UsagePeriod,
    UsageSeries, UsageSummary,
};
use super::repository::{OfflineRow, UsageRow};
use std::collections::{BTreeMap, HashMap};

/// Everything needed to shape a series.
pub struct SeriesInput<'a> {
    pub granularity: Granularity,
    pub timezone: chrono_tz::Tz,
    /// The period of the first requested bucket.
    pub from: UsagePeriod,
    /// The period of the last requested bucket, inclusive.
    pub to: UsagePeriod,
    pub rows: Vec<UsageRow>,
    pub offline: Vec<OfflineRow>,
    pub policy: &'a ResolvedPolicy,
    pub include_breakdown: bool,
    pub data_since: Option<String>,
    /// Traffic still in memory, keyed the same way the rows are.
    pub pending: Vec<UsageRow>,
    pub generated_at_utc_ms: i64,
}

/// Build a dense series.
///
/// Dense means every bucket in `[from, to]` is present, zero-filled. A missing
/// bucket would force the frontend to reconstruct the local calendar -- with
/// DST -- in JavaScript, which is exactly the work the backend exists to own.
pub fn build_series(input: SeriesInput<'_>) -> UsageSeries {
    let SeriesInput {
        granularity,
        timezone,
        from,
        to,
        rows,
        offline,
        policy,
        include_breakdown,
        data_since,
        pending,
        generated_at_utc_ms,
    } = input;

    // Fold stored and pending rows together, so the current bucket is not
    // frozen between flushes and then jumps.
    //
    // Keyed by interface NAME, not by row id: an unflushed row has no database
    // id yet, so keying by id would collapse every pending interface into one
    // entry -- summing a counted NIC with excluded loopback -- and would fail
    // to merge a pending row with the stored row for the same interface.
    let pending_keys: std::collections::HashSet<String> =
        pending.iter().map(|r| r.bucket_key.clone()).collect();
    let mut by_bucket: HashMap<String, BTreeMap<String, InterfaceTraffic>> = HashMap::new();
    for row in rows.into_iter().chain(pending) {
        let included = policy.counts(&row.interface_name, row.kind);
        let slot = by_bucket
            .entry(row.bucket_key.clone())
            .or_default()
            .entry(row.interface_name.clone())
            .or_insert_with(|| InterfaceTraffic {
                interface_id: row.interface_id,
                name: row.interface_name.clone(),
                kind: row.kind,
                included,
                traffic: Traffic::ZERO,
            });
        slot.traffic += row.traffic;
        // Prefer a real id over the placeholder an unflushed row carries, so a
        // breakdown entry can still be correlated with `get_interfaces`.
        if slot.interface_id < 0 && row.interface_id >= 0 {
            slot.interface_id = row.interface_id;
        }
        // Recomputed rather than trusted from the first row seen.
        slot.included = included;
    }

    // True only if a pending bucket actually falls inside the requested range.
    // Reporting it for a query about last March would be a lie the UI would
    // render as "these numbers are still moving".
    let mut includes_pending = false;

    let mut included_interfaces: Vec<String> = Vec::new();
    let mut total = UsageSummary::ZERO;
    let mut buckets = Vec::new();

    for (key, period) in time::enumerate_keys(timezone, granularity, &from, &to) {
        if pending_keys.contains(&key) {
            includes_pending = true;
        }
        let mut summary = UsageSummary::ZERO;
        let mut by_interface: Vec<InterfaceTraffic> = by_bucket
            .remove(&key)
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        by_interface.sort_by_key(|i| std::cmp::Reverse(i.traffic.total_bytes()));

        for it in &by_interface {
            summary.observed += it.traffic;
            if it.included {
                summary.included += it.traffic;
                if !included_interfaces.contains(&it.name) {
                    included_interfaces.push(it.name.clone());
                }
            }
        }
        total.included += summary.included;
        total.observed += summary.observed;

        buckets.push(UsageBucket {
            key,
            start_utc_ms: period.start_utc_ms,
            end_utc_ms: period.end_utc_ms,
            summary,
            by_interface: if include_breakdown {
                by_interface
            } else {
                Vec::new()
            },
        });
    }

    // Offline windows are clipped to the queried range but not redistributed:
    // the fraction of a window that overlaps the range is reported as such, and
    // the window's own bounds travel with it so the UI can say what it covers.
    let range = UsagePeriod::new(from.start_utc_ms, to.end_utc_ms);
    let mut offline_total = UsageSummary::ZERO;
    let mut windows: BTreeMap<(i64, i64), UsageSummary> = BTreeMap::new();
    for row in offline {
        let window = UsagePeriod::new(row.from_utc_ms, row.to_utc_ms);
        let overlap = window.overlap_ms(&range);
        if overlap == 0 {
            continue;
        }
        // Proportional clipping is the only defensible split of a window that
        // straddles the range edge; it is reported as offline either way, so it
        // never masquerades as measured usage.
        let share = |b: u64| -> u64 {
            let d = window.duration_ms();
            if d <= 0 {
                b
            } else {
                ((b as u128 * overlap as u128) / d as u128) as u64
            }
        };
        let clipped = Traffic::new(share(row.traffic.rx_bytes), share(row.traffic.tx_bytes));
        let entry = windows
            .entry((row.from_utc_ms, row.to_utc_ms))
            .or_insert(UsageSummary::ZERO);
        entry.observed += clipped;
        offline_total.observed += clipped;
        if policy.counts(&row.interface_name, row.kind) {
            entry.included += clipped;
            offline_total.included += clipped;
        }
    }

    included_interfaces.sort();

    UsageSeries {
        granularity,
        timezone: timezone.name().to_string(),
        buckets,
        total,
        offline_total,
        offline_windows: windows
            .into_iter()
            .map(|((f, t), summary)| OfflineWindowSummary {
                from_utc_ms: f,
                to_utc_ms: t,
                summary,
            })
            .collect(),
        included_interfaces,
        data_since,
        includes_pending,
        generated_at_utc_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{Config, InterfacePolicy};
    use crate::core::types::InterfaceKind;

    fn tz() -> chrono_tz::Tz {
        "Europe/Paris".parse().expect("tz")
    }

    fn row(key: &str, id: i64, name: &str, kind: InterfaceKind, rx: u64, tx: u64) -> UsageRow {
        UsageRow {
            bucket_key: key.into(),
            interface_id: id,
            interface_name: name.into(),
            kind,
            traffic: Traffic::new(rx, tx),
        }
    }

    fn input<'a>(
        policy: &'a ResolvedPolicy,
        from: &str,
        to: &str,
        rows: Vec<UsageRow>,
    ) -> SeriesInput<'a> {
        SeriesInput {
            granularity: Granularity::Day,
            timezone: tz(),
            from: time::period_for_key(tz(), from).expect("from key"),
            to: time::period_for_key(tz(), to).expect("to key"),
            rows,
            offline: Vec::new(),
            policy,
            include_breakdown: true,
            data_since: None,
            pending: Vec::new(),
            generated_at_utc_ms: 0,
        }
    }

    #[test]
    fn a_week_with_gaps_still_returns_seven_buckets() {
        let p = Config::default().resolve();
        let s = build_series(input(
            &p,
            "2026-09-07",
            "2026-09-13",
            vec![
                row("2026-09-07", 1, "wlp7s0", InterfaceKind::Wifi, 1_000, 100),
                row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 5_000, 500),
            ],
        ));
        assert_eq!(s.buckets.len(), 7, "the machine being off is a zero, not a gap");
        assert_eq!(s.buckets[0].summary.included, Traffic::new(1_000, 100));
        assert_eq!(s.buckets[1].summary.included, Traffic::ZERO);
        assert_eq!(s.buckets[4].summary.included, Traffic::new(5_000, 500));
        assert_eq!(s.total.included, Traffic::new(6_000, 600));
        // Keys are contiguous and ordered.
        let keys: Vec<_> = s.buckets.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "2026-09-07", "2026-09-08", "2026-09-09", "2026-09-10", "2026-09-11",
                "2026-09-12", "2026-09-13"
            ]
        );
    }

    #[test]
    fn the_billable_total_excludes_what_is_already_counted_elsewhere() {
        // The exact shape of this machine: a VPN and a container bridge whose
        // bytes also traverse the WiFi NIC.
        let p = Config::default().resolve();
        let s = build_series(input(
            &p,
            "2026-09-11",
            "2026-09-11",
            vec![
                row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 1_000_000, 200_000),
                row("2026-09-11", 2, "tailscale0", InterfaceKind::Vpn, 300_000, 50_000),
                row("2026-09-11", 3, "docker0", InterfaceKind::Bridge, 90_000, 40_000),
                row("2026-09-11", 4, "lo", InterfaceKind::Loopback, 9_999_999, 9_999_999),
            ],
        ));
        assert_eq!(
            s.total.included,
            Traffic::new(1_000_000, 200_000),
            "only the physical NIC counts"
        );
        assert_eq!(
            s.total.observed,
            Traffic::new(11_389_999, 10_289_999),
            "everything observed is still reported, as a diagnostic"
        );
        assert_eq!(s.included_interfaces, ["wlp7s0"]);
        // The breakdown says which is which, so the UI never has to guess.
        let b = &s.buckets[0].by_interface;
        assert!(b.iter().find(|i| i.name == "wlp7s0").expect("wifi").included);
        assert!(!b.iter().find(|i| i.name == "lo").expect("lo").included);
    }

    #[test]
    fn changing_the_policy_reinterprets_history_without_rewriting_it() {
        // Per-interface storage is what makes this possible: the same rows
        // answer both questions.
        let rows = vec![
            row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 1_000_000, 0),
            row("2026-09-11", 2, "wg0", InterfaceKind::Vpn, 800_000, 0),
        ];
        let default = Config::default().resolve();
        let s = build_series(input(&default, "2026-09-11", "2026-09-11", rows.clone()));
        assert_eq!(s.total.included, Traffic::new(1_000_000, 0));

        let vpn_only = Config {
            interface_policy: InterfacePolicy::Manual,
            included_interfaces: vec!["wg0".into()],
            excluded_interfaces: vec![],
            ..Config::default()
        }
        .resolve();
        let s2 = build_series(input(&vpn_only, "2026-09-11", "2026-09-11", rows));
        assert_eq!(
            s2.total.included,
            Traffic::new(800_000, 0),
            "the same stored rows, counted the user's way"
        );
    }

    #[test]
    fn an_empty_period_is_zeros_and_never_nulls() {
        let p = Config::default().resolve();
        let s = build_series(input(&p, "2026-09-11", "2026-09-11", vec![]));
        assert_eq!(s.buckets.len(), 1);
        assert_eq!(s.buckets[0].summary, UsageSummary::ZERO);
        assert_eq!(s.total, UsageSummary::ZERO);
        assert!(s.included_interfaces.is_empty());
        // And it serializes as numbers, not nulls.
        let j = serde_json::to_value(&s).expect("serializes");
        assert_eq!(j["total"]["included"]["rx_bytes"], 0);
    }

    #[test]
    fn a_dst_day_carries_its_real_length() {
        let p = Config::default().resolve();
        let mut i = input(&p, "2026-10-25", "2026-10-25", vec![]);
        i.granularity = Granularity::Day;
        let s = build_series(i);
        let b = &s.buckets[0];
        assert_eq!(
            b.end_utc_ms - b.start_utc_ms,
            25 * 3_600_000,
            "the chart must be able to see that this day was 25 hours"
        );
    }

    #[test]
    fn months_and_years_roll_up_from_the_same_rows() {
        let p = Config::default().resolve();
        let rows = vec![
            row("2026-08", 1, "wlp7s0", InterfaceKind::Wifi, 1_000, 0),
            row("2026-09", 1, "wlp7s0", InterfaceKind::Wifi, 2_000, 0),
        ];
        let mut i = input(&p, "2026-08", "2026-09", rows);
        i.granularity = Granularity::Month;
        i.from = time::period_for_key(tz(), "2026-08").expect("k");
        i.to = time::period_for_key(tz(), "2026-09").expect("k");
        let s = build_series(i);
        assert_eq!(s.buckets.len(), 2);
        assert_eq!(s.buckets[0].key, "2026-08");
        assert_eq!(s.total.included, Traffic::new(3_000, 0));
    }

    #[test]
    fn pending_rows_for_different_interfaces_do_not_collide() {
        // Unflushed rows carry no database id. Keying the per-bucket map by id
        // would merge every pending interface into one entry -- so a counted
        // NIC and excluded loopback would be summed together and the day total
        // would come out nondeterministically zero or inflated.
        let p = Config::default().resolve();
        let mut i = input(&p, "2026-09-11", "2026-09-11", vec![]);
        i.pending = vec![
            UsageRow {
                bucket_key: "2026-09-11".into(),
                interface_id: -1,
                interface_name: "wlp7s0".into(),
                kind: InterfaceKind::Wifi,
                traffic: Traffic::new(1_000, 0),
            },
            UsageRow {
                bucket_key: "2026-09-11".into(),
                interface_id: -1,
                interface_name: "lo".into(),
                kind: InterfaceKind::Loopback,
                traffic: Traffic::new(9_000_000, 0),
            },
        ];
        let s = build_series(i);
        assert_eq!(
            s.buckets[0].by_interface.len(),
            2,
            "two interfaces must stay two entries"
        );
        assert_eq!(
            s.total.included,
            Traffic::new(1_000, 0),
            "loopback must not be summed into the usage total"
        );
        assert_eq!(s.total.observed, Traffic::new(9_001_000, 0));
    }

    #[test]
    fn a_pending_row_merges_with_the_stored_row_for_the_same_interface() {
        let p = Config::default().resolve();
        let mut i = input(
            &p,
            "2026-09-11",
            "2026-09-11",
            vec![row("2026-09-11", 7, "wlp7s0", InterfaceKind::Wifi, 1_000, 0)],
        );
        // Same interface, but not yet flushed, so it has no id.
        i.pending = vec![UsageRow {
            bucket_key: "2026-09-11".into(),
            interface_id: -1,
            interface_name: "wlp7s0".into(),
            kind: InterfaceKind::Wifi,
            traffic: Traffic::new(250, 0),
        }];
        let s = build_series(i);
        assert_eq!(
            s.buckets[0].by_interface.len(),
            1,
            "one interface must not appear twice in its own breakdown"
        );
        assert_eq!(s.buckets[0].by_interface[0].interface_id, 7, "keeps the real id");
        assert_eq!(s.total.included, Traffic::new(1_250, 0));
    }

    #[test]
    fn pending_bytes_are_folded_into_the_current_bucket() {
        // Without this, "Today" would freeze between flushes and then jump.
        let p = Config::default().resolve();
        let mut i = input(
            &p,
            "2026-09-11",
            "2026-09-11",
            vec![row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 1_000, 0)],
        );
        i.pending = vec![row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 250, 0)];
        let s = build_series(i);
        assert!(s.includes_pending);
        assert_eq!(s.total.included, Traffic::new(1_250, 0));
        assert_eq!(
            s.buckets[0].by_interface.len(),
            1,
            "the same interface must not appear twice"
        );
    }

    #[test]
    fn offline_traffic_is_reported_separately_and_never_folded_into_a_day() {
        let p = Config::default().resolve();
        let from = time::period_for_key(tz(), "2026-09-09").expect("k");
        let to = time::period_for_key(tz(), "2026-09-11").expect("k");
        let mut i = input(&p, "2026-09-09", "2026-09-11", vec![]);
        i.offline = vec![OfflineRow {
            interface_id: 1,
            interface_name: "wlp7s0".into(),
            kind: InterfaceKind::Wifi,
            from_utc_ms: from.start_utc_ms,
            to_utc_ms: to.end_utc_ms,
            traffic: Traffic::new(440_857_267, 0),
        }];
        let s = build_series(i);
        assert_eq!(
            s.total.included,
            Traffic::ZERO,
            "unattributable bytes must not become a day's total"
        );
        assert_eq!(s.offline_total.included, Traffic::new(440_857_267, 0));
        assert_eq!(s.offline_windows.len(), 1);
        assert_eq!(s.offline_windows[0].from_utc_ms, from.start_utc_ms);
    }

    #[test]
    fn an_offline_window_straddling_the_range_edge_is_clipped_proportionally() {
        let p = Config::default().resolve();
        let from = time::period_for_key(tz(), "2026-09-11").expect("k");
        let mut i = input(&p, "2026-09-11", "2026-09-11", vec![]);
        // A window covering two days, only one of which is queried.
        i.offline = vec![OfflineRow {
            interface_id: 1,
            interface_name: "wlp7s0".into(),
            kind: InterfaceKind::Wifi,
            from_utc_ms: from.start_utc_ms - 24 * 3_600_000,
            to_utc_ms: from.end_utc_ms,
            traffic: Traffic::new(1_000, 0),
        }];
        let s = build_series(i);
        assert_eq!(
            s.offline_total.included,
            Traffic::new(500, 0),
            "half the window overlaps, so half the bytes are attributed to it"
        );
    }

    #[test]
    fn an_offline_window_outside_the_range_is_ignored() {
        let p = Config::default().resolve();
        let mut i = input(&p, "2026-09-11", "2026-09-11", vec![]);
        i.offline = vec![OfflineRow {
            interface_id: 1,
            interface_name: "wlp7s0".into(),
            kind: InterfaceKind::Wifi,
            from_utc_ms: 0,
            to_utc_ms: 1_000,
            traffic: Traffic::new(1_000, 0),
        }];
        let s = build_series(i);
        assert!(s.offline_windows.is_empty());
        assert_eq!(s.offline_total, UsageSummary::ZERO);
    }

    #[test]
    fn includes_pending_is_false_when_the_buffered_bytes_fall_outside_the_range() {
        // Otherwise a query about last March would claim its numbers are still
        // moving, which the UI would render as a live tile.
        let p = Config::default().resolve();
        let mut i = input(&p, "2026-03-01", "2026-03-31", vec![]);
        i.pending = vec![row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 1_000, 0)];
        let s = build_series(i);
        assert!(!s.includes_pending, "March is settled history");
        assert_eq!(s.total, UsageSummary::ZERO);
    }

    #[test]
    fn the_breakdown_is_omitted_when_not_requested() {
        let p = Config::default().resolve();
        let mut i = input(
            &p,
            "2026-09-11",
            "2026-09-11",
            vec![row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 1, 1)],
        );
        i.include_breakdown = false;
        let s = build_series(i);
        assert!(s.buckets[0].by_interface.is_empty());
        // ...but the totals are still right.
        assert_eq!(s.total.included, Traffic::new(1, 1));
    }

    #[test]
    fn an_empty_include_set_yields_zeros_rather_than_an_error() {
        let cfg = Config {
            interface_policy: InterfacePolicy::Manual,
            included_interfaces: vec![],
            ..Config::default()
        };
        let p = cfg.resolve();
        let s = build_series(input(
            &p,
            "2026-09-11",
            "2026-09-11",
            vec![row("2026-09-11", 1, "wlp7s0", InterfaceKind::Wifi, 1_000, 0)],
        ));
        assert_eq!(s.total.included, Traffic::ZERO);
        assert_eq!(s.total.observed, Traffic::new(1_000, 0));
        assert!(s.included_interfaces.is_empty());
    }

    #[test]
    fn interfaces_within_a_bucket_are_ordered_by_size() {
        let p = Config {
            interface_policy: InterfacePolicy::AllExcept,
            excluded_interfaces: vec![],
            ..Config::default()
        }
        .resolve();
        let s = build_series(input(
            &p,
            "2026-09-11",
            "2026-09-11",
            vec![
                row("2026-09-11", 1, "small", InterfaceKind::Wifi, 10, 0),
                row("2026-09-11", 2, "big", InterfaceKind::Wifi, 10_000, 0),
                row("2026-09-11", 3, "mid", InterfaceKind::Wifi, 500, 0),
            ],
        ));
        let names: Vec<_> = s.buckets[0]
            .by_interface
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(names, ["big", "mid", "small"]);
    }
}
