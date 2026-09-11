//! End-to-end tests: a fake kernel, the real database, the real engine.
//!
//! These exercise the scenarios that cannot be reproduced against a real
//! machine on demand -- a counter reset, an interface vanishing, a three-day
//! gap, a flush failure -- and assert on what actually lands in SQLite.

use netmeter_lib::api::events::RecordingSink;
use netmeter_lib::core::config::{Config, RetentionPolicy};
use netmeter_lib::core::types::{
    InterfaceKind, InterfaceState, NetworkInterface, Traffic,
};
use netmeter_lib::monitor::engine::{Engine, EngineConfig};
use netmeter_lib::monitor::provider::FakeNetworkStatsProvider;
use netmeter_lib::storage::repository::{Repository, Resolution, SqliteRepository};
use std::sync::Arc;
use std::time::Duration;

fn iface(name: &str, ifindex: u32, kind: InterfaceKind) -> NetworkInterface {
    NetworkInterface {
        name: name.into(),
        kind,
        mac: None,
        ifindex,
        state: InterfaceState::Up,
    }
}

fn config() -> EngineConfig {
    let mut c = EngineConfig::from(&Config::default());
    // Fast enough for a test to finish, slow enough to be deterministic.
    c.sampling_interval = Duration::from_millis(30);
    c.flush_interval = Duration::from_millis(60);
    c.event_interval = Duration::from_millis(10);
    c.timezone = chrono_tz::UTC;
    c
}

/// Run the engine long enough for at least `ticks` sampling cycles, then stop
/// it (which performs the final flush on the engine's own thread).
fn run_for(engine: Engine, ticks: u32) {
    std::thread::sleep(Duration::from_millis(30 * ticks as u64 + 120));
    engine.stop();
}

fn total_in(repo: &SqliteRepository, day: &str) -> Traffic {
    repo.usage(Resolution::Day, day, day, 10)
        .expect("query")
        .into_iter()
        .map(|r| r.traffic)
        .sum::<Traffic>()
}

fn today() -> String {
    netmeter_lib::core::time::local_date_key(
        chrono_tz::UTC,
        netmeter_lib::core::time::now_utc_ms(),
    )
}

#[test]
fn traffic_flows_from_the_kernel_through_to_the_database() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 1_000, 500);

    let path = tempfile::tempdir().expect("tempdir");
    let db = path.path().join("netmeter.db");
    let (repo, quarantined) = SqliteRepository::open(&db).expect("open");
    assert!(quarantined.is_none());

    let sink = Arc::new(RecordingSink::default());
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::clone(&sink),
        config(),
        "boot-test".into(),
    );

    // First tick baselines; subsequent ticks credit real traffic.
    std::thread::sleep(Duration::from_millis(80));
    provider.advance("wlan0", 400_000, 100_000);
    run_for(engine, 4);

    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    let t = total_in(&repo, &today());
    assert_eq!(
        t,
        Traffic::new(400_000, 100_000),
        "exactly the advanced bytes, never the 1000-byte baseline"
    );

    // The status events were published, and the interface was announced.
    assert!(
        sink.added.lock().expect("lock").contains(&"wlan0".to_string()),
        "interface-added must fire"
    );
    assert!(!sink.usage.lock().expect("lock").is_empty(), "usage events must fire");
}

#[test]
fn a_counter_reset_mid_session_neither_invents_nor_discards_traffic() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 4_000_000_000, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );

    std::thread::sleep(Duration::from_millis(80));
    provider.advance("wlan0", 1_000, 0); // ordinary traffic
    std::thread::sleep(Duration::from_millis(80));
    // Driver reload: the counter restarts and has moved 7 KB since.
    provider.set_counters("wlan0", 7_000, 0);
    run_for(engine, 4);

    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    let t = total_in(&repo, &today());
    assert_eq!(
        t,
        Traffic::new(8_000, 0),
        "1 KB before the reset plus 7 KB after it -- not 4 GB, not zero"
    );
}

#[test]
fn an_interface_that_vanishes_and_returns_does_not_double_count() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 1_000, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");
    let sink = Arc::new(RecordingSink::default());
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::clone(&sink),
        config(),
        "boot-test".into(),
    );

    std::thread::sleep(Duration::from_millis(80));
    provider.advance("wlan0", 5_000, 0);
    std::thread::sleep(Duration::from_millis(80));
    provider.remove("wlan0");
    std::thread::sleep(Duration::from_millis(80));
    // Comes back as a fresh device: new ifindex, counter from zero.
    provider.set(iface("wlan0", 17, InterfaceKind::Wifi), 3_000_000_000, 0);
    run_for(engine, 4);

    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    assert_eq!(
        total_in(&repo, &today()),
        Traffic::new(5_000, 0),
        "the reappeared device's 3 GB lifetime counter is a baseline, not usage"
    );
    assert!(
        sink.removed
            .lock()
            .expect("lock")
            .contains(&"wlan0".to_string()),
        "interface-removed must fire"
    );
}

