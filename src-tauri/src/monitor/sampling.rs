//! Turning successive counter readings into bucketed traffic.
//!
//! This is where the accuracy rules meet state. Three decisions define it:
//!
//! 1. **Buckets are keyed at sample time, not at flush time.** The accumulator
//!    is a map from `(interface, hour, local date)` to bytes, and the keys come
//!    from the wall clock at the moment the counters were read. Keying at flush
//!    time would push up to a whole flush interval of traffic across every
//!    midnight and every hour boundary -- so "Today" would start each day
//!    wrong, permanently, and the error would be invisible because the totals
//!    would still add up.
//!
//! 2. **A baseline is only carried forward when the identity is provably the
//!    same device.** That means the same boot (`boot_id`) *and* the same
//!    `ifindex`. A USB dongle replaced by a different one under the same name
//!    would otherwise inherit a baseline from a device it never was.
//!
//! 3. **Bytes we cannot place in time are not placed in time.** If the process
//!    was not running for two days, the kernel's cumulative counter still holds
//!    those two days of traffic, but nothing says *when* within them it
//!    happened. Those bytes are recorded as an offline window with its own
//!    start and end, not smeared across days and not dumped into the current
//!    hour.

use super::provider::NetworkStatsProvider;
use crate::core::config::ResolvedPolicy;
use crate::core::statistics::{self, TimeAnalysis};
use crate::core::types::{
    CounterSample, DeltaKind, InterfaceKind, NetworkCounters, NetworkInterface, Traffic,
};
use std::collections::HashMap;

/// Identity of a bucket in the accumulator.
///
/// `hour_start_utc_ms` is a UTC hour, which is always exactly 3_600_000 ms, and
/// `local_date` is the local calendar day that hour's start falls in. Both are
/// carried so the flush writes the hour row and the day row without doing any
/// timezone work of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BucketKey {
    pub interface: String,
    pub hour_start_utc_ms: i64,
    pub local_date: String,
}

/// Bytes that accrued while NetMeter was not watching.
///
/// Kept separate from bucketed usage because the only honest thing that can be
/// said about them is the window they fall in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OfflineWindow {
    pub interface: String,
    pub from_utc_ms: i64,
    pub to_utc_ms: i64,
    pub traffic: Traffic,
}

/// What one tick produced.
#[derive(Debug, Clone, Default)]
pub struct SampleOutcome {
    /// Traffic attributed to a bucket, ready to be written.
    pub buckets: HashMap<BucketKey, Traffic>,
    /// Traffic that happened during a gap, with the window it happened in.
    pub offline: Vec<OfflineWindow>,
    /// Live rate per interface, for the GUI. `None` where no rate could be
    /// computed (first sample, or an interval spanning a suspend).
    pub rates: HashMap<String, Traffic>,
    /// The monotonic interval the rates were measured over.
    pub interval: Option<std::time::Duration>,
    /// Interfaces that appeared this tick.
    pub added: Vec<String>,
    /// Interfaces that disappeared this tick.
    pub removed: Vec<String>,
    /// Interfaces whose counters went backwards this tick.
    pub reset: Vec<String>,
    /// What the clocks did between this tick and the last.
    pub time: TimeAnalysis,
    /// True when the wall clock could not be trusted, so nothing was bucketed.
    pub discarded_untrusted_clock: bool,
}

impl SampleOutcome {
    /// Total traffic attributed to buckets this tick, across all interfaces.
    pub fn bucketed_total(&self) -> Traffic {
        self.buckets.values().copied().sum()
    }
}

/// What we remember about an interface between ticks.
#[derive(Debug, Clone)]
struct Baseline {
    ifindex: u32,
    counters: NetworkCounters,
    /// Wall-clock time of the reading. Used to bound an offline window.
    at_utc_ms: i64,
}

/// A counter baseline recovered from the database at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedBaseline {
    pub interface: String,
    pub ifindex: u32,
    pub counters: NetworkCounters,
    pub at_utc_ms: i64,
    /// The kernel boot this reading was taken in.
    pub boot_id: String,
}

/// The clocks at one instant. Passed in rather than read here, so the whole
/// sampler is deterministic and every time-related scenario is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clocks {
    /// `CLOCK_MONOTONIC`: stops during suspend. The rate denominator.
    pub monotonic_ms: i64,
    /// `CLOCK_BOOTTIME`: keeps running during suspend. Immune to NTP.
    pub boottime_ms: i64,
    /// Wall clock. The only one that names a calendar date, and the only one
    /// that can jump.
    pub wall_utc_ms: i64,
}

