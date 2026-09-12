//! What the persistence layer has to get right: accumulate rather than
//! replace, keep a day's total intact when its tail is collapsed, and prune
//! the tier that grows without touching the one that does not.

use netmeterd::store::{Batch, BucketKey, Retention, Store};
use tempfile::TempDir;

const DAY_MS: i64 = 86_400_000;
const NOW: i64 = 1_789_000_000_000;

fn day_key(offset_days: i64) -> String {
    netmeter_lib::core::time::local_date_key(chrono_tz::UTC, NOW + offset_days * DAY_MS)
}

fn store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let (s, quarantined) = Store::open(&dir.path().join("apps.db")).expect("open");
    assert!(quarantined.is_none());
    (dir, s)
}

fn key(hour_ms: i64, date: &str, app: &str) -> BucketKey {
    BucketKey {
        hour_start_utc_ms: hour_ms,
        local_date: date.to_string(),
        uid: 1000,
        app: app.to_string(),
    }
}

fn total_for(s: &Store, date: &str, app: &str) -> (u64, u64) {
    s.day_totals(date)
        .expect("totals")
        .into_iter()
        .find(|(a, _, _)| a == app)
        .map(|(_, rx, tx)| (rx, tx))
        .unwrap_or((0, 0))
}

#[test]
fn two_flushes_in_one_hour_add_up_instead_of_replacing() {
    let (_d, mut s) = store();
    let mut b = Batch::default();
    b.add(key(0, "2026-09-12", "firefox"), 1_000, 100);
    s.flush(&b, 0).expect("first");

    let mut b = Batch::default();
    b.add(key(0, "2026-09-12", "firefox"), 500, 50);
    s.flush(&b, 0).expect("second");

    assert_eq!(total_for(&s, "2026-09-12", "firefox"), (1_500, 150));
    assert_eq!(s.hourly_rows().expect("rows"), 1, "one hour, one row");
}

#[test]
fn an_empty_batch_writes_nothing() {
    let (_d, mut s) = store();
    s.flush(&Batch::default(), 0).expect("flush");
    assert_eq!(s.hourly_rows().expect("rows"), 0);
    assert!(s.day_totals("2026-09-12").expect("totals").is_empty());
}

#[test]
fn overhead_is_kept_per_day_and_accumulates() {
    let (_d, mut s) = store();
    let mut b = Batch::default();
    b.add_overhead("2026-09-12".into(), 700, 400);
    b.add_overhead("2026-09-13".into(), 10, 20);
    s.flush(&b, 0).expect("flush");

    let mut b = Batch::default();
    b.add_overhead("2026-09-12".into(), 300, 100);
    s.flush(&b, 0).expect("flush");

    assert_eq!(s.overhead_for("2026-09-12").expect("o"), (1_000, 500));
    assert_eq!(s.overhead_for("2026-09-13").expect("o"), (10, 20));
    assert_eq!(s.overhead_for("2026-01-01").expect("o"), (0, 0));
}

#[test]
fn collapsing_a_days_tail_preserves_the_days_total() {
    let (_d, mut s) = store();
    let yesterday = day_key(-1);
    let mut b = Batch::default();
    // 30 apps, descending, so the top 20 are obvious and the tail is not.
    let mut expected_rx = 0u64;
    for i in 0..30u64 {
        let rx = (30 - i) * 1_000;
        expected_rx += rx;
        b.add(key(0, &yesterday, &format!("app{i:02}")), rx, 0);
    }
    s.flush(&b, NOW).expect("flush");
    assert_eq!(s.day_totals(&yesterday).expect("t").len(), 30);

    s.prune(Retention::default(), chrono_tz::UTC, NOW)
        .expect("prune");

    let rows = s.day_totals(&yesterday).expect("t");
    assert_eq!(rows.len(), 21, "top 20 plus one other row");
    assert!(rows.iter().any(|(a, _, _)| a == "other"));

    let kept: u64 = rows.iter().map(|(_, rx, _)| rx).sum();
    assert_eq!(kept, expected_rx, "collapsing must not lose bytes");
}

#[test]
fn collapsing_twice_does_not_double_count() {
    let (_d, mut s) = store();
    let yesterday = day_key(-1);
    let mut b = Batch::default();
    for i in 0..25u64 {
        b.add(key(0, &yesterday, &format!("app{i:02}")), (25 - i) * 100, 0);
    }
    s.flush(&b, NOW).expect("flush");

    s.prune(Retention::default(), chrono_tz::UTC, NOW)
        .expect("first");
    let after_one = s.day_totals(&yesterday).expect("t");
    s.prune(Retention::default(), chrono_tz::UTC, NOW)
        .expect("second");
    let after_two = s.day_totals(&yesterday).expect("t");

    assert_eq!(after_one, after_two, "prune must be idempotent");
}

#[test]
fn today_is_never_collapsed_while_it_is_still_being_written() {
    let (_d, mut s) = store();
    let now = NOW;
    let today = day_key(0);

    let mut b = Batch::default();
    for i in 0..25u64 {
        b.add(key(0, &today, &format!("app{i:02}")), (25 - i) * 100, 0);
    }
    s.flush(&b, now).expect("flush");
    s.prune(Retention::default(), chrono_tz::UTC, now)
        .expect("prune");

    assert_eq!(
        s.day_totals(&today).expect("t").len(),
        25,
        "a day still in progress keeps every application"
    );
}

#[test]
fn retention_drops_hourly_rows_and_keeps_daily_ones() {
    let (_d, mut s) = store();
    let now = NOW;
    let old_ms = now - 500 * DAY_MS;
    let old_date = netmeter_lib::core::time::local_date_key(chrono_tz::UTC, old_ms);

    let mut b = Batch::default();
    b.add(key(old_ms, &old_date, "firefox"), 4_000, 400);
    b.add(key(now, &netmeter_lib::core::time::local_date_key(chrono_tz::UTC, now), "firefox"), 1, 1);
    s.flush(&b, now).expect("flush");
    assert_eq!(s.hourly_rows().expect("rows"), 2);

    s.prune(Retention::default(), chrono_tz::UTC, now)
        .expect("prune");

    assert_eq!(s.hourly_rows().expect("rows"), 1, "the 500-day-old hour goes");
    assert_eq!(
        total_for(&s, &old_date, "firefox"),
        (4_000, 400),
        "its day row stays: daily_days is 0, meaning keep forever"
    );
}

#[test]
fn an_application_with_no_history_left_is_forgotten() {
    let (_d, mut s) = store();
    let now = NOW;
    let old_ms = now - 500 * DAY_MS;
    let old_date = netmeter_lib::core::time::local_date_key(chrono_tz::UTC, old_ms);

    let mut b = Batch::default();
    b.add(key(old_ms, &old_date, "one-shot"), 10, 10);
    s.flush(&b, now).expect("flush");

    // Daily rows expire too, so nothing references the app afterwards.
    s.prune(
        Retention {
            hourly_days: 30,
            daily_days: 30,
        },
        chrono_tz::UTC,
        now,
    )
    .expect("prune");

    assert!(s.day_totals(&old_date).expect("t").is_empty());
    assert_eq!(s.hourly_rows().expect("rows"), 0);
}
