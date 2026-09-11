//! Reading the kernel's counters from `/proc/net/dev`.
//!
//! Parsing notes, because this file is the one that silently reports wrong
//! numbers if it is written casually:
//!
//! * The separator is the colon, not whitespace. Older kernels rendered the
//!   line as `"%6s:%8llu ..."` with no space after the colon, so a wide byte
//!   count produces `eth0:1234567890` with nothing between them. Splitting on
//!   whitespace first therefore loses the interface name on a busy machine --
//!   the classic bug in this file. `dev_valid_name()` rejects `:` in interface
//!   names, so the first colon is an unambiguous separator.
//! * Column *count* is not stable across kernel versions, but the first two
//!   fields (rx bytes, rx packets) and the ninth (tx bytes) have been fixed
//!   since the 64-bit stats rework. Fields are located by index from the left,
//!   never by character offset -- the values routinely overflow their
//!   `%7llu` field width, so columns do not line up.
//! * Parsing is per-line fallible. One malformed line, or one interface with a
//!   non-UTF-8 name, must cost that interface only, never the whole tick.
//!   The file is read as bytes for exactly this reason: `read_to_string`
//!   rejects the *entire* file if any byte is not valid UTF-8.

use super::interfaces::{self, SYS_CLASS_NET};
use super::provider::NetworkStatsProvider;
use crate::core::errors::MonitorError;
use crate::core::types::{CounterSample, NetworkCounters, NetworkInterface};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Where the kernel renders per-interface counters.
pub const PROC_NET_DEV: &str = "/proc/net/dev";

/// Index of `rx_bytes` among the whitespace-separated values after the colon.
const RX_BYTES_FIELD: usize = 0;
/// Index of `tx_bytes`. Eight receive columns precede it: bytes, packets,
/// errs, drop, fifo, frame, compressed, multicast.
const TX_BYTES_FIELD: usize = 8;

/// Reads counters from `/proc/net/dev` and metadata from `/sys/class/net`.
#[derive(Debug)]
pub struct LinuxNetworkStatsProvider {
    proc_net_dev: PathBuf,
    sys_class_net: PathBuf,
    /// Lines that did not parse, ever. Surfaced in `MonitorStatus` so a
    /// systematically wrong parser is visible instead of silently dropping
    /// interfaces.
    parse_errors: AtomicU64,
}

impl Default for LinuxNetworkStatsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxNetworkStatsProvider {
    pub fn new() -> Self {
        Self::with_paths(PROC_NET_DEV, SYS_CLASS_NET)
    }

    /// Point the provider at alternative paths. Used by tests to parse
    /// captured kernel output without a kernel.
    pub fn with_paths(proc_net_dev: impl Into<PathBuf>, sys_class_net: impl Into<PathBuf>) -> Self {
        Self {
            proc_net_dev: proc_net_dev.into(),
            sys_class_net: sys_class_net.into(),
            parse_errors: AtomicU64::new(0),
        }
    }

    pub fn parse_errors(&self) -> u64 {
        self.parse_errors.load(Ordering::Relaxed)
    }
}

impl NetworkStatsProvider for LinuxNetworkStatsProvider {
    fn parse_errors(&self) -> u64 {
        self.parse_errors.load(Ordering::Relaxed)
    }

    fn counters(&self) -> Result<Vec<CounterSample>, MonitorError> {
        let bytes = std::fs::read(&self.proc_net_dev).map_err(|source| MonitorError::Read {
            path: self.proc_net_dev.display().to_string(),
            source,
        })?;

        let parsed = parse_proc_net_dev(&bytes);
        if parsed.skipped > 0 {
            self.parse_errors
                .fetch_add(parsed.skipped, Ordering::Relaxed);
        }
        if parsed.rows.is_empty() {
            // Every machine has at least `lo`. An empty parse means the format
            // is not what we think it is, and reporting "no traffic" would be
            // a lie; refusing the sample is the honest outcome.
            return Err(MonitorError::Format {
                path: self.proc_net_dev.display().to_string(),
                detail: format!("no interface lines parsed ({} skipped)", parsed.skipped),
            });
        }

        Ok(parsed
            .rows
            .into_iter()
            .map(|r| CounterSample {
                ifindex: read_ifindex(&self.sys_class_net, &r.name).unwrap_or(0),
                name: r.name,
                counters: NetworkCounters {
                    rx_bytes: r.rx_bytes,
                    tx_bytes: r.tx_bytes,
                },
            })
            .collect())
    }

    fn interfaces(&self) -> Result<Vec<NetworkInterface>, MonitorError> {
        // Driven from /proc/net/dev rather than from readdir(/sys/class/net)
        // so the interface list and the counter list can never disagree.
        let bytes = std::fs::read(&self.proc_net_dev).map_err(|source| MonitorError::Read {
            path: self.proc_net_dev.display().to_string(),
            source,
        })?;

        Ok(parse_proc_net_dev(&bytes)
            .rows
            .into_iter()
            .map(|r| {
                let facts = interfaces::read_facts(&self.sys_class_net, &r.name);
                NetworkInterface {
                    kind: interfaces::classify(&r.name, &facts),
                    mac: facts.mac.clone(),
                    ifindex: facts.ifindex.unwrap_or(0),
                    state: facts.operstate,
                    name: r.name,
                }
            })
            .collect())
    }
}

