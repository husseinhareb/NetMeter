//! Domain types shared by every layer.

use serde::{Deserialize, Serialize};

/// How an interface is classified. This drives the default accounting policy:
/// only [`InterfaceKind::Ethernet`], [`InterfaceKind::Wifi`] and
/// [`InterfaceKind::Wwan`] carry traffic that is not already counted somewhere
/// else. See `docs/ACCOUNTING.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    /// Physical wired NIC (has a `device` link, no `wireless` directory).
    Ethernet,
    /// Physical wireless NIC (`wireless/` directory or `DEVTYPE=wlan`).
    Wifi,
    /// Mobile broadband / WWAN modem.
    Wwan,
    /// Loopback (`ARPHRD_LOOPBACK`). Never real network usage.
    Loopback,
    /// TUN/TAP, WireGuard, Tailscale, PPP -- an encapsulating tunnel. Its
    /// payload is carried by, and counted again on, a physical interface.
    Vpn,
    /// A software bridge (`docker0`, `br-*`, `virbr0`).
    Bridge,
    /// A leg of a bridge or bond (`brport`/`master` present): a veth end, a
    /// bond slave. Its traffic is also visible on its master.
    Enslaved,
    /// A VLAN sub-interface. Its frames also pass over the parent NIC.
    Vlan,
    /// Present in the kernel but not matching any rule above. Treated as
    /// virtual, because physical is something we prove, never something we
    /// fall back to.
    Virtual,
}

impl InterfaceKind {
    /// True for interfaces that terminate at real hardware. These are the only
    /// ones whose bytes are not a re-count of another interface's bytes.
    pub fn is_physical(self) -> bool {
        matches!(self, Self::Ethernet | Self::Wifi | Self::Wwan)
    }

    /// True when the interface is worth keeping permanent per-interface history
    /// for. Bridge legs churn -- Docker mints a fresh random `vethXXXXXXX` on
    /// every container start -- so persisting them grows the tables without
    /// bound for data nobody queries.
    pub fn is_persistable(self) -> bool {
        !matches!(self, Self::Enslaved)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ethernet => "ethernet",
            Self::Wifi => "wifi",
            Self::Wwan => "wwan",
            Self::Loopback => "loopback",
            Self::Vpn => "vpn",
            Self::Bridge => "bridge",
            Self::Enslaved => "enslaved",
            Self::Vlan => "vlan",
            Self::Virtual => "virtual",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "ethernet" => Self::Ethernet,
            "wifi" => Self::Wifi,
            "wwan" => Self::Wwan,
            "loopback" => Self::Loopback,
            "vpn" => Self::Vpn,
            "bridge" => Self::Bridge,
            "enslaved" => Self::Enslaved,
            "vlan" => Self::Vlan,
            _ => Self::Virtual,
        }
    }
}

/// `operstate` as reported by the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceState {
    Up,
    Down,
    Dormant,
    /// What the kernel reports for `lo` and for most tunnels. Not "down".
    #[default]
    Unknown,
}

impl InterfaceState {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "up" => Self::Up,
            "down" | "lowerlayerdown" => Self::Down,
            "dormant" => Self::Dormant,
            // `lo` and most tunnels report "unknown" and are nonetheless
            // carrying traffic, so this is not treated as "down".
            _ => Self::Unknown,
        }
    }
}

/// A network interface as observed from the kernel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    pub kind: InterfaceKind,
    /// `None` for interfaces with no link-layer address (`tailscale0` has an
    /// empty `address` file). Display metadata only -- never identity, because
    /// WiFi MAC randomization and bridge membership both change it.
    pub mac: Option<String>,
    /// Kernel interface index. Stable across renames and down/up; changes when
    /// the device is destroyed and recreated, which is exactly what makes it
    /// the right guard for carrying a counter baseline forward.
    pub ifindex: u32,
    pub state: InterfaceState,
}

/// A cumulative RX/TX byte pair as the kernel reports it.
///
/// These are monotonically increasing for the lifetime of the *device*, not of
/// the name: destroying and recreating an interface restarts them at zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetworkCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// A counter reading plus the identity it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterSample {
    pub name: String,
    pub ifindex: u32,
    pub counters: NetworkCounters,
}

/// Bytes moved over some interval. Always non-negative, by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Traffic {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

impl Traffic {
    pub const ZERO: Self = Self {
        rx_bytes: 0,
        tx_bytes: 0,
    };