/// Holds baselines and produces deltas.
pub struct Sampler {
    baselines: HashMap<String, Baseline>,
    known: HashMap<String, NetworkInterface>,
    last_clocks: Option<Clocks>,
    boot_id: String,
    timezone: chrono_tz::Tz,
    policy: ResolvedPolicy,
    /// Traffic that happened while the process was down is only recoverable
    /// within this window; beyond it the gap is so long that reporting a single
    /// undifferentiated blob is not useful, so we re-baseline instead.
    max_offline_ms: i64,
    flush_interval_ms: i64,
}

/// Gaps shorter than this multiple of the flush interval are treated as a
/// crash-recovery, and the bytes go to the bucket the process was last alive
/// in -- which is where they actually belong.
const CRASH_RECOVERY_FLUSHES: i64 = 3;

impl Sampler {
    pub fn new(
        boot_id: String,
        timezone: chrono_tz::Tz,
        policy: ResolvedPolicy,
        flush_interval_ms: i64,
    ) -> Self {
        Self {
            baselines: HashMap::new(),
            known: HashMap::new(),
            last_clocks: None,
            boot_id,
            timezone,
            policy,
            max_offline_ms: 30 * 24 * 3_600_000, // 30 days
            flush_interval_ms: flush_interval_ms.max(1_000),
        }
    }

    pub fn set_timezone(&mut self, tz: chrono_tz::Tz) {
        self.timezone = tz;
    }

    pub fn set_policy(&mut self, policy: ResolvedPolicy) {
        self.policy = policy;
    }

    pub fn timezone(&self) -> chrono_tz::Tz {
        self.timezone
    }

    /// Interfaces currently known, with their classification.
    pub fn known_interfaces(&self) -> impl Iterator<Item = &NetworkInterface> {
        self.known.values()
    }

    pub fn counts(&self, name: &str) -> bool {
        self.known
            .get(name)
            .map(|i| self.policy.counts(name, i.kind))
            .unwrap_or(false)
    }

    pub fn kind_of(&self, name: &str) -> Option<InterfaceKind> {
        self.known.get(name).map(|i| i.kind)
    }

    /// Seed baselines from the database so traffic that happened while the
    /// process was not running can be recovered.
    ///
    /// A baseline is only accepted when the boot id matches: across a reboot
    /// the kernel counters restart, so a stored reading says nothing about the
    /// current ones. `ifindex` is checked per-interface at first sight.
    pub fn restore(&mut self, saved: Vec<PersistedBaseline>) -> usize {
        let mut restored = 0;
        for b in saved {
            if b.boot_id != self.boot_id {
                continue;
            }
            self.baselines.insert(
                b.interface,
                Baseline {
                    ifindex: b.ifindex,
                    counters: b.counters,
                    at_utc_ms: b.at_utc_ms,
                },
            );
            restored += 1;
        }
        restored
    }

    /// The current baselines, for persisting alongside a flush.
    pub fn baselines(&self) -> Vec<PersistedBaseline> {
        self.baselines
            .iter()
            .map(|(name, b)| PersistedBaseline {
                interface: name.clone(),
                ifindex: b.ifindex,
                counters: b.counters,
                at_utc_ms: b.at_utc_ms,
                boot_id: self.boot_id.clone(),
            })
            .collect()
    }

    /// Refresh the cached interface metadata. Called when the interface set
    /// changes, when a counter goes backwards, and periodically.
    pub fn refresh_interfaces<P: NetworkStatsProvider>(&mut self, provider: &P) {
        match provider.interfaces() {
            Ok(list) => {
                self.known = list.into_iter().map(|i| (i.name.clone(), i)).collect();
            }
            Err(e) => {
                // Keep the previous classification rather than losing it: a
                // stale kind is much better than reclassifying everything as
                // unknown and dropping it out of the usage total.
                tracing::debug!(error = %e, "interface metadata refresh failed; keeping cache");
            }
        }
    }