fn read_ifindex(sys_class_net: &Path, name: &str) -> Option<u32> {
    std::fs::read_to_string(sys_class_net.join(name).join("ifindex"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// One parsed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterRow {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Everything a parse produced, including what it had to discard.
#[derive(Debug, Default)]
pub struct ParsedCounters {
    pub rows: Vec<CounterRow>,
    /// Lines that looked like data but could not be read.
    pub skipped: u64,
}

/// Parse the body of `/proc/net/dev`.
///
/// Pure, takes bytes, never fails as a whole: a line that cannot be read is
/// counted and skipped.
pub fn parse_proc_net_dev(bytes: &[u8]) -> ParsedCounters {
    let mut out = ParsedCounters::default();

    for line in bytes.split(|&b| b == b'\n') {
        // Split on the colon first -- see the module note on why whitespace
        // splitting is wrong here.
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            // The two header lines and the trailing empty line have no colon
            // in a position that matters; they are not errors.
            continue;
        };

        let (name_part, values) = line.split_at(colon);
        let name = match std::str::from_utf8(name_part) {
            Ok(s) => s.trim(),
            // A non-UTF-8 interface name costs this interface only. The kernel
            // permits it; nothing else in NetMeter can represent it.
            Err(_) => {
                out.skipped += 1;
                continue;
            }
        };

        if name.is_empty() {
            continue;
        }
        // Guards against the header line `"Inter-|   Receive ... |  Transmit"`,
        // which contains a colon in some locales/kernels.
        if name.contains('|') || name.contains(char::is_whitespace) {
            continue;
        }

        let mut fields = values[1..]
            .split(|b: &u8| b.is_ascii_whitespace())
            .filter(|f| !f.is_empty())
            .map(|f| std::str::from_utf8(f).ok().and_then(|s| s.parse::<u64>().ok()));

        let rx = fields.nth(RX_BYTES_FIELD).flatten();
        // `nth` consumed through RX_BYTES_FIELD, so the remaining offset is
        // the difference minus one.
        let tx = fields.nth(TX_BYTES_FIELD - RX_BYTES_FIELD - 1).flatten();

        match (rx, tx) {
            (Some(rx_bytes), Some(tx_bytes)) => out.rows.push(CounterRow {
                name: name.to_string(),
                rx_bytes,
                tx_bytes,
            }),
            _ => out.skipped += 1,
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim output of `cat /proc/net/dev` on the development machine.
    const REAL: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1084632059  303408    0    0    0     0          0         0 1084632059  303408    0    0    0     0       0          0
enp8s0:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0
wlp7s0: 4105000937 3735189    0    1    0     0          0         0 1037860334 1972876    0    0    0     0       0          0
tailscale0: 7915557  101320    0    0    0     0          0         0 12637091   88955    0    0    0     0       0          0
br-ee189c1b10a3:   87422     901    0    0    0     0          0         0   235274    1663    0    1    0     0       0          0
docker0:       0       0    0    0    0     0          0         0        0       0    0  113    0     0       0          0
vethed62bc1: 100474511  625733    0    0    0     0          0         0 201667766  660308    0    0    0     0       0          0
";

    fn get<'a>(p: &'a ParsedCounters, name: &str) -> &'a CounterRow {
        p.rows
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} missing from parse"))
    }

    #[test]
    fn parses_real_kernel_output() {
        let p = parse_proc_net_dev(REAL.as_bytes());
        assert_eq!(p.skipped, 0, "no line in real kernel output should skip");
        assert_eq!(p.rows.len(), 7);

        assert_eq!(get(&p, "lo").rx_bytes, 1_084_632_059);
        assert_eq!(get(&p, "lo").tx_bytes, 1_084_632_059);
        // The headline case: a value past 2^32, proving 64-bit counters.
        assert_eq!(get(&p, "wlp7s0").rx_bytes, 4_105_000_937);
        assert_eq!(get(&p, "wlp7s0").tx_bytes, 1_037_860_334);
        assert_eq!(get(&p, "enp8s0").rx_bytes, 0);
        assert_eq!(get(&p, "tailscale0").tx_bytes, 12_637_091);
        // A name longer than the %6s field width, with leading spaces in the
        // value.
        assert_eq!(get(&p, "br-ee189c1b10a3").rx_bytes, 87_422);
        assert_eq!(get(&p, "br-ee189c1b10a3").tx_bytes, 235_274);
    }

    #[test]
    fn header_lines_are_never_mistaken_for_interfaces() {
        let p = parse_proc_net_dev(REAL.as_bytes());
        for r in &p.rows {
            assert!(!r.name.contains("Inter"), "header parsed as interface");
            assert!(!r.name.contains("face"), "header parsed as interface");
        }
    }

    #[test]
    fn handles_the_no_space_after_colon_form() {
        // Older kernels used "%6s:%8llu": a wide value leaves no separator.
        // Splitting on whitespace first would lose the name entirely here.
        let s = "  eth0:4294967296 100 0 0 0 0 0 0 8589934592 200 0 0 0 0 0 0\n";
        let p = parse_proc_net_dev(s.as_bytes());
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].name, "eth0");
        assert_eq!(p.rows[0].rx_bytes, 4_294_967_296);
        assert_eq!(p.rows[0].tx_bytes, 8_589_934_592);
    }

    #[test]
    fn handles_names_with_dots_and_dashes() {
        let s = "eth0.100: 10 1 0 0 0 0 0 0 20 2 0 0 0 0 0 0\n\
                 br-abc123: 30 3 0 0 0 0 0 0 40 4 0 0 0 0 0 0\n";
        let p = parse_proc_net_dev(s.as_bytes());
        assert_eq!(get(&p, "eth0.100").rx_bytes, 10);
        assert_eq!(get(&p, "br-abc123").tx_bytes, 40);
    }

    #[test]
    fn u64_max_counters_parse_without_overflow() {
        let s = format!(
            "x0: {m} 1 0 0 0 0 0 0 {m} 2 0 0 0 0 0 0\n",
            m = u64::MAX
        );
        let p = parse_proc_net_dev(s.as_bytes());
        assert_eq!(p.rows[0].rx_bytes, u64::MAX);
        assert_eq!(p.rows[0].tx_bytes, u64::MAX);
    }

    #[test]
    fn a_malformed_line_costs_only_that_interface() {
        let s = "good0: 10 1 0 0 0 0 0 0 20 2 0 0 0 0 0 0\n\
                 bad0: not-a-number\n\
                 short0: 1 2 3\n\
                 good1: 30 1 0 0 0 0 0 0 40 2 0 0 0 0 0 0\n";
        let p = parse_proc_net_dev(s.as_bytes());
        assert_eq!(p.rows.len(), 2, "both good lines survive");
        assert_eq!(p.skipped, 2);
        assert_eq!(get(&p, "good0").rx_bytes, 10);
        assert_eq!(get(&p, "good1").tx_bytes, 40);
    }

    #[test]
    fn a_non_utf8_interface_name_costs_only_that_interface() {
        let mut bytes = b"good0: 10 1 0 0 0 0 0 0 20 2 0 0 0 0 0 0\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, b'0']);
        bytes.extend_from_slice(b": 1 2 0 0 0 0 0 0 3 4 0 0 0 0 0 0\n");
        bytes.extend_from_slice(b"good1: 30 1 0 0 0 0 0 0 40 2 0 0 0 0 0 0\n");

        let p = parse_proc_net_dev(&bytes);
        assert_eq!(p.rows.len(), 2, "the whole file must not be rejected");
        assert_eq!(p.skipped, 1);
    }

    #[test]
    fn truncated_and_empty_input_do_not_panic() {
        assert!(parse_proc_net_dev(b"").rows.is_empty());
        assert!(parse_proc_net_dev(b"\n\n\n").rows.is_empty());
        assert!(parse_proc_net_dev(b"Inter-|   Receive").rows.is_empty());
        // A line cut off mid-write.
        let p = parse_proc_net_dev(b"eth0: 123 45");
        assert_eq!(p.rows.len(), 0);
        assert_eq!(p.skipped, 1);
    }

    #[test]
    fn crlf_line_endings_parse() {
        let s = "eth0: 10 1 0 0 0 0 0 0 20 2 0 0 0 0 0 0\r\n";
        let p = parse_proc_net_dev(s.as_bytes());
        assert_eq!(p.rows.len(), 1);
        assert_eq!(p.rows[0].tx_bytes, 20);
    }

    #[test]
    fn extra_trailing_columns_are_ignored() {
        // Future kernels may append columns; the ones we read are positional
        // from the left, so that must not matter.
        let s = "eth0: 10 1 0 0 0 0 0 0 20 2 0 0 0 0 0 0 999 888 777\n";
        let p = parse_proc_net_dev(s.as_bytes());
        assert_eq!(p.rows[0].rx_bytes, 10);
        assert_eq!(p.rows[0].tx_bytes, 20);
    }

    #[test]
    fn reads_the_live_kernel_file_on_this_machine() {
        // An integration check against the real kernel: whatever this machine
        // has, the parser must find at least loopback and agree it is sane.
        let p = LinuxNetworkStatsProvider::new();
        let Ok(samples) = p.counters() else {
            return; // not Linux, or /proc not mounted
        };
        assert!(!samples.is_empty(), "the kernel always reports at least lo");
        assert!(
            samples.iter().any(|s| s.name == "lo"),
            "loopback must be present"
        );
        assert_eq!(p.parse_errors(), 0, "real kernel output must parse cleanly");

        let ifaces = p.interfaces().expect("interfaces readable");
        assert_eq!(
            ifaces.len(),
            samples.len(),
            "the interface list and the counter list must not disagree"
        );
        for i in &ifaces {
            assert!(i.ifindex > 0, "{} has no ifindex", i.name);
        }
    }
}