    pub fn new(rx_bytes: u64, tx_bytes: u64) -> Self {
        Self { rx_bytes, tx_bytes }
    }

    pub fn total_bytes(self) -> u64 {
        self.rx_bytes.saturating_add(self.tx_bytes)
    }

    pub fn is_zero(self) -> bool {
        self.rx_bytes == 0 && self.tx_bytes == 0
    }

    /// Saturating so a pathological counter can never wrap a total back to a
    /// small number. `+` and `+=` use this.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            rx_bytes: self.rx_bytes.saturating_add(other.rx_bytes),
            tx_bytes: self.tx_bytes.saturating_add(other.tx_bytes),
        }
    }
}

impl std::ops::Add for Traffic {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        self.saturating_add(rhs)
    }
}

impl std::ops::AddAssign for Traffic {
    fn add_assign(&mut self, rhs: Self) {
        *self = self.saturating_add(rhs);
    }
}

impl std::iter::Sum for Traffic {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Traffic::saturating_add)
    }
}

/// Why a delta looks the way it does. Recorded so the UI and the logs can tell
/// "you really used 4 GB" from "the counter restarted".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    /// The counter advanced normally.
    Normal,
    /// The interface was seen for the first time: the cumulative counter is
    /// whatever accrued before NetMeter was watching, so it is a baseline, not
    /// usage. Credits zero.
    Baseline,
    /// The counter went backwards: the device was destroyed and recreated (or
    /// the driver reloaded). Whatever it had counted before the reset is
    /// unrecoverable, but the post-reset value is real traffic on this
    /// interface and is credited.
    Reset,
}

/// The result of comparing two counter readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrafficDelta {
    pub traffic: Traffic,
    pub kind: DeltaKind,
}

impl TrafficDelta {
    pub const NOTHING: Self = Self {
        traffic: Traffic::ZERO,
        kind: DeltaKind::Normal,
    };
}

/// Bytes per second. `None` means "not known yet" -- the first sample after a
/// start, a zero-length interval, or an interval spanning a suspend -- which is
/// a different thing from a measured zero.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct DataRate {
    pub rx_bytes_per_sec: Option<f64>,
    pub tx_bytes_per_sec: Option<f64>,
}

impl DataRate {
    pub const UNKNOWN: Self = Self {
        rx_bytes_per_sec: None,
        tx_bytes_per_sec: None,
    };
}

/// The calendar resolution of a usage query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    /// One bucket per clock hour, from `usage_hour`. Bounded by retention.
    Hour,
    /// One bucket per local calendar day, from `usage_day`. Kept forever.
    Day,
    /// One bucket per local calendar month.
    Month,
    /// One bucket per local calendar year.
    Year,
}

impl Granularity {
    /// How many leading characters of a `YYYY-MM-DD` local date identify this
    /// bucket. Used directly as the `substr()` length in the rollup query.
    pub fn key_len(self) -> usize {
        match self {
            Self::Hour => 13, // "YYYY-MM-DDTHH"
            Self::Day => 10,
            Self::Month => 7,
            Self::Year => 4,
        }
    }
}

/// A half-open instant range `[start, end)`, in epoch milliseconds UTC.
///
/// Half-open is what makes a 23-hour or 25-hour DST day fall out of the
/// arithmetic instead of needing a special case, and it guarantees adjacent
/// periods neither overlap nor leave a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsagePeriod {
    pub start_utc_ms: i64,
    pub end_utc_ms: i64,
}

impl UsagePeriod {
    pub fn new(start_utc_ms: i64, end_utc_ms: i64) -> Self {
        Self {
            start_utc_ms,
            end_utc_ms,
        }
    }

    pub fn contains(&self, t: i64) -> bool {
        t >= self.start_utc_ms && t < self.end_utc_ms
    }

    /// Milliseconds of overlap with another period; zero when disjoint. Used to
    /// report how much of an offline window falls inside a queried range.
    pub fn overlap_ms(&self, other: &UsagePeriod) -> i64 {
        let s = self.start_utc_ms.max(other.start_utc_ms);
        let e = self.end_utc_ms.min(other.end_utc_ms);
        (e - s).max(0)
    }

    pub fn duration_ms(&self) -> i64 {
        (self.end_utc_ms - self.start_utc_ms).max(0)
    }
}

