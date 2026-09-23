//! Per-application network accounting, measured in the kernel.
//!
//! See docs/PER_APP.md. The probes attach to the protocol operations, the
//! counters live in one BPF hash map, and userspace drains that map, resolves
//! each pid to an application and writes hourly and daily rows.
//!
//! It also runs the GUI crate's interface engine, so the usage total is
//! recorded from boot rather than only while a desktop session has the GUI
//! open. The GUI reads that database and samples for itself only when no
//! daemon answers.

mod app;
mod interfaces;

use netmeter_lib::api::events::NullEventSink;
use netmeter_lib::api::ipc::{self, DaemonStatus};
use netmeter_lib::core::config::Config;
use netmeter_lib::monitor::{Engine, EngineConfig, LinuxNetworkStatsProvider};
use netmeter_lib::storage::SqliteRepository;
use netmeterd::server::Server;
use netmeterd::store::{Batch, BucketKey, Retention, Store};
use std::sync::{Arc, Mutex};

use netmeterd::skel;

use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags};
use netmeter_lib::core::time;
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const KEY_SIZE: usize = 8;
const VALUE_SIZE: usize = 32;
const COMM_LEN: usize = 16;

/// Written by a system service, read by every user's GUI through the socket.
const DEFAULT_DB: &str = "/var/lib/netmeter/apps.db";

/// The interface totals, beside the per-app database. Machine-wide and
/// readable by everyone: every user's GUI opens it read-only.
const INTERFACE_DB: &str = "netmeter.db";

/// Enough slack that ordinary sampling skew never trips the warning, small
/// enough that a systematic overcount trips it within minutes.
const EXCESS_WARN_BYTES: u64 = 64 << 20;

struct Sample {
    tgid: u32,
    uid: u32,
    comm: String,
    rx: u64,
    tx: u64,
}

fn parse_key(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() != KEY_SIZE {
        return None;
    }
    let tgid: [u8; 4] = b[0..4].try_into().ok()?;
    let uid: [u8; 4] = b[4..8].try_into().ok()?;
    Some((u32::from_le_bytes(tgid), u32::from_le_bytes(uid)))
}

