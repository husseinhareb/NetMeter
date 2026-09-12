//! Per-application network accounting, measured in the kernel.
//!
//! Step 2 of docs/PER_APP.md: the BPF program and in-memory counters, printing
//! to stdout. No persistence and no socket yet -- the point of this stage is to
//! check the probes against reality and to find out how much traffic per-app
//! accounting cannot attribute.

mod app;
mod interfaces;

mod skel {
    #![allow(clippy::all, dead_code, non_snake_case, non_camel_case_types)]
    include!(concat!(env!("OUT_DIR"), "/netmeterd.skel.rs"));
}

use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags};
use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::time::{Duration, Instant};

const KEY_SIZE: usize = 8;
const VALUE_SIZE: usize = 32;
const COMM_LEN: usize = 16;

struct Sample {
    tgid: u32,
    comm: String,
    rx: u64,
    tx: u64,
}

fn parse_key(b: &[u8]) -> Option<u32> {
    if b.len() != KEY_SIZE {
        return None;
    }
    let tgid: [u8; 4] = b[0..4].try_into().ok()?;
    Some(u32::from_le_bytes(tgid))
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
    let interval = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5));

    let mut open_object = MaybeUninit::uninit();
    let open = skel::NetmeterdSkelBuilder::default().open(&mut open_object)?;
    let skel = open.load().map_err(|e| {
        if e.kind() == libbpf_rs::ErrorKind::PermissionDenied {
            eprintln!(
                "\nLoading BPF needs CAP_BPF and CAP_PERFMON, and this kernel has \
                 kernel.unprivileged_bpf_disabled set.\nRun it under sudo, or install \
                 the system service described in docs/PER_APP.md.\n"
            );
        }
        e
    })?;


    // Attached one at a time, so a signature that does not match this kernel
    // names itself instead of taking the whole program down silently.
    let mut links = Vec::new();
    let mut failed = Vec::new();
    macro_rules! attach {
        ($sym:expr, $prog:expr) => {
            match $prog.attach() {
                Ok(link) => {
                    links.push(link);
                    eprintln!("  attached {}", $sym);
                }
                Err(e) => {
                    eprintln!("  FAILED   {}: {e}", $sym);
                    failed.push($sym);
                }
            }
        };
    }
    attach!("tcp_sendmsg", skel.progs.tcp_send);
    attach!("udp_sendmsg", skel.progs.udp_send);
    attach!("udpv6_sendmsg", skel.progs.udpv6_send);
    attach!("tcp_cleanup_rbuf", skel.progs.tcp_recv);
    attach!("udp_recvmsg", skel.progs.udp_recv);
    attach!("udpv6_recvmsg", skel.progs.udpv6_recv);

    // Losing an IPv6 UDP probe costs some QUIC; losing TCP means the numbers
    // are meaningless, so that is fatal rather than a footnote.
    for critical in ["tcp_sendmsg", "tcp_cleanup_rbuf"] {
        if failed.contains(&critical) {
            return Err(format!("{critical} did not attach; refusing to report partial totals").into());
        }
    }
    eprintln!("sampling every {}s, ctrl-c to stop", interval.as_secs());

    let (mut base_rx, mut base_tx) = interfaces::physical_totals();
    let mut totals: HashMap<String, (u64, u64)> = HashMap::new();
    let mut attributed = (0u64, 0u64);
    let started = Instant::now();

    loop {
        std::thread::sleep(interval);

        // Drain: read every key, then delete it. Anything the kernel adds
        // between the read and the delete is lost, so the delete is what the
        // next tick's arithmetic is based on, never a remembered value.
        let counters = &skel.maps.counters;
        let mut samples = Vec::new();
        for key in counters.keys() {
            let Some(tgid) = parse_key(&key) else { continue };
            let Ok(Some(raw)) = counters.lookup(&key, MapFlags::ANY) else {
                continue;
            };
            let Some((rx, tx, comm)) = parse_value(&raw) else {
                continue;
            };
            let _ = counters.delete(&key);
            samples.push(Sample { tgid, comm, rx, tx });
        }

        for s in samples {
            let key = app::identify(s.tgid, &s.comm);
            let slot = totals.entry(key).or_insert((0, 0));
            slot.0 += s.rx;
            slot.1 += s.tx;
            attributed.0 += s.rx;
            attributed.1 += s.tx;
        }

        let (now_rx, now_tx) = interfaces::physical_totals();
        // A counter that went backwards means the interface reset; rebase
        // rather than report negative traffic.
        if now_rx < base_rx || now_tx < base_tx {
            base_rx = now_rx;
            base_tx = now_tx;
        }
        let wire = (now_rx - base_rx, now_tx - base_tx);

        let drops = read_drops(&skel.maps.dropped);
        report(started.elapsed(), &totals, attributed, wire, drops);
    }
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

fn report(
    elapsed: Duration,
    totals: &HashMap<String, (u64, u64)>,
    attributed: (u64, u64),
    wire: (u64, u64),
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
    let missing = (
        wire.0.saturating_sub(attributed.0),
        wire.1.saturating_sub(attributed.1),
    );
    println!(
        "{:<28}{:>12}{:>12}",
        "unattributed",
        format!("{} / {}", bytes(missing.0), share(missing.0, wire.0)),
        format!("{} / {}", bytes(missing.1), share(missing.1, wire.1))
    );
    if drops.0 + drops.1 > 0 {
        println!(
            "{:<28}{:>12}{:>12}  (map full)",
            "lost in kernel",
            bytes(drops.0),
            bytes(drops.1)
        );
    }
}