/// Totals for one period, split the two ways that matter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummary {
    /// Bytes on the interfaces the user's policy counts toward their usage.
    /// This is the headline number.
    pub included: Traffic,
    /// Bytes on every interface NetMeter recorded, including tunnels and
    /// bridges. Larger than reality by construction -- a VPN's payload appears
    /// here twice -- so it is a diagnostic, never a total.
    pub observed: Traffic,
}

impl UsageSummary {
    pub const ZERO: Self = Self {
        included: Traffic::ZERO,
        observed: Traffic::ZERO,
    };
}

/// What the sampling engine is doing, derived from heartbeats rather than from
/// a stored flag -- a flag cannot tell "sampling" from "the thread died an hour
/// ago", which is the failure a usage meter must never hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorState {
    Stopped,
    Running,
    /// Ticking, but something is wrong: writes are failing, or the sampler has
    /// been restarted after a panic.
    Degraded,
    /// Started, but has not ticked in several intervals.
    Stalled,
}

/// A full status report for the GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorStatus {
    pub state: MonitorState,
    /// When the engine was started, or `None` if it never was.
    pub started_at_utc_ms: Option<i64>,
    /// Wall-clock time of the most recent completed tick.
    pub last_tick_utc_ms: Option<i64>,
    /// Wall-clock time of the most recent successful flush to the database.
    pub last_flush_utc_ms: Option<i64>,
    pub sampling_interval_seconds: u32,
    /// Interfaces currently present in `/proc/net/dev`.
    pub interfaces_observed: u32,
    /// Of those, the ones the policy counts.
    pub interfaces_included: u32,
    /// Times the supervisor has had to restart the sampling thread.
    pub restarts: u32,
    /// `/proc/net/dev` lines skipped because they did not parse. Non-zero here
    /// means the parser is systematically wrong about this kernel.
    pub parse_errors: u64,
    /// Bytes still in memory, not yet written to the database.
    pub pending: Traffic,
    /// Populated when `state` is `Degraded`.
    pub degraded_reason: Option<String>,
}

/// Traffic attributed to one interface within a bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceTraffic {
    pub interface_id: i64,
    pub name: String,
    pub kind: InterfaceKind,
    /// Whether this interface's bytes are part of `UsageBucket::included`.
    pub included: bool,
    pub traffic: Traffic,
}

/// One bucket of a usage series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBucket {
    /// `"2026"`, `"2026-09"`, `"2026-09-11"` or `"2026-09-11T14"`.
    pub key: String,
    /// The instant range the bucket covers. Carried so a chart can position a
    /// bar without doing DST-correct date arithmetic in JavaScript, and so a
    /// 23- or 25-hour day is visibly that.
    pub start_utc_ms: i64,
    pub end_utc_ms: i64,
    pub summary: UsageSummary,
    /// Empty unless the request asked for a breakdown.
    pub by_interface: Vec<InterfaceTraffic>,
}

/// Traffic the kernel counted while NetMeter was not watching.
///
/// Reported separately, never folded into a bucket, because the counter says
/// how many bytes moved but nothing says when inside the window they moved.
/// Spreading them across the days they span would be inventing a shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineWindowSummary {
    pub from_utc_ms: i64,
    pub to_utc_ms: i64,
    pub summary: UsageSummary,
}

/// The answer to any usage question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSeries {
    pub granularity: Granularity,
    /// The IANA zone the day boundaries were computed in.
    pub timezone: String,
    /// One entry per bucket in the requested range, in order, with no gaps.
    /// A period with no traffic is a zero bucket, never a missing one.
    pub buckets: Vec<UsageBucket>,
    pub total: UsageSummary,
    /// Bytes in this range that could not be placed in a bucket.
    pub offline_total: UsageSummary,
    pub offline_windows: Vec<OfflineWindowSummary>,
    /// The interfaces whose bytes make up `total.included`, so the UI can
    /// explain the headline number.
    pub included_interfaces: Vec<String>,
    /// Local date of the earliest usage NetMeter holds, or `None` if it holds
    /// none. Lets the UI grey out bars from before it was installed instead of
    /// drawing them as a genuine zero.
    pub data_since: Option<String>,
    /// True when the last bucket includes bytes still held in memory. Without
    /// this the headline number would freeze between flushes and then jump.
    pub includes_pending: bool,
    pub generated_at_utc_ms: i64,
}
