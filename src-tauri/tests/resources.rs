//! Resource-usage tests.
//!
//! NetMeter is meant to run for months on a laptop, so "cheap" is a property to
//! assert, not a claim to make in a comment. The bounds here are deliberately
//! loose -- they are there to catch a regression of an order of magnitude, not
//! to fail on a busy CI box.

use netmeter_lib::core::types::{InterfaceKind, NetworkCounters, Traffic};
use netmeter_lib::monitor::provider::NetworkStatsProvider;
use netmeter_lib::monitor::sampling::{BucketKey, PersistedBaseline};
use netmeter_lib::monitor::LinuxNetworkStatsProvider;
use netmeter_lib::storage::repository::{FlushBatch, Repository, SqliteRepository};

const T0: i64 = 1_789_128_000_000;

#[test]
fn a_day_of_idle_flushes_is_cheap_and_does_not_grow_the_database() {
    // The steady state on a machine doing nothing: the counters do not move, so
    // every flush writes only the counter_state rows that let a restart resume.
    // 2880 flushes is a full day at the 30 s default.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (mut repo, _) = SqliteRepository::open(&db).expect("open");

    let baselines: Vec<PersistedBaseline> = (0..5)
        .map(|i| PersistedBaseline {
            interface: format!("iface{i}"),
            ifindex: i + 1,
            counters: NetworkCounters {
                rx_bytes: 1_000_000,
                tx_bytes: 1_000,
            },
            at_utc_ms: T0,
            boot_id: "boot-probe".into(),
        })
        .collect();

    const FLUSHES: usize = 2_880;
    let start = std::time::Instant::now();
    for k in 0..FLUSHES {
        let mut b = baselines.clone();
        for x in b.iter_mut() {
            x.at_utc_ms = T0 + k as i64 * 30_000;
        }
        repo.flush(&FlushBatch {
            baselines: b,
            at_utc_ms: T0 + k as i64 * 30_000,
            ..Default::default()
        })
        .expect("flush");
    }
    let elapsed = start.elapsed();
    drop(repo);

    let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    println!("a day of idle flushes: {elapsed:?} total, database {size} B");

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "a day of idle flushes took {elapsed:?}; it should be well under a second of CPU"
    );
    // counter_state holds one row per interface and is replaced, never
    // appended, so an idle day must not grow the file.
    assert!(
        size < 256 * 1024,
        "an idle day grew the database to {size} B; counter_state must replace, not accumulate"
    );
}

#[test]
fn a_year_of_real_usage_stays_small() {
    // Four counted interfaces, traffic every hour, for a year. This is the
    // "retain useful statistics for years without the database becoming
    // unnecessarily large" requirement, measured.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (mut repo, _) = SqliteRepository::open(&db).expect("open");

    const HOURS: i64 = 365 * 24;
    const IFACES: usize = 4;
    let mut batch_buckets = Vec::new();
    for h in 0..HOURS {
        let hour = T0 + h * 3_600_000;
        let date = chrono::DateTime::from_timestamp_millis(hour)
            .expect("valid instant")
            .format("%Y-%m-%d")
            .to_string();
        for i in 0..IFACES {
            batch_buckets.push((
                BucketKey {
                    interface: format!("iface{i}"),
                    hour_start_utc_ms: hour,
                    local_date: date.clone(),
                },
                Traffic::new(10_000_000, 1_000_000),
            ));
        }
        // Flush in day-sized chunks, as the engine would over that day.
        if batch_buckets.len() >= 24 * IFACES {
            let interfaces: Vec<_> = (0..IFACES)
                .map(|i| (format!("iface{i}"), InterfaceKind::Wifi, None))
                .collect();
            repo.flush(&FlushBatch {
                buckets: std::mem::take(&mut batch_buckets).into_iter().collect(),
                interfaces,
                at_utc_ms: hour,
                ..Default::default()
            })
            .expect("flush");
        }
    }
    repo.compact().expect("compact");
    drop(repo);

    let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    println!(
        "a year of hourly usage on {IFACES} interfaces: {size} B ({:.1} MB)",
        size as f64 / 1e6
    );
    assert!(
        size < 8 * 1024 * 1024,
        "a year came to {size} B; a decade must stay comfortable on a laptop"
    );
}

#[test]
fn reading_the_kernel_counters_is_negligible_at_the_default_interval() {
    let p = LinuxNetworkStatsProvider::new();
    if p.counters().is_err() {
        return; // not Linux
    }
    const N: u32 = 500;
    let start = std::time::Instant::now();
    for _ in 0..N {
        let _ = p.counters().expect("read");
    }
    let per_read = start.elapsed() / N;
    // At a 5 s sampling interval this is the entire steady-state cost of
    // measurement.
    let duty = per_read.as_secs_f64() / 5.0 * 100.0;
    println!("per counter read: {per_read:?} ({duty:.4}% duty cycle at 5 s)");
    assert!(
        duty < 0.5,
        "measurement would use {duty:.3}% of a core; the polling design assumes far less"
    );
}

#[test]
fn the_wal_does_not_grow_without_bound_over_many_flushes() {
    // `journal_size_limit` is what actually truncates the WAL after a
    // checkpoint. Without it the -wal file sits at its historical high-water
    // mark for ever, which on a months-long uptime is how a 3 MB database ends
    // up beside a much larger journal.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (mut repo, _) = SqliteRepository::open(&db).expect("open");

    for k in 0..3_000i64 {
        let hour = T0 + k * 60_000;
        repo.flush(&FlushBatch {
            buckets: [(
                BucketKey {
                    interface: "wlp7s0".into(),
                    hour_start_utc_ms: hour,
                    local_date: "2026-09-11".into(),
                },
                Traffic::new(1_000, 100),
            )]
            .into_iter()
            .collect(),
            interfaces: vec![("wlp7s0".into(), InterfaceKind::Wifi, None)],
            at_utc_ms: hour,
            ..Default::default()
        })
        .expect("flush");
    }
    repo.compact().expect("compact");

    let wal = db.with_extension("db-wal");
    let wal_size = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    println!("wal after 3000 flushes: {wal_size} B");
    assert!(
        wal_size <= 8 * 1024 * 1024,
        "the WAL reached {wal_size} B, past journal_size_limit"
    );
}
