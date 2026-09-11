//! Storage tests against a real SQLite file: what actually lands in the tables,
//! how it rolls up, and what survives a crash.

use netmeter_lib::core::config::RetentionPolicy;
use netmeter_lib::core::types::{InterfaceKind, NetworkCounters, Traffic};
use netmeter_lib::monitor::sampling::{BucketKey, OfflineWindow, PersistedBaseline};
use netmeter_lib::storage::repository::{
    FlushBatch, Repository, Resolution, SqliteRepository,
};
use std::collections::HashMap;

/// 2026-09-11T12:00:00Z
const T0: i64 = 1_789_128_000_000;
const HOUR: i64 = 3_600_000;

fn key(iface: &str, hour: i64, date: &str) -> BucketKey {
    BucketKey {
        interface: iface.into(),
        hour_start_utc_ms: hour,
        local_date: date.into(),
    }
}

fn batch(at: i64, buckets: Vec<(BucketKey, Traffic)>) -> FlushBatch {
    let interfaces: Vec<_> = buckets
        .iter()
        .map(|(k, _)| (k.interface.clone(), InterfaceKind::Wifi, None))
        .collect();
    FlushBatch {
        buckets: buckets.into_iter().collect::<HashMap<_, _>>(),
        offline: vec![],
        baselines: vec![],
        interfaces,
        at_utc_ms: at,
    }
}

fn day_total(repo: &SqliteRepository, from: &str, to: &str) -> Traffic {
    repo.usage(Resolution::Day, from, to, 10)
        .expect("query")
        .into_iter()
        .map(|r| r.traffic)
        .sum::<Traffic>()
}

#[test]
fn two_flushes_into_the_same_bucket_accumulate_rather_than_overwrite() {
    // The single most consequential detail in the schema. If the upsert
    // replaced instead of accumulating, each bucket would hold only the last
    // flush's bytes -- the daily total would collapse to near zero while the
    // kernel counters still looked right, which reads as a display bug and
    // sends you hunting in the wrong layer.
    let mut repo = SqliteRepository::in_memory().expect("db");
    let k = key("wlp7s0", T0, "2026-09-11");

    repo.flush(&batch(T0, vec![(k.clone(), Traffic::new(1_000, 100))]))
        .expect("first flush");
    repo.flush(&batch(T0, vec![(k.clone(), Traffic::new(2_500, 250))]))
        .expect("second flush");
    repo.flush(&batch(T0, vec![(k, Traffic::new(500, 50))]))
        .expect("third flush");

    assert_eq!(
        day_total(&repo, "2026-09-11", "2026-09-11"),
        Traffic::new(4_000, 400),
        "three flushes must sum, not replace"
    );
    // And the hour row agrees with the day row.
    let hours = repo
        .usage(Resolution::Hour, &T0.to_string(), &(T0 + HOUR).to_string(), 13)
        .expect("hour query");
    assert_eq!(hours.len(), 1);
    assert_eq!(hours[0].traffic, Traffic::new(4_000, 400));
}

#[test]
fn a_flush_spanning_midnight_splits_across_two_days() {
    // The accumulator is keyed at sample time, so one flush can carry buckets
    // for two calendar days. Both must land in their own day.
    let mut repo = SqliteRepository::in_memory().expect("db");
    repo.flush(&batch(
        T0,
        vec![
            (key("wlp7s0", T0 - HOUR, "2026-09-10"), Traffic::new(900, 0)),
            (key("wlp7s0", T0, "2026-09-11"), Traffic::new(100, 0)),
        ],
    ))
    .expect("flush");

    assert_eq!(
        day_total(&repo, "2026-09-10", "2026-09-10"),
        Traffic::new(900, 0),
        "bytes from before midnight belong to the previous day"
    );
    assert_eq!(
        day_total(&repo, "2026-09-11", "2026-09-11"),
        Traffic::new(100, 0)
    );
}