fn parse_value(b: &[u8]) -> Option<(u64, u64, String)> {
    if b.len() != VALUE_SIZE {
        return None;
    }
    let rx = u64::from_le_bytes(b[0..8].try_into().ok()?);
    let tx = u64::from_le_bytes(b[8..16].try_into().ok()?);
    let raw = &b[16..16 + COMM_LEN];
    let end = raw.iter().position(|c| *c == 0).unwrap_or(COMM_LEN);
    Some((rx, tx, String::from_utf8_lossy(&raw[..end]).into_owned()))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let interval = std::env::var("NETMETERD_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| std::env::args().nth(1).and_then(|s| s.parse().ok()))
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5));
    let flush_every = Duration::from_secs(30);
    let db_path: PathBuf = std::env::var_os("NETMETERD_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DB));

    init_logging();
    let timezone = time::system_timezone();

    let (mut store, quarantined) = Store::open(&db_path).inspect_err(|e| {
        if is_permission_denied(e) {
            eprintln!(
                "\nCannot open {}: permission denied.\nAs a system service this path \
                 is created by systemd's StateDirectory=netmeter. To try it by hand, \
                 point it somewhere writable:\n\n    \
                 sudo env NETMETERD_DB=/tmp/netmeter-apps.db {} 5\n",
                db_path.display(),
                std::env::args().next().unwrap_or_default()
            );
        }
    })?;
    if let Some(path) = quarantined {
        tracing::error!(path = %path.display(), "previous database was unreadable and was moved aside");
    }
    tracing::info!(database = %db_path.display(), timezone = %timezone, "netmeterd starting");

    let mut open_object = MaybeUninit::uninit();
    let open = skel::NetmeterdSkelBuilder::default().open(&mut open_object)?;
    let skel = open.load().inspect_err(|e| {
        if e.kind() == libbpf_rs::ErrorKind::PermissionDenied {
            eprintln!(
                "\nLoading BPF needs CAP_BPF and CAP_PERFMON, and this kernel has \
                 kernel.unprivileged_bpf_disabled set.\nRun it under sudo, or install \
                 the system service described in docs/PER_APP.md.\n"
            );
        }
    })?;

    // Attached one at a time, so a signature that does not match this kernel
    // names itself instead of taking the whole program down silently.
    let mut links = Vec::new();
    let mut failed = Vec::new();
    macro_rules! attach {
        ($sym:expr, $prog:expr) => {
            match $prog.attach() {
                Ok(link) => links.push(link),
                Err(e) => {
                    tracing::error!(probe = $sym, error = %e, "probe did not attach");
                    failed.push($sym);
                }
            }
        };
    }
    attach!("tcp_sendmsg", skel.progs.tcp_send);
    attach!("udp_sendmsg", skel.progs.udp_send);
    attach!("udpv6_sendmsg", skel.progs.udpv6_send);
    attach!("tcp_recvmsg", skel.progs.tcp_recv);
    attach!("udp_recvmsg", skel.progs.udp_recv);
    attach!("udpv6_recvmsg", skel.progs.udpv6_recv);

    // Losing an IPv6 UDP probe costs some QUIC; losing TCP means the numbers
    // are meaningless, so that is fatal rather than a footnote.
    for critical in ["tcp_sendmsg", "tcp_recvmsg"] {
        if failed.contains(&critical) {
            return Err(format!("{critical} did not attach; refusing to record partial totals").into());
        }
    }
    tracing::info!(
        probes = links.len(),
        interval_s = interval.as_secs(),
        "sampling"
    );

    // Published over the socket so the GUI can tell "no data yet" from "the
    // helper is not running" from "running, but two probes did not attach".
    let status = Arc::new(Mutex::new(DaemonStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        probes_attached: links.len(),
        probes_expected: ipc::PROBES_EXPECTED,
        started_at_utc_ms: time::now_utc_ms(),
        last_flush_utc_ms: None,
        dropped: Default::default(),
        interface_db: None,
    }));

    // Held for the life of the process: dropping it stops the sampler.
    let interface_db = db_path.with_file_name(INTERFACE_DB);
    let engine = start_interface_engine(&interface_db);
    if let (Some(_), Ok(mut s)) = (&engine, status.lock()) {
        s.interface_db = Some(interface_db.display().to_string());
    }

    let socket_path = std::env::var_os("NETMETERD_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(ipc::SOCKET_PATH));
    {
        let server = Server::new(
            socket_path,
            db_path.clone(),
            timezone,
            Arc::clone(&status),
            engine.as_ref().map(|e| e.shared()),
        );
        // Its own thread with its own read-only connections, so a slow client
        // can never delay a sample.
        std::thread::spawn(move || {
            if let Err(e) = server.run() {
                tracing::error!(error = %e, "socket server stopped");
            }
        });
    }

    let tty = unsafe { libc::isatty(1) } == 1;
    let (mut last_rx, mut last_tx) = interfaces::physical_totals();
    let mut batch = Batch::default();
    let mut session: HashMap<String, (u64, u64)> = HashMap::new();
    let mut session_attributed = (0u64, 0u64);
    let mut session_wire = (0u64, 0u64);
    let mut session_overhead = (0u64, 0u64);
    // What the applications claimed above what the interfaces moved. It is
    // clamped away below, so without a running total it leaves no trace.
    let mut session_excess = (0u64, 0u64);
    let mut warned_excess = false;
    let mut last_flush = Instant::now();
    let mut last_prune_date = store.meta("last_prune_date")?;
    let started = Instant::now();

    loop {
        std::thread::sleep(interval);
        let now = time::now_utc_ms();
        let hour = time::hour_start_ms(now);
        let local_date = time::local_date_key(timezone, now);

        // Drain: read every key, then delete it. The kernel keeps counting
        // into a fresh entry while we work, so nothing is double counted.
        let counters = &skel.maps.counters;
        let mut samples = Vec::new();
        for key in counters.keys() {
            let Some((tgid, uid)) = parse_key(&key) else {
                continue;
            };
            let Ok(Some(raw)) = counters.lookup(&key, MapFlags::ANY) else {
                continue;
            };
            let Some((rx, tx, comm)) = parse_value(&raw) else {
                continue;
            };
            let _ = counters.delete(&key);
            samples.push(Sample {
                tgid,
                uid,
                comm,
                rx,
                tx,
            });
        }

        let mut tick_attributed = (0u64, 0u64);
        for s in samples {
            // Resolved now, while the process is probably still alive: after
            // it exits only `comm` is left.
            let key = app::identify(s.tgid, &s.comm);
            if let Some(exe) = app::exe_path(s.tgid) {
                batch.exe_paths.insert(key.clone(), exe);
            }
            tick_attributed.0 += s.rx;
            tick_attributed.1 += s.tx;
            let slot = session.entry(key.clone()).or_insert((0, 0));
            slot.0 += s.rx;
            slot.1 += s.tx;
            batch.add(
                BucketKey {
                    hour_start_utc_ms: hour,
                    local_date: local_date.clone(),
                    uid: s.uid,
                    app: key,
                },
                s.rx,
                s.tx,
            );
        }
        session_attributed.0 += tick_attributed.0;
        session_attributed.1 += tick_attributed.1;

        // Protocol overhead: what the interfaces moved that no application
        // owns. Headers and acknowledgements, measured rather than modelled.
        let (now_rx, now_tx) = interfaces::physical_totals();
        let wire = if now_rx < last_rx || now_tx < last_tx {
            // An interface counter went backwards: a reset, not traffic.
            tracing::info!("interface counters reset; rebasing");
            (0, 0)
        } else {
            (now_rx - last_rx, now_tx - last_tx)
        };
        last_rx = now_rx;
        last_tx = now_tx;
        session_wire.0 += wire.0;
        session_wire.1 += wire.1;

        // Clamped, not signed: within one tick the two measurements are taken
        // moments apart, so a small negative is skew rather than a finding.
        let overhead = (
            wire.0.saturating_sub(tick_attributed.0),
            wire.1.saturating_sub(tick_attributed.1),
        );

        // The other side of that clamp. Skew cancels out over a session; a
        // systematic overcount does not, and it used to floor at zero here
        // and reach the GUI as a per-app total larger than the machine's own.
        session_excess.0 += tick_attributed.0.saturating_sub(wire.0);
        session_excess.1 += tick_attributed.1.saturating_sub(wire.1);
        if !warned_excess && session_excess.0 + session_excess.1 > EXCESS_WARN_BYTES {
            warned_excess = true;
            tracing::warn!(
                rx_bytes = session_excess.0,
                tx_bytes = session_excess.1,
                "attributed traffic exceeds the wire total: the per-app figures \
                 include bytes that never reached a NIC. Tunnelled payload counted \
                 once for the application and again for the VPN client is the \
                 expected cause; anything larger is a probe reading the wrong value."
            );
        }
        if overhead.0 > 0 || overhead.1 > 0 {
            batch.add_overhead(local_date.clone(), overhead.0, overhead.1);
            session_overhead.0 += overhead.0;
            session_overhead.1 += overhead.1;
        }

        if last_flush.elapsed() >= flush_every {
            store.flush(&batch, now)?;
            let rows = batch.buckets.len();
            batch = Batch::default();
            last_flush = Instant::now();
            tracing::debug!(rows, "flushed");

            let dropped = read_drops(&skel.maps.dropped);
            if let Ok(mut s) = status.lock() {
                s.last_flush_utc_ms = Some(now);
                s.dropped = ipc::Traffic {
                    rx_bytes: dropped.0,
                    tx_bytes: dropped.1,
                };
            }

            if last_prune_date.as_deref() != Some(local_date.as_str()) {
                store.prune(Retention::default(), timezone, now)?;
                store.set_meta("last_prune_date", &local_date)?;
                last_prune_date = Some(local_date.clone());
                tracing::info!(date = %local_date, "pruned");
            }
        }

        if tty {
            report(
                started.elapsed(),
                &session,
                session_attributed,
                session_wire,
                session_overhead,
                read_drops(&skel.maps.dropped),
            );
        }
    }
}