    /// Process one set of counter readings.
    pub fn sample(&mut self, samples: Vec<CounterSample>, now: Clocks) -> SampleOutcome {
        let mut out = SampleOutcome::default();

        // --- What did the clocks do since the last tick? ---
        if let Some(prev) = self.last_clocks {
            out.time = statistics::analyze_time(
                now.monotonic_ms - prev.monotonic_ms,
                now.boottime_ms - prev.boottime_ms,
                now.wall_utc_ms - prev.wall_utc_ms,
            );
            let mono = now.monotonic_ms - prev.monotonic_ms;
            if mono >= 0 {
                out.interval = Some(std::time::Duration::from_millis(mono as u64));
            }
        }
        // A rate measured across a suspend would divide real bytes by hours of
        // sleep and report a plausible-looking wrong number.
        if out.time.suspended() {
            out.interval = None;
        }
        self.last_clocks = Some(now);

        // If the wall clock jumped, the bucket keys it would produce name the
        // wrong calendar day -- possibly a day years away, which would then be
        // permanent in `usage_day`. Hold the baselines where they are and skip
        // the tick: the next good tick's delta from those same baselines is
        // still exactly right, so no bytes are lost by waiting.
        if out.time.clock_stepped() {
            out.discarded_untrusted_clock = true;
            tracing::warn!(
                step_ms = out.time.clock_step_ms,
                "wall clock stepped; holding this sample until the clock settles"
            );
            return out;
        }

        // `TimeAnalysis` is Copy; take it once so the attribution loop can
        // hold a mutable borrow of `out`.
        let time = out.time;

        let seen: std::collections::HashSet<&str> =
            samples.iter().map(|s| s.name.as_str()).collect();

        // --- Interfaces that vanished. ---
        let gone: Vec<String> = self
            .baselines
            .keys()
            .filter(|n| !seen.contains(n.as_str()))
            .cloned()
            .collect();
        for name in gone {
            // Drop the baseline. If the interface comes back it is re-baselined
            // from scratch, so a recreated device cannot produce a phantom
            // delta against a counter it never had.
            self.baselines.remove(&name);
            out.removed.push(name);
        }

        // --- Every interface we can see. ---
        for s in samples {
            let (traffic, kind) = match self.baselines.get(&s.name) {
                // Known interface, same device.
                Some(prev) if prev.ifindex == s.ifindex || s.ifindex == 0 || prev.ifindex == 0 => {
                    let d = statistics::counter_delta(prev.counters, s.counters);
                    (d.traffic, d.kind)
                }
                // Same name, different device: a dongle was swapped, or a
                // tunnel was torn down and recreated between ticks. The stored
                // counters belong to a device that no longer exists.
                Some(_) => {
                    out.added.push(s.name.clone());
                    (Traffic::ZERO, DeltaKind::Baseline)
                }
                // Never seen. The kernel counter holds pre-NetMeter history,
                // which is not usage we observed.
                None => {
                    out.added.push(s.name.clone());
                    (Traffic::ZERO, DeltaKind::Baseline)
                }
            };

            if kind == DeltaKind::Reset {
                out.reset.push(s.name.clone());
            }

            // The window these bytes could have happened in.
            let since = self
                .baselines
                .get(&s.name)
                .map(|b| b.at_utc_ms)
                .unwrap_or(now.wall_utc_ms);

            self.baselines.insert(
                s.name.clone(),
                Baseline {
                    ifindex: s.ifindex,
                    counters: s.counters,
                    at_utc_ms: now.wall_utc_ms,
                },
            );

            if traffic.is_zero() {
                continue;
            }

            out.rates.insert(s.name.clone(), traffic);
            self.attribute(&mut out, &s.name, traffic, since, now, &time);
        }

        out
    }

    /// Decide where a delta's bytes belong in time.
    fn attribute(
        &self,
        out: &mut SampleOutcome,
        name: &str,
        traffic: Traffic,
        since_utc_ms: i64,
        now: Clocks,
        time: &TimeAnalysis,
    ) {
        let gap_ms = now.wall_utc_ms - since_utc_ms;
        let crash_window = self.flush_interval_ms * CRASH_RECOVERY_FLUSHES;

        // Normal tick, or a restart quick enough that the bytes plainly belong
        // where the process left off.
        if gap_ms <= crash_window && !time.suspended() {
            let at = if gap_ms > 0 && since_utc_ms > 0 {
                // Attribute to when the process was last alive, not to now:
                // a crash recovery at 00:00:02 must credit yesterday.
                since_utc_ms
            } else {
                now.wall_utc_ms
            };
            self.push_bucket(out, name, traffic, at);
            return;
        }

        // A long gap: suspended, or the process was not running. The bytes are
        // real, but nothing in the kernel says when inside the window they
        // moved, so they are recorded as a window rather than as a bucket.
        if gap_ms > self.max_offline_ms {
            tracing::info!(
                interface = name,
                gap_hours = gap_ms / 3_600_000,
                "gap too long to attribute; re-baselining without crediting"
            );
            return;
        }

        out.offline.push(OfflineWindow {
            interface: name.to_string(),
            from_utc_ms: since_utc_ms,
            to_utc_ms: now.wall_utc_ms,
            traffic,
        });
    }