#[test]
fn months_and_years_roll_up_from_day_rows_with_no_extra_tables() {
    let mut repo = SqliteRepository::in_memory().expect("db");
    let days = [
        ("2025-12-31", 1_000u64),
        ("2026-01-01", 2_000),
        ("2026-01-31", 3_000),
        ("2026-02-14", 4_000),
        ("2026-12-25", 5_000),
    ];
    for (d, rx) in days {
        repo.flush(&batch(
            T0,
            vec![(key("wlp7s0", T0, d), Traffic::new(rx, 0))],
        ))
        .expect("flush");
    }

    // Month rollup: the first 7 characters of the local date.
    let jan: Traffic = repo
        .usage(Resolution::Day, "2026-01-01", "2026-01-31", 7)
        .expect("month")
        .into_iter()
        .map(|r| r.traffic)
        .sum::<Traffic>();
    assert_eq!(jan, Traffic::new(5_000, 0));

    // Year rollup: the first 4.
    let rows = repo
        .usage(Resolution::Day, "2026-01-01", "2026-12-31", 4)
        .expect("year");
    assert_eq!(rows.len(), 1, "one bucket per interface per year");
    assert_eq!(rows[0].bucket_key, "2026");
    assert_eq!(rows[0].traffic, Traffic::new(14_000, 0));

    // 2025 is excluded by the range, proving the boundary is not off by one.
    let y2025 = repo
        .usage(Resolution::Day, "2025-01-01", "2025-12-31", 4)
        .expect("year");
    assert_eq!(y2025[0].traffic, Traffic::new(1_000, 0));
}

#[test]
fn a_crash_between_flushes_loses_nothing_because_the_baseline_is_equally_stale() {
    // The crash-safety argument, executed. The counter_state row is written in
    // the same transaction as the usage, so a lost in-memory buffer is exactly
    // recoverable from the kernel's cumulative counter.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");

    {
        let (mut repo, _) = SqliteRepository::open(&db).expect("open");
        repo.flush(&FlushBatch {
            buckets: [(key("wlp7s0", T0, "2026-09-11"), Traffic::new(1_000, 0))]
                .into_iter()
                .collect(),
            offline: vec![],
            baselines: vec![PersistedBaseline {
                interface: "wlp7s0".into(),
                ifindex: 3,
                counters: NetworkCounters {
                    rx_bytes: 500_000,
                    tx_bytes: 0,
                },
                at_utc_ms: T0,
                boot_id: "boot-a".into(),
            }],
            interfaces: vec![("wlp7s0".into(), InterfaceKind::Wifi, None)],
            at_utc_ms: T0,
        })
        .expect("flush");
        // Process is killed here. Anything accumulated after this flush is
        // gone from memory.
    }

    let (repo, quarantined) = SqliteRepository::open(&db).expect("reopen after crash");
    assert!(quarantined.is_none(), "a clean file must not be quarantined");
    let baselines = repo.baselines().expect("baselines");
    assert_eq!(baselines.len(), 1);
    assert_eq!(
        baselines[0].counters.rx_bytes, 500_000,
        "the baseline that survived is the one the kernel counter is differenced against"
    );
    assert_eq!(baselines[0].boot_id, "boot-a");
    assert_eq!(
        day_total(&repo, "2026-09-11", "2026-09-11"),
        Traffic::new(1_000, 0),
        "committed usage survives"
    );
}

#[test]
fn offline_windows_are_stored_and_queried_by_overlap() {
    let mut repo = SqliteRepository::in_memory().expect("db");
    repo.flush(&FlushBatch {
        buckets: HashMap::new(),
        offline: vec![OfflineWindow {
            interface: "wlp7s0".into(),
            from_utc_ms: T0 - 3 * 24 * HOUR,
            to_utc_ms: T0,
            traffic: Traffic::new(440_857_267, 0),
        }],
        baselines: vec![],
        interfaces: vec![("wlp7s0".into(), InterfaceKind::Wifi, None)],
        at_utc_ms: T0,
    })
    .expect("flush");

    // Overlapping range finds it.
    let hit = repo.offline(T0 - HOUR, T0 + HOUR).expect("query");
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0].traffic.rx_bytes, 440_857_267);

    // Non-overlapping range does not.
    assert!(repo.offline(T0 + HOUR, T0 + 2 * HOUR).expect("query").is_empty());

    // Crucially, it is NOT in any day bucket.
    assert_eq!(
        day_total(&repo, "2026-09-01", "2026-09-30"),
        Traffic::ZERO,
        "unattributable bytes must never be reported as a day's usage"
    );
}