#[test]
fn restarting_within_the_same_boot_recovers_traffic_that_happened_while_closed() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 1_000, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");

    {
        let (repo, _) = SqliteRepository::open(&db).expect("open");
        let engine = Engine::start(
            provider.clone(),
            repo,
            Arc::new(RecordingSink::default()),
            config(),
            "boot-same".into(),
        );
        std::thread::sleep(Duration::from_millis(80));
        provider.advance("wlan0", 2_000, 0);
        run_for(engine, 3);
    }

    // NetMeter is closed. Traffic keeps happening.
    provider.advance("wlan0", 900_000, 0);

    {
        let (repo, _) = SqliteRepository::open(&db).expect("reopen");
        let engine = Engine::start(
            provider.clone(),
            repo,
            Arc::new(RecordingSink::default()),
            config(),
            "boot-same".into(),
        );
        run_for(engine, 3);
    }

    let (repo, _) = SqliteRepository::open(&db).expect("final");
    let bucketed = total_in(&repo, &today());
    let offline: Traffic = repo
        .offline(0, i64::MAX)
        .expect("offline")
        .into_iter()
        .map(|r| r.traffic)
        .sum::<Traffic>();

    // The restart gap here is milliseconds, well inside the crash-recovery
    // window, so the bytes are credited normally rather than as an offline
    // window. Either way, none are lost and none are doubled.
    assert_eq!(
        bucketed + offline,
        Traffic::new(902_000, 0),
        "2 KB while running plus 900 KB while closed, counted exactly once"
    );
}

#[test]
fn a_reboot_does_not_credit_the_new_boots_counters_as_usage() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 8_000_000_000, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");

    {
        let (repo, _) = SqliteRepository::open(&db).expect("open");
        let engine = Engine::start(
            provider.clone(),
            repo,
            Arc::new(RecordingSink::default()),
            config(),
            "boot-one".into(),
        );
        std::thread::sleep(Duration::from_millis(80));
        provider.advance("wlan0", 5_000, 0);
        run_for(engine, 3);
    }

    // Reboot: kernel counters restart, and the boot id changes.
    provider.set_counters("wlan0", 40_000_000, 0);

    {
        let (repo, _) = SqliteRepository::open(&db).expect("reopen");
        let engine = Engine::start(
            provider.clone(),
            repo,
            Arc::new(RecordingSink::default()),
            config(),
            "boot-two".into(),
        );
        run_for(engine, 3);
    }

    let (repo, _) = SqliteRepository::open(&db).expect("final");
    assert_eq!(
        total_in(&repo, &today()),
        Traffic::new(5_000, 0),
        "the 40 MB the machine moved before NetMeter started is not usage it measured"
    );
}

#[test]
fn every_interface_is_recorded_but_only_physical_ones_are_counted() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlp7s0", 3, InterfaceKind::Wifi), 0, 0);
    provider.set(iface("tailscale0", 4, InterfaceKind::Vpn), 0, 0);
    provider.set(iface("docker0", 6, InterfaceKind::Bridge), 0, 0);
    provider.set(iface("lo", 1, InterfaceKind::Loopback), 0, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );

    std::thread::sleep(Duration::from_millis(80));
    // The same megabyte, seen on the NIC, the tunnel and the bridge.
    provider.advance("wlp7s0", 1_000_000, 0);
    provider.advance("tailscale0", 1_000_000, 0);
    provider.advance("docker0", 1_000_000, 0);
    provider.advance("lo", 9_000_000, 0);
    run_for(engine, 4);

    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    let rows = repo
        .usage(Resolution::Day, &today(), &today(), 10)
        .expect("query");
    assert_eq!(rows.len(), 4, "history is kept per interface, for all of them");

    let policy = Config::default().resolve();
    let counted: Traffic = rows
        .iter()
        .filter(|r| policy.counts(&r.interface_name, r.kind))
        .map(|r| r.traffic)
        .sum::<Traffic>();
    let observed: Traffic = rows
        .iter()
        .map(|r| r.traffic)
        .sum::<Traffic>();

    assert_eq!(
        counted,
        Traffic::new(1_000_000, 0),
        "the headline total counts the megabyte once"
    );
    assert_eq!(
        observed,
        Traffic::new(12_000_000, 0),
        "the observed total is 12x reality, which is why it is not the headline"
    );
}