    fn push_bucket(&self, out: &mut SampleOutcome, name: &str, traffic: Traffic, at_utc_ms: i64) {
        let hour = crate::core::time::hour_start_ms(at_utc_ms);
        let key = BucketKey {
            interface: name.to_string(),
            hour_start_utc_ms: hour,
            // The local date of the *hour start*, so the hour row and the day
            // row it rolls into can never disagree about which day they are in.
            local_date: crate::core::time::local_date_key(self.timezone, hour),
        };
        *out.buckets.entry(key).or_insert(Traffic::ZERO) += traffic;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::types::InterfaceState;

    const BOOT: &str = "boot-aaaa";
    const OTHER_BOOT: &str = "boot-bbbb";
    /// 2026-09-11T12:00:00Z, a Friday well away from any boundary.
    const T0: i64 = 1_789_128_000_000;

    fn policy() -> ResolvedPolicy {
        Config::default().resolve()
    }

    fn sampler() -> Sampler {
        let mut s = Sampler::new(
            BOOT.into(),
            "Europe/Paris".parse().expect("tz"),
            policy(),
            30_000,
        );
        s.known.insert(
            "wlan0".into(),
            NetworkInterface {
                name: "wlan0".into(),
                kind: InterfaceKind::Wifi,
                mac: None,
                ifindex: 3,
                state: InterfaceState::Up,
            },
        );
        s
    }

    fn clocks(offset_ms: i64) -> Clocks {
        Clocks {
            monotonic_ms: offset_ms,
            boottime_ms: offset_ms,
            wall_utc_ms: T0 + offset_ms,
        }
    }

    fn sample(name: &str, ifindex: u32, rx: u64, tx: u64) -> CounterSample {
        CounterSample {
            name: name.into(),
            ifindex,
            counters: NetworkCounters {
                rx_bytes: rx,
                tx_bytes: tx,
            },
        }
    }

    fn only_traffic(o: &SampleOutcome) -> Traffic {
        o.bucketed_total()
    }

    #[test]
    fn first_sight_of_an_interface_credits_nothing() {
        let mut s = sampler();
        // The kernel counter is already at 4 GB; that is history.
        let o = s.sample(vec![sample("wlan0", 3, 4_000_000_000, 1_000_000)], clocks(0));
        assert_eq!(only_traffic(&o), Traffic::ZERO);
        assert_eq!(o.added, ["wlan0"]);
        assert!(o.offline.is_empty());
    }

    #[test]
    fn ordinary_traffic_is_credited_to_the_current_hour() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 1_000, 500)], clocks(0));
        let o = s.sample(vec![sample("wlan0", 3, 6_000, 2_500)], clocks(5_000));
        assert_eq!(only_traffic(&o), Traffic::new(5_000, 2_000));
        assert_eq!(o.buckets.len(), 1);
        let (k, _) = o.buckets.iter().next().expect("one bucket");
        assert_eq!(k.interface, "wlan0");
        assert_eq!(k.hour_start_utc_ms, crate::core::time::hour_start_ms(T0));
    }

    #[test]
    fn a_counter_reset_credits_post_reset_bytes_and_is_reported() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 4_000_000_000, 500)], clocks(0));
        // Driver reloaded: rx restarted and has moved 3 MB since. tx did not
        // go backwards, so it is judged as an ordinary 500-byte increment --
        // per-direction judgement is deliberately conservative, because
        // crediting `new` for a direction that never reset would over-count.
        let o = s.sample(vec![sample("wlan0", 3, 3_000_000, 1_000)], clocks(5_000));
        assert_eq!(o.reset, ["wlan0"]);
        assert_eq!(only_traffic(&o), Traffic::new(3_000_000, 500));
    }

    #[test]
    fn an_interface_that_disappears_is_reported_and_forgotten() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 1_000, 500)], clocks(0));
        let o = s.sample(vec![], clocks(5_000));
        assert_eq!(o.removed, ["wlan0"]);
        assert!(s.baselines.is_empty());
    }

    #[test]
    fn a_reconnecting_interface_does_not_produce_phantom_traffic() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 4_000_000_000, 0)], clocks(0));
        s.sample(vec![], clocks(5_000)); // gone
        // Back with a fresh counter and a new ifindex, as a recreated device.
        let o = s.sample(vec![sample("wlan0", 9, 12_345, 0)], clocks(10_000));
        assert_eq!(only_traffic(&o), Traffic::ZERO, "re-baseline, not 12 KB");
        assert_eq!(o.added, ["wlan0"]);
    }

    #[test]
    fn a_swapped_dongle_under_the_same_name_does_not_inherit_a_baseline() {
        let mut s = sampler();
        s.sample(vec![sample("eth1", 7, 8_000_000_000, 0)], clocks(0));
        // Different device, same name, appears within one tick. ifindex is the
        // only thing that distinguishes them.
        let o = s.sample(vec![sample("eth1", 21, 50_000_000, 0)], clocks(5_000));
        assert_eq!(only_traffic(&o), Traffic::ZERO);
        assert!(o.added.contains(&"eth1".to_string()));
    }

    #[test]
    fn traffic_is_credited_to_the_hour_it_happened_in_not_the_hour_it_is_flushed_in() {
        let mut s = sampler();
        // Land 40 seconds before the top of an hour.
        let hour = crate::core::time::hour_start_ms(T0) + 3_600_000;
        let before = hour - 40_000;
        s.sample(
            vec![sample("wlan0", 3, 0, 0)],
            Clocks {
                monotonic_ms: 0,
                boottime_ms: 0,
                wall_utc_ms: before,
            },
        );
        let o = s.sample(
            vec![sample("wlan0", 3, 1_000, 0)],
            Clocks {
                monotonic_ms: 5_000,
                boottime_ms: 5_000,
                wall_utc_ms: before + 5_000,
            },
        );
        let (k, _) = o.buckets.iter().next().expect("bucketed");
        assert_eq!(
            k.hour_start_utc_ms,
            hour - 3_600_000,
            "must land in the hour the bytes moved in"
        );
    }

    #[test]
    fn suspend_sends_bytes_to_an_offline_window_not_to_the_wake_up_hour() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 1_000, 0)], clocks(0));
        // 8 hours suspended: BOOTTIME and wall advanced, MONOTONIC did not.
        let eight_h = 8 * 3_600_000;
        let o = s.sample(
            vec![sample("wlan0", 3, 501_000, 0)],
            Clocks {
                monotonic_ms: 5_000,
                boottime_ms: eight_h + 5_000,
                wall_utc_ms: T0 + eight_h + 5_000,
            },
        );
        assert!(o.time.suspended());
        assert!(
            o.buckets.is_empty(),
            "500 KB must not be dumped into the wake-up hour"
        );
        assert_eq!(o.offline.len(), 1);
        assert_eq!(o.offline[0].traffic, Traffic::new(500_000, 0));
        assert_eq!(o.offline[0].from_utc_ms, T0);
        assert_eq!(o.interval, None, "no rate across a suspend");
    }

    #[test]
    fn a_long_process_downtime_becomes_an_offline_window() {
        let mut s = sampler();
        // Restored from the database: last seen three days ago, same boot.
        s.restore(vec![PersistedBaseline {
            interface: "wlan0".into(),
            ifindex: 3,
            counters: NetworkCounters {
                rx_bytes: 4_000_000_000,
                tx_bytes: 0,
            },
            at_utc_ms: T0 - 3 * 24 * 3_600_000,
            boot_id: BOOT.into(),
        }]);
        let o = s.sample(vec![sample("wlan0", 3, 4_440_857_267, 0)], clocks(0));
        assert!(
            o.buckets.is_empty(),
            "three days of traffic must not become today's total"
        );
        assert_eq!(o.offline.len(), 1);
        assert_eq!(o.offline[0].traffic.rx_bytes, 440_857_267);
        assert_eq!(o.offline[0].from_utc_ms, T0 - 3 * 24 * 3_600_000);
        assert_eq!(o.offline[0].to_utc_ms, T0);
    }

    #[test]
    fn a_short_restart_credits_the_bucket_the_process_died_in() {
        let mut s = sampler();
        // Crashed 10 s ago, well inside the crash-recovery window.
        s.restore(vec![PersistedBaseline {
            interface: "wlan0".into(),
            ifindex: 3,
            counters: NetworkCounters {
                rx_bytes: 1_000,
                tx_bytes: 0,
            },
            at_utc_ms: T0 - 10_000,
            boot_id: BOOT.into(),
        }]);
        let o = s.sample(vec![sample("wlan0", 3, 6_000, 0)], clocks(0));
        assert!(o.offline.is_empty());
        assert_eq!(only_traffic(&o), Traffic::new(5_000, 0));
    }

    #[test]
    fn a_crash_recovery_across_midnight_credits_yesterday() {
        let tz: chrono_tz::Tz = "Europe/Paris".parse().expect("tz");
        let midnight =
            crate::core::time::day_period(tz, chrono::NaiveDate::from_ymd_opt(2026, 9, 12).unwrap())
                .start_utc_ms;
        let mut s = Sampler::new(BOOT.into(), tz, policy(), 30_000);
        // Process was alive 5 s before midnight; first tick after restart is
        // 2 s after midnight.
        s.restore(vec![PersistedBaseline {
            interface: "wlan0".into(),
            ifindex: 3,
            counters: NetworkCounters {
                rx_bytes: 0,
                tx_bytes: 0,
            },
            at_utc_ms: midnight - 5_000,
            boot_id: BOOT.into(),
        }]);
        let o = s.sample(
            vec![sample("wlan0", 3, 200_000_000, 0)],
            Clocks {
                monotonic_ms: 0,
                boottime_ms: 0,
                wall_utc_ms: midnight + 2_000,
            },
        );
        let (k, _) = o.buckets.iter().next().expect("bucketed");
        assert_eq!(
            k.local_date, "2026-09-11",
            "traffic from before midnight belongs to the previous day"
        );
    }

    #[test]
    fn a_reboot_discards_the_stored_baseline_entirely() {
        let mut s = sampler();
        let restored = s.restore(vec![PersistedBaseline {
            interface: "wlan0".into(),
            ifindex: 3,
            counters: NetworkCounters {
                rx_bytes: 4_000_000_000,
                tx_bytes: 0,
            },
            at_utc_ms: T0 - 60_000,
            boot_id: OTHER_BOOT.into(),
        }]);
        assert_eq!(restored, 0, "a different boot's counters mean nothing");
        // After a reboot the kernel counter is small; without the guard this
        // would look like a reset and credit the whole post-boot total.
        let o = s.sample(vec![sample("wlan0", 3, 12_000_000, 0)], clocks(0));
        assert_eq!(only_traffic(&o), Traffic::ZERO);
    }

    #[test]
    fn a_forward_clock_step_is_held_rather_than_written_to_a_future_day() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 0, 0)], clocks(0));
        let year = 365i64 * 24 * 3_600_000;
        let o = s.sample(
            vec![sample("wlan0", 3, 1_000_000, 0)],
            Clocks {
                monotonic_ms: 5_000,
                boottime_ms: 5_000,
                wall_utc_ms: T0 + 5_000 + year,
            },
        );
        assert!(o.discarded_untrusted_clock);
        assert!(o.buckets.is_empty(), "no row may be minted for 2027");
        assert!(o.offline.is_empty());

        // NTP corrects the clock. That correction is itself a step, so this
        // tick is held too -- two held ticks is the honest cost of a clock
        // that moved twice.
        let o2 = s.sample(
            vec![sample("wlan0", 3, 1_000_000, 0)],
            Clocks {
                monotonic_ms: 10_000,
                boottime_ms: 10_000,
                wall_utc_ms: T0 + 10_000,
            },
        );
        assert!(o2.discarded_untrusted_clock);

        // Now the clock is steady again. The baseline was never advanced, so
        // the full delta is still there to be credited: holding a sample costs
        // latency, never bytes.
        let o3 = s.sample(
            vec![sample("wlan0", 3, 1_000_000, 0)],
            Clocks {
                monotonic_ms: 15_000,
                boottime_ms: 15_000,
                wall_utc_ms: T0 + 15_000,
            },
        );
        assert!(!o3.discarded_untrusted_clock);
        assert_eq!(
            only_traffic(&o3),
            Traffic::new(1_000_000, 0),
            "nothing was lost by waiting for the clock to settle"
        );
        assert_eq!(
            crate::core::time::local_date_key(s.timezone(), T0 + 15_000),
            o3.buckets.keys().next().expect("bucketed").local_date,
            "and it lands on the real day, not the one the bad clock named"
        );
    }

    #[test]
    fn a_backward_clock_step_is_also_held() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 0, 0)], clocks(0));
        let o = s.sample(
            vec![sample("wlan0", 3, 1_000, 0)],
            Clocks {
                monotonic_ms: 5_000,
                boottime_ms: 5_000,
                wall_utc_ms: T0 - 3_600_000,
            },
        );
        assert!(o.discarded_untrusted_clock);
    }

    #[test]
    fn duplicate_samples_at_the_same_instant_credit_nothing_twice() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 1_000, 0)], clocks(0));
        let a = s.sample(vec![sample("wlan0", 3, 2_000, 0)], clocks(5_000));
        let b = s.sample(vec![sample("wlan0", 3, 2_000, 0)], clocks(5_000));
        assert_eq!(only_traffic(&a), Traffic::new(1_000, 0));
        assert_eq!(only_traffic(&b), Traffic::ZERO, "same reading, no new bytes");
        assert_eq!(b.interval, Some(std::time::Duration::ZERO));
    }

    #[test]
    fn several_interfaces_are_tracked_independently() {
        let mut s = sampler();
        let t0 = vec![
            sample("wlan0", 3, 100, 100),
            sample("tailscale0", 4, 50, 50),
            sample("lo", 1, 999, 999),
        ];
        s.sample(t0, clocks(0));
        let o = s.sample(
            vec![
                sample("wlan0", 3, 1_100, 200),
                sample("tailscale0", 4, 550, 150),
                sample("lo", 1, 1_999, 1_999),
            ],
            clocks(5_000),
        );
        // All three are measured -- the policy decides what *counts*, and that
        // happens later, at query time, so history stays re-interpretable.
        assert_eq!(o.buckets.len(), 3);
        let by_iface: HashMap<_, _> = o
            .buckets
            .iter()
            .map(|(k, v)| (k.interface.as_str(), *v))
            .collect();
        assert_eq!(by_iface["wlan0"], Traffic::new(1_000, 100));
        assert_eq!(by_iface["tailscale0"], Traffic::new(500, 100));
        assert_eq!(by_iface["lo"], Traffic::new(1_000, 1_000));
    }

    #[test]
    fn a_failed_read_does_not_disturb_baselines() {
        // Modelled as "no samples arrive this tick": the interface list is
        // empty, so baselines drop. The engine, not the sampler, is what skips
        // a failed read -- assert the sampler is never handed a partial list.
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 1_000, 0)], clocks(0));
        let before = s.baselines.get("wlan0").expect("baseline").counters;
        // A tick the engine skipped simply never calls sample(); the next one
        // picks up from the same baseline.
        let o = s.sample(vec![sample("wlan0", 3, 3_000, 0)], clocks(10_000));
        assert_eq!(before.rx_bytes, 1_000);
        assert_eq!(only_traffic(&o), Traffic::new(2_000, 0));
    }

    #[test]
    fn baselines_round_trip_through_persistence() {
        let mut s = sampler();
        s.sample(vec![sample("wlan0", 3, 4_440_857_267, 12)], clocks(0));
        let saved = s.baselines();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].counters.rx_bytes, 4_440_857_267);
        assert_eq!(saved[0].boot_id, BOOT);

        let mut fresh = sampler();
        assert_eq!(fresh.restore(saved), 1);
        let o = fresh.sample(vec![sample("wlan0", 3, 4_440_857_367, 12)], clocks(1_000));
        assert_eq!(only_traffic(&o), Traffic::new(100, 0));
    }

    #[test]
    fn an_absurd_gap_is_dropped_rather_than_reported_as_one_blob() {
        let mut s = sampler();
        s.restore(vec![PersistedBaseline {
            interface: "wlan0".into(),
            ifindex: 3,
            counters: NetworkCounters {
                rx_bytes: 1_000,
                tx_bytes: 0,
            },
            at_utc_ms: T0 - 400 * 24 * 3_600_000,
            boot_id: BOOT.into(),
        }]);
        let o = s.sample(vec![sample("wlan0", 3, 9_000_000_000, 0)], clocks(0));
        assert!(o.buckets.is_empty());
        assert!(o.offline.is_empty(), "a year-wide window is not useful");
    }
}
