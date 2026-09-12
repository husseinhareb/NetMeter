//! The socket, end to end, without BPF or privileges: write rows as this uid,
//! serve them over a real unix socket, and read them back as a client.

use netmeter_lib::api::ipc::{read_frame, write_frame, Request, Response};
use netmeter_lib::core::types::Granularity;
use netmeterd::server::{Server, SharedStatus};
use netmeterd::store::{Batch, BucketKey, Store};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const NOW: i64 = 1_789_000_000_000;

fn me() -> u32 {
    // SAFETY: getuid cannot fail and touches no memory.
    unsafe { libc::getuid() }
}

fn status() -> SharedStatus {
    Arc::new(Mutex::new(netmeter_lib::api::ipc::DaemonStatus {
        version: "test".into(),
        probes_attached: 6,
        probes_expected: 6,
        started_at_utc_ms: NOW,
        last_flush_utc_ms: Some(NOW),
        dropped: Default::default(),
    }))
}

/// A store seeded with two applications for this uid and one for a different
/// one, plus a day of overhead.
fn seeded(dir: &TempDir) -> (PathBuf, String) {
    let db = dir.path().join("apps.db");
    let (mut store, _) = Store::open(&db).expect("open");
    let today = netmeter_lib::core::time::local_date_key(chrono_tz::UTC, NOW);

    let mut batch = Batch::default();
    for (app, rx, tx, uid) in [
        ("firefox", 5_000_000u64, 200_000u64, me()),
        ("spotify", 1_000_000, 50_000, me()),
        ("someone-elses-app", 9_000_000, 9_000_000, me() + 1),
    ] {
        batch.add(
            BucketKey {
                hour_start_utc_ms: netmeter_lib::core::time::hour_start_ms(NOW),
                local_date: today.clone(),
                uid,
                app: app.to_string(),
            },
            rx,
            tx,
        );
    }
    batch.add_overhead(today.clone(), 300_000, 150_000);
    store.flush(&batch, NOW).expect("flush");
    (db, today)
}

fn ask(socket: &PathBuf, request: &Request) -> Response {
    // The listener binds before `run` returns nothing, so retry briefly
    // rather than racing the thread's first instruction.
    for _ in 0..100 {
        if let Ok(mut s) = UnixStream::connect(socket) {
            write_frame(&mut s, request).expect("write");
            return read_frame(&mut s).expect("read");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("server never came up");
}

fn serve(db: PathBuf, socket: PathBuf) {
    let server = Server::new(socket, db, chrono_tz::UTC, status());
    std::thread::spawn(move || server.run().expect("serve"));
}

#[test]
fn a_client_gets_its_own_rows_and_not_another_uids() {
    let dir = TempDir::new().expect("dir");
    let (db, today) = seeded(&dir);
    let socket = dir.path().join("sock");
    serve(db, socket.clone());

    let response = ask(
        &socket,
        &Request::AppUsage {
            granularity: Granularity::Day,
            from: today.clone(),
            to: today.clone(),
        },
    );

    let Response::AppUsage(usage) = response else {
        panic!("expected usage, got {response:?}");
    };
    assert_eq!(usage.uid, me());
    let names: Vec<&str> = usage.total.iter().map(|a| a.app.as_str()).collect();
    assert_eq!(names, ["firefox", "spotify"], "largest first, own uid only");
    assert!(
        !names.contains(&"someone-elses-app"),
        "another uid's rows must never be served"
    );
    assert_eq!(usage.total[0].rx_bytes, 5_000_000);
    assert_eq!(usage.overhead.rx_bytes, 300_000);
    assert_eq!(usage.data_since.as_deref(), Some(today.as_str()));
}

#[test]
fn the_series_is_dense_so_an_idle_day_is_a_zero_not_a_gap() {
    let dir = TempDir::new().expect("dir");
    let (db, today) = seeded(&dir);
    let socket = dir.path().join("sock");
    serve(db, socket.clone());

    let week_ago =
        netmeter_lib::core::time::local_date_key(chrono_tz::UTC, NOW - 6 * 86_400_000);
    let response = ask(
        &socket,
        &Request::AppUsage {
            granularity: Granularity::Day,
            from: week_ago,
            to: today.clone(),
        },
    );

    let Response::AppUsage(usage) = response else {
        panic!("expected usage");
    };
    assert_eq!(usage.buckets.len(), 7, "seven days, six of them empty");
    let today_bucket = usage
        .buckets
        .iter()
        .find(|b| b.key == today)
        .expect("today present");
    assert_eq!(today_bucket.apps.len(), 2);
    assert!(usage
        .buckets
        .iter()
        .filter(|b| b.key != today)
        .all(|b| b.apps.is_empty()));
}

#[test]
fn hour_buckets_carry_no_overhead_rather_than_a_made_up_share() {
    let dir = TempDir::new().expect("dir");
    let (db, today) = seeded(&dir);
    let socket = dir.path().join("sock");
    serve(db, socket.clone());

    let response = ask(
        &socket,
        &Request::AppUsage {
            granularity: Granularity::Hour,
            from: today.clone(),
            to: today,
        },
    );
    let Response::AppUsage(usage) = response else {
        panic!("expected usage");
    };
    assert_eq!(usage.buckets.len(), 24);
    assert!(
        usage.buckets.iter().all(|b| b.overhead.rx_bytes == 0),
        "overhead is measured per day; splitting it over hours would invent one"
    );
    assert_eq!(
        usage.buckets.iter().filter(|b| !b.apps.is_empty()).count(),
        1
    );
}

#[test]
fn a_nonsense_range_is_a_bad_request_not_a_storage_error() {
    let dir = TempDir::new().expect("dir");
    let (db, _) = seeded(&dir);
    let socket = dir.path().join("sock");
    serve(db, socket.clone());

    let response = ask(
        &socket,
        &Request::AppUsage {
            granularity: Granularity::Day,
            from: "not-a-date".into(),
            to: "2026-09-12".into(),
        },
    );
    match response {
        Response::Error { kind, .. } => assert_eq!(kind, "bad_request"),
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn a_backwards_range_is_refused() {
    let dir = TempDir::new().expect("dir");
    let (db, _) = seeded(&dir);
    let socket = dir.path().join("sock");
    serve(db, socket.clone());

    let response = ask(
        &socket,
        &Request::AppUsage {
            granularity: Granularity::Day,
            from: "2026-09-12".into(),
            to: "2026-09-01".into(),
        },
    );
    match response {
        Response::Error { kind, message } => {
            assert_eq!(kind, "bad_request");
            assert!(message.contains("ends before"), "{message}");
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn status_is_served_without_touching_the_database() {
    let dir = TempDir::new().expect("dir");
    let socket = dir.path().join("sock");
    // A database path that does not exist: status must not need it.
    serve(dir.path().join("absent.db"), socket.clone());

    let response = ask(&socket, &Request::Status);
    let Response::Status(s) = response else {
        panic!("expected status, got {response:?}");
    };
    assert_eq!(s.probes_attached, 6);
    assert_eq!(s.probes_expected, 6);
}