/// The GUI's interface sampler, hosted here so it runs whether or not anyone
/// is logged in.
///
/// Default configuration on purpose: the interface policy only decides what
/// counts toward a total, which each GUI applies at query time from its own
/// settings; every interface is recorded regardless. A failure costs the
/// interface history, not the per-app accounting, so it is logged rather than
/// fatal, and the GUI falls back to sampling for itself.
fn start_interface_engine(path: &std::path::Path) -> Option<Engine> {
    let (repository, quarantined) = match SqliteRepository::open(path) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "interface database did not open");
            return None;
        }
    };
    if let Some(p) = quarantined {
        tracing::error!(path = %p.display(), "previous interface database was unreadable and was moved aside");
    }
    let config = Config::default();
    tracing::info!(database = %path.display(), "recording interfaces");
    Some(Engine::start(
        LinuxNetworkStatsProvider::new(),
        repository,
        Arc::new(NullEventSink),
        EngineConfig::from(&config),
        netmeter_lib::system::power::boot_id(),
    ))
}

/// Both the data directory and the database file itself can be refused, and
/// the two arrive as different variants.
fn is_permission_denied(e: &netmeter_lib::core::errors::StorageError) -> bool {
    use netmeter_lib::core::errors::StorageError;
    match e {
        StorageError::DataDir { source, .. } => {
            source.kind() == std::io::ErrorKind::PermissionDenied
        }
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(f, _)) => {
            f.code == rusqlite::ErrorCode::CannotOpen
        }
        _ => false,
    }
}