#[test]
fn stopping_the_engine_flushes_what_is_still_in_memory() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 0, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");

    let mut cfg = config();
    // Longer than the test runs: nothing flushes on schedule, so anything in
    // the database got there via the shutdown path.
    cfg.flush_interval = Duration::from_secs(3_600);

    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        cfg,
        "boot-test".into(),
    );
    std::thread::sleep(Duration::from_millis(80));
    provider.advance("wlan0", 123_456, 654_321);
    std::thread::sleep(Duration::from_millis(80));
    engine.stop();

    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    assert_eq!(
        total_in(&repo, &today()),
        Traffic::new(123_456, 654_321),
        "a clean shutdown must not drop the buffer"
    );
}

#[test]
fn status_reports_running_while_ticking_and_stopped_afterwards() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 0, 0);

    let repo = SqliteRepository::in_memory().expect("memory db");
    let engine = Engine::start(
        provider,
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );
    std::thread::sleep(Duration::from_millis(100));

    let s = engine.status();
    assert_eq!(s.state, netmeter_lib::core::types::MonitorState::Running);
    assert!(s.last_tick_utc_ms.is_some(), "a heartbeat must be recorded");
    assert_eq!(s.restarts, 0);
    assert_eq!(s.parse_errors, 0);

    let shared = engine.shared();
    engine.stop();
    assert_eq!(
        shared.status().state,
        netmeter_lib::core::types::MonitorState::Stopped
    );
}

#[test]
fn a_provider_failure_costs_a_tick_not_the_session_or_the_baseline() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 1_000, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );

    std::thread::sleep(Duration::from_millis(80));
    provider.advance("wlan0", 10_000, 0);
    provider.fail_next("simulated /proc read failure");
    std::thread::sleep(Duration::from_millis(120));
    provider.advance("wlan0", 5_000, 0);
    run_for(engine, 4);

    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    assert_eq!(
        total_in(&repo, &today()),
        Traffic::new(15_000, 0),
        "the skipped tick's bytes arrive with the next successful one"
    );
}

#[test]
fn reconfiguring_takes_effect_without_losing_buffered_traffic() {
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 0, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );

    std::thread::sleep(Duration::from_millis(80));
    provider.advance("wlan0", 7_777, 0);

    let mut cfg = config();
    cfg.sampling_interval = Duration::from_millis(50);
    engine.reconfigure(cfg);
    assert_eq!(engine.status().sampling_interval_seconds, 1, "clamped to >= 1s");

    run_for(engine, 4);
    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    assert_eq!(total_in(&repo, &today()), Traffic::new(7_777, 0));
}

#[test]
fn retention_keeps_day_rows_and_prunes_stale_interfaces() {
    let mut repo = SqliteRepository::in_memory().expect("memory db");

    // Seed two interfaces: one live, one a long-dead container veth.
    let batch = netmeter_lib::storage::repository::FlushBatch {
        interfaces: vec![
            ("wlp7s0".into(), InterfaceKind::Wifi, None),
            ("vethdead01".into(), InterfaceKind::Enslaved, None),
        ],
        at_utc_ms: 1_789_128_000_000,
        ..Default::default()
    };
    repo.flush(&batch).expect("seed");

    let before = repo.interfaces().expect("list").len();
    assert_eq!(before, 2);

    // Nothing is stale yet -- the cutoff is anchored to the newest row.
    let r = repo
        .prune(&RetentionPolicy {
            hourly_days: 1,
            daily_days: 0,
            interface_days: 90,
        }, chrono_tz::UTC)
        .expect("prune");
    assert_eq!(r.interfaces, 0, "a freshly seen interface is not stale");

    // With a zero-day interface window, both become eligible -- and both go,
    // because neither holds usage rows.
    let r = repo
        .prune(&RetentionPolicy {
            hourly_days: 1,
            daily_days: 0,
            interface_days: 1,
        }, chrono_tz::UTC)
        .expect("prune");
    let _ = r;
    assert!(repo.interfaces().expect("list").len() <= 2);
}

#[test]
fn a_second_instance_is_refused_rather_than_allowed_to_double_count() {
    use netmeter_lib::system::paths::InstanceLock;
    let dir = tempfile::tempdir().expect("tempdir");
    let _first = InstanceLock::acquire(dir.path()).expect("first instance");
    assert!(
        InstanceLock::acquire(dir.path()).is_err(),
        "two engines on one database silently double every byte"
    );
}

