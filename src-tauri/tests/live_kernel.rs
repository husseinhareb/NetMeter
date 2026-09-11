//! Tests against the real kernel on the machine running them.
//!
//! These assert invariants that must hold on *any* Linux box, not facts about
//! this particular one, so they stay meaningful in CI and on a developer
//! laptop alike. They skip cleanly where `/proc/net/dev` is unavailable.

use netmeter_lib::core::config::Config;
use netmeter_lib::core::types::InterfaceKind;
use netmeter_lib::monitor::provider::NetworkStatsProvider;
use netmeter_lib::monitor::LinuxNetworkStatsProvider;

fn provider() -> Option<LinuxNetworkStatsProvider> {
    let p = LinuxNetworkStatsProvider::new();
    p.counters().ok().map(|_| p)
}

#[test]
fn the_real_proc_net_dev_parses_with_no_errors() {
    let Some(p) = provider() else { return };
    let samples = p.counters().expect("counters");
    assert!(!samples.is_empty(), "every Linux box reports at least lo");
    assert_eq!(
        p.parse_errors(),
        0,
        "a non-zero count means the parser is wrong about this kernel's format"
    );
    for s in &samples {
        assert!(!s.name.is_empty());
        assert!(
            !s.name.contains(':') && !s.name.contains(char::is_whitespace),
            "{:?} is not a valid interface name -- the colon split is wrong",
            s.name
        );
    }
}

#[test]
fn counters_only_ever_increase_between_two_real_reads() {
    let Some(p) = provider() else { return };
    let first = p.counters().expect("first read");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let second = p.counters().expect("second read");

    for a in &first {
        let Some(b) = second.iter().find(|s| s.name == a.name) else {
            continue; // an interface may legitimately vanish between reads
        };
        // If this ever fails on a quiet quarter-second, the reset rule is
        // firing on something that is not a reset.
        assert!(
            b.counters.rx_bytes >= a.counters.rx_bytes,
            "{} rx went backwards without a device reset",
            a.name
        );
        assert!(b.counters.tx_bytes >= a.counters.tx_bytes, "{} tx", a.name);
    }
}

#[test]
fn nothing_without_a_device_link_is_ever_counted_as_usage() {
    // The single invariant that prevents double counting, checked against
    // whatever interfaces this machine actually has.
    let Some(p) = provider() else { return };
    let policy = Config::default().resolve();

    for iface in p.interfaces().expect("interfaces") {
        if !policy.counts(&iface.name, iface.kind) {
            continue;
        }
        assert!(
            iface.kind.is_physical(),
            "{} ({:?}) is counted but is not physical",
            iface.name,
            iface.kind
        );
        let has_device = std::path::Path::new("/sys/class/net")
            .join(&iface.name)
            .join("device")
            .exists();
        assert!(
            has_device,
            "{} is counted but has no device link -- it is a tunnel or a bridge",
            iface.name
        );
    }
}

#[test]
fn loopback_tunnels_and_bridges_are_never_in_the_default_total() {
    let Some(p) = provider() else { return };
    let policy = Config::default().resolve();
    for iface in p.interfaces().expect("interfaces") {
        if matches!(
            iface.kind,
            InterfaceKind::Loopback
                | InterfaceKind::Vpn
                | InterfaceKind::Bridge
                | InterfaceKind::Enslaved
                | InterfaceKind::Vlan
                | InterfaceKind::Virtual
        ) {
            assert!(
                !policy.counts(&iface.name, iface.kind),
                "{} ({:?}) would double-count",
                iface.name,
                iface.kind
            );
        }
    }
}

#[test]
fn every_interface_has_a_kernel_index() {
    let Some(p) = provider() else { return };
    for iface in p.interfaces().expect("interfaces") {
        assert!(
            iface.ifindex > 0,
            "{} has no ifindex; baseline identity would be unguarded",
            iface.name
        );
    }
}

#[test]
fn reading_the_counters_is_cheap_enough_to_do_every_few_seconds() {
    // The resource budget, measured rather than asserted in prose. One read of
    // a ~2 KB pseudo-file plus a parse; if this is slow, the polling design is
    // wrong.
    let Some(p) = provider() else { return };
    let start = std::time::Instant::now();
    const N: u32 = 200;
    for _ in 0..N {
        let _ = p.counters().expect("read");
    }
    let per_read = start.elapsed() / N;
    println!("per counter read: {per_read:?}");
    assert!(
        per_read < std::time::Duration::from_millis(5),
        "a counter read took {per_read:?}; at a 5 s interval this must be negligible"
    );
}

#[test]
fn the_system_timezone_resolves_to_something_usable() {
    let tz = netmeter_lib::core::time::system_timezone();
    let today = netmeter_lib::core::time::local_date_key(
        tz,
        netmeter_lib::core::time::now_utc_ms(),
    );
    assert_eq!(today.len(), 10, "expected YYYY-MM-DD, got {today:?}");
    // And the day it names is a real, non-degenerate day.
    let period =
        netmeter_lib::core::time::period_for_key(tz, &today).expect("today parses");
    let hours = period.duration_ms() / 3_600_000;
    assert!(
        (23..=25).contains(&hours),
        "a local day was {hours} hours long"
    );
}