fn init_logging() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("netmeterd=info,netmeter_lib=warn"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().without_time())
        .try_init();
}

fn read_drops(map: &libbpf_rs::Map) -> (u64, u64) {
    let one = |slot: u32| {
        map.lookup(&slot.to_le_bytes(), MapFlags::ANY)
            .ok()
            .flatten()
            .and_then(|v| v.get(0..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    };
    (one(0), one(1))
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", 100.0 * part as f64 / whole as f64)
    }
}

/// Printed only on a terminal. Under systemd this would be journal noise on
/// every tick, and the database is the real output.
fn report(
    elapsed: Duration,
    totals: &HashMap<String, (u64, u64)>,
    attributed: (u64, u64),
    wire: (u64, u64),
    overhead: (u64, u64),
    drops: (u64, u64),
) {
    let mut rows: Vec<_> = totals.iter().collect();
    rows.sort_by_key(|(_, (rx, tx))| std::cmp::Reverse(rx + tx));

    println!("\n=== {:.0}s since start ===", elapsed.as_secs_f64());
    println!("{:<28}{:>12}{:>12}", "application", "down", "up");
    for (name, (rx, tx)) in rows.iter().take(15) {
        println!("{:<28}{:>12}{:>12}", name, bytes(*rx), bytes(*tx));
    }
    println!("{:-<52}", "");
    println!(
        "{:<28}{:>12}{:>12}",
        "attributed",
        bytes(attributed.0),
        bytes(attributed.1)
    );
    println!(
        "{:<28}{:>12}{:>12}",
        "on the wire (physical)",
        bytes(wire.0),
        bytes(wire.1)
    );
    println!(
        "{:<28}{:>12}{:>12}",
        "protocol overhead",
        format!("{} ({})", bytes(overhead.0), share(overhead.0, wire.0)),
        format!("{} ({})", bytes(overhead.1), share(overhead.1, wire.1))
    );

    // The alarm that would have caught a bad probe in one tick rather than
    // three minutes of plausible-looking table.
    if attributed.0 > wire.0.saturating_mul(2) || attributed.1 > wire.1.saturating_mul(2) {
        println!(
            "  IMPLAUSIBLE: attributed traffic exceeds the wire total. A probe is \
             reading the wrong value -- do not trust the table above."
        );
    }
    if drops.0 + drops.1 > 0 {
        println!(
            "{:<28}{:>12}{:>12}  (map full)",
            "lost in kernel",
            bytes(drops.0),
            bytes(drops.1)
        );
    }
}