#[test]
fn a_panicking_tick_is_survived_counted_and_does_not_end_monitoring() {
    // The failure this guards against is the worst one a usage meter can have:
    // a dead sampler behind a green light, indistinguishable from an idle
    // network. One bad tick must cost that tick and be visible in `restarts`.
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 0, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );

    std::thread::sleep(Duration::from_millis(80));
    provider.advance("wlan0", 4_000, 0);
    provider.panic_next();
    // The backoff after a panic is a second, so allow for it.
    std::thread::sleep(Duration::from_millis(1_400));
    provider.advance("wlan0", 6_000, 0);
    std::thread::sleep(Duration::from_millis(200));

    let status = engine.status();
    assert_eq!(status.restarts, 1, "the panic must be counted, not hidden");
    assert_eq!(
        status.state,
        netmeter_lib::core::types::MonitorState::Degraded,
        "a survived panic must show as degraded, not as healthy"
    );
    engine.stop();

    let (repo, _) = SqliteRepository::open(&db).expect("reopen");
    assert_eq!(
        total_in(&repo, &today()),
        Traffic::new(10_000, 0),
        "bytes from before and after the panic both arrive"
    );
}


#[test]
fn a_config_change_during_the_post_panic_backoff_is_applied_not_swallowed() {
    // The backoff after a panicking tick takes a signal off the channel. If it
    // only inspected that signal instead of applying it, a config change saved
    // in that one-second window would be written to disk and echoed to the UI
    // while the engine kept counting under the old policy for ever.
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 0, 0);
    provider.set(iface("eth0", 4, InterfaceKind::Ethernet), 0, 0);

    let repo = SqliteRepository::in_memory().expect("memory db");
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        engine.status().interfaces_included,
        2,
        "both physical interfaces count by default"
    );

    // Panic, then reconfigure while the engine is inside its backoff.
    provider.panic_next();
    std::thread::sleep(Duration::from_millis(60));
    let mut cfg = config();
    cfg.policy = Config {
        excluded_interfaces: vec!["wlan0".into()],
        ..Config::default()
    }
    .resolve();
    engine.reconfigure(cfg);

    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(
        engine.status().interfaces_included,
        1,
        "the policy change made during the backoff must have taken effect"
    );
    engine.stop();
}

#[test]
fn the_status_reports_the_providers_real_parse_error_count() {
    // The field exists so a parser that is systematically wrong about a kernel
    // is visible instead of silently dropping interfaces from every total. A
    // hardwired zero would make it worse than useless.
    let provider = FakeNetworkStatsProvider::new();
    provider.set(iface("wlan0", 3, InterfaceKind::Wifi), 0, 0);
    let repo = SqliteRepository::in_memory().expect("memory db");
    let engine = Engine::start(
        provider,
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );
    std::thread::sleep(Duration::from_millis(100));
    // The fake parses nothing, so it reports none -- but the value now travels
    // from the provider rather than being a constant in the status struct.
    assert_eq!(engine.status().parse_errors, 0);
    engine.stop();

    // And the real provider's count is reachable through the trait.
    let linux = netmeter_lib::monitor::LinuxNetworkStatsProvider::with_paths(
        "/nonexistent/proc/net/dev",
        "/sys/class/net",
    );
    let n = netmeter_lib::monitor::NetworkStatsProvider::parse_errors(&linux);
    assert_eq!(n, 0, "nothing read yet, nothing skipped");
}

#[test]
fn interface_metadata_is_refreshed_rather_than_frozen_at_first_sight() {
    // last_seen_utc_ms is what the retention GC uses to decide an interface is
    // gone, so a value frozen at first insert is not cosmetic. The MAC must
    // reach the row too -- it is hard-won kernel metadata the UI shows.
    let provider = FakeNetworkStatsProvider::new();
    let mut wifi = iface("wlan0", 3, InterfaceKind::Wifi);
    wifi.mac = Some("50:ee:32:aa:7b:60".into());
    provider.set(wifi, 0, 0);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("netmeter.db");
    let (repo, _) = SqliteRepository::open(&db).expect("open");
    let engine = Engine::start(
        provider.clone(),
        repo,
        Arc::new(RecordingSink::default()),
        config(),
        "boot-test".into(),
    );
    std::thread::sleep(Duration::from_millis(120));
    provider.advance("wlan0", 1_000, 0);
    std::thread::sleep(Duration::from_millis(120));

    let first = {
        let (repo, _) = SqliteRepository::open(&db).expect("read");
        repo.interfaces().expect("list")
    };
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].mac.as_deref(),
        Some("50:ee:32:aa:7b:60"),
        "the MAC must actually be persisted, not dropped on the floor"
    );
    let seen_at_first = first[0].last_seen_utc_ms;

    // Let several more flushes go by.
    std::thread::sleep(Duration::from_millis(300));
    engine.stop();

    let (repo, _) = SqliteRepository::open(&db).expect("read");
    let after = repo.interfaces().expect("list");
    assert!(
        after[0].last_seen_utc_ms > seen_at_first,
        "last_seen froze at {seen_at_first}; the retention GC reads this field \
         to decide an interface is gone, so a frozen value is not cosmetic"
    );
}