#[test]
fn retention_prunes_hours_relative_to_the_data_not_the_system_clock() {
    // If the cutoff came from now(), a clock stepped forward a year would
    // delete every row on the next startup -- and there is no source to
    // re-derive that history from, because the kernel keeps a cumulative
    // counter, not a history.
    let mut repo = SqliteRepository::in_memory().expect("db");
    for days_ago in [0i64, 10, 100, 500] {
        let hour = T0 - days_ago * 24 * HOUR;
        let date = chrono::DateTime::from_timestamp_millis(hour)
            .expect("valid")
            .format("%Y-%m-%d")
            .to_string();
        repo.flush(&batch(
            T0,
            vec![(key("wlp7s0", hour, &date), Traffic::new(1_000, 0))],
        ))
        .expect("flush");
    }

    let before: usize = repo
        .usage(Resolution::Hour, &i64::MIN.to_string(), &i64::MAX.to_string(), 13)
        .expect("q")
        .len();
    assert_eq!(before, 4);

    let report = repo
        .prune(&RetentionPolicy {
            hourly_days: 90,
            daily_days: 0,
            interface_days: 0,
        }, chrono_tz::UTC)
        .expect("prune");
    assert_eq!(report.hour_rows, 2, "the 100- and 500-day-old hours go");

    let after = repo
        .usage(Resolution::Hour, &i64::MIN.to_string(), &i64::MAX.to_string(), 13)
        .expect("q")
        .len();
    assert_eq!(after, 2);

    // Day rows are kept forever by default, so the history is still there.
    assert_eq!(
        day_total(&repo, "2020-01-01", "2030-01-01"),
        Traffic::new(4_000, 0),
        "pruning the hour tier must not touch the permanent day tier"
    );
}

#[test]
fn pruning_an_interface_takes_its_usage_with_it() {
    let mut repo = SqliteRepository::in_memory().expect("db");
    repo.flush(&batch(
        T0,
        vec![(key("vethdead", T0, "2026-09-11"), Traffic::new(1_000, 0))],
    ))
    .expect("flush");
    assert_eq!(repo.interfaces().expect("list").len(), 1);

    // It holds usage, so the liveness GC leaves it alone -- deleting it would
    // orphan history the user can still query.
    let r = repo
        .prune(&RetentionPolicy {
            hourly_days: 0,
            daily_days: 0,
            interface_days: 1,
        }, chrono_tz::UTC)
        .expect("prune");
    assert_eq!(r.interfaces, 0, "an interface with history is not garbage");
    assert_eq!(repo.interfaces().expect("list").len(), 1);
}

#[test]
fn an_empty_range_returns_no_rows_rather_than_a_null_row() {
    // A bare SUM() over zero matching rows returns one row containing NULL,
    // which fails the u64 decode. GROUP BY returns zero rows instead, and the
    // series builder fills the zeros.
    let repo = SqliteRepository::in_memory().expect("db");
    let rows = repo
        .usage(Resolution::Day, "1999-01-01", "1999-12-31", 10)
        .expect("must not error on an empty range");
    assert!(rows.is_empty());
    assert_eq!(day_total(&repo, "1999-01-01", "1999-12-31"), Traffic::ZERO);
}

#[test]
fn an_interface_keeps_its_identity_and_history_across_disappearances() {
    let mut repo = SqliteRepository::in_memory().expect("db");
    repo.flush(&batch(
        T0,
        vec![(key("wlp7s0", T0, "2026-09-10"), Traffic::new(1_000, 0))],
    ))
    .expect("day one");
    let id_before = repo.interfaces().expect("list")[0].id;

    // Gone for a while, then back.
    repo.flush(&batch(
        T0 + 24 * HOUR,
        vec![(key("wlp7s0", T0 + 24 * HOUR, "2026-09-11"), Traffic::new(2_000, 0))],
    ))
    .expect("day two");

    let ifaces = repo.interfaces().expect("list");
    assert_eq!(ifaces.len(), 1, "one NIC, one identity");
    assert_eq!(ifaces[0].id, id_before);
    assert_eq!(
        day_total(&repo, "2026-09-10", "2026-09-11"),
        Traffic::new(3_000, 0),
        "history spans the gap"
    );
}

#[test]
fn u64_counters_past_the_32_bit_boundary_round_trip_exactly() {
    // wlp7s0 on the development machine is already past 2^32.
    let mut repo = SqliteRepository::in_memory().expect("db");
    let big = 4_440_857_267u64;
    repo.flush(&batch(
        T0,
        vec![(key("wlp7s0", T0, "2026-09-11"), Traffic::new(big, big))],
    ))
    .expect("flush");
    assert_eq!(
        day_total(&repo, "2026-09-11", "2026-09-11"),
        Traffic::new(big, big)
    );
}

#[test]
fn meta_values_round_trip_so_the_daily_prune_remembers_when_it_ran() {
    let mut repo = SqliteRepository::in_memory().expect("db");
    assert_eq!(repo.meta("last_prune_date").expect("read"), None);
    repo.set_meta("last_prune_date", "2026-09-11").expect("write");
    assert_eq!(
        repo.meta("last_prune_date").expect("read"),
        Some("2026-09-11".into())
    );
    repo.set_meta("last_prune_date", "2026-09-12").expect("overwrite");
    assert_eq!(
        repo.meta("last_prune_date").expect("read"),
        Some("2026-09-12".into())
    );
}
