//! Pure, deterministic statistics. No I/O, no clocks, no database.
//!
//! Every accuracy rule NetMeter has lives in this file so it can be tested
//! exhaustively without simulating a machine.

use super::types::{DataRate, DeltaKind, NetworkCounters, Traffic, TrafficDelta};
use std::time::Duration;

/// Compare a new cumulative counter reading against the previous one.
///
/// Three cases, and the middle one is the one that is usually got wrong:
///
/// * the counter advanced -- credit the difference;
/// * the counter went *backwards* -- the device was destroyed and recreated, or
///   the driver reloaded, and the counter restarted at zero. Whatever accrued
///   before the reset is gone and unrecoverable, but `new` is not noise: it is
///   bytes this interface has genuinely moved since the reset, sitting right
///   there in the kernel's counter. Credit `new`. Crediting zero instead
///   (the intuitive "discard the sample" reading) silently under-counts every
///   suspend/resume and every dongle replug, on exactly the physical
///   interfaces the billable total is made of;
/// * the counter is unchanged -- zero bytes, which is not an error.
///
/// This never returns a negative value and never invents traffic: the result is
/// always bytes the kernel has attributed to this interface.
///
/// Wrap-around is deliberately *not* handled. `/proc/net/dev` is rendered from
/// `struct rtnl_link_stats64`, so the counters are 64-bit on every supported
/// kernel; wrapping 2^64 bytes would take longer than the heat death of a
/// laptop. Treating a decrease as a wrap instead of a reset would invent
/// petabytes of phantom traffic the first time an interface is recreated.
pub fn counter_delta(prev: NetworkCounters, new: NetworkCounters) -> TrafficDelta {
    // A reset is judged per-direction: a device recreation resets both, but
    // judging them independently means a single odd counter cannot drag the
    // other one's real delta down to zero.
    let (rx, rx_reset) = one_delta(prev.rx_bytes, new.rx_bytes);
    let (tx, tx_reset) = one_delta(prev.tx_bytes, new.tx_bytes);
    TrafficDelta {
        traffic: Traffic::new(rx, tx),
        kind: if rx_reset || tx_reset {
            DeltaKind::Reset
        } else {
            DeltaKind::Normal
        },
    }
}

fn one_delta(prev: u64, new: u64) -> (u64, bool) {
    if new >= prev {
        (new - prev, false)
    } else {
        (new, true)
    }
}

/// The delta for an interface seen for the first time.
///
/// The kernel's counter holds everything the interface moved before NetMeter
/// was watching -- possibly gigabytes from earlier in the boot. That is not
/// usage we observed, so it is recorded as a baseline and credited as zero.
pub fn first_observation() -> TrafficDelta {
    TrafficDelta {
        traffic: Traffic::ZERO,
        kind: DeltaKind::Baseline,
    }
}

/// Shortest interval we are willing to divide by. Two samples closer together
/// than this produce a rate dominated by timer jitter, and at exactly zero it
/// would be `f64::INFINITY` -- which `serde_json` serializes as a bare `null`,
/// so the frontend would receive a null in a field typed `number`.
const MIN_RATE_INTERVAL: Duration = Duration::from_millis(50);

/// Bytes per second over an interval, or `None` if the interval cannot support
/// a meaningful answer.
///
/// The interval must come from a *monotonic* clock. Deriving a rate from wall
/// time makes an NTP step look like a traffic spike.
pub fn rate(traffic: Traffic, elapsed: Duration) -> DataRate {
    if elapsed < MIN_RATE_INTERVAL {
        return DataRate::UNKNOWN;
    }
    let secs = elapsed.as_secs_f64();
    DataRate {
        rx_bytes_per_sec: Some(traffic.rx_bytes as f64 / secs),
        tx_bytes_per_sec: Some(traffic.tx_bytes as f64 / secs),
    }
}

/// Sum an iterator of traffic values, saturating rather than wrapping.
pub fn total<I: IntoIterator<Item = Traffic>>(items: I) -> Traffic {
    items.into_iter().sum()
}

/// How the three clocks read between two ticks, and what that says about what
/// the machine did in between.
///
/// * `CLOCK_MONOTONIC` stops during suspend on Linux.
/// * `CLOCK_BOOTTIME` keeps running during suspend.
/// * The wall clock can be stepped at any moment by NTP or by the user.
///
/// So the two differences below isolate the two events exactly, with no
/// threshold-guessing and without depending on `Instant`'s suspend behaviour,
/// which `std` explicitly declines to specify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeAnalysis {
    /// Milliseconds the machine spent suspended between the two ticks.
    pub suspended_ms: i64,
    /// Milliseconds the wall clock jumped, beyond the time that actually
    /// elapsed. Negative means it was stepped backwards.
    pub clock_step_ms: i64,
}

impl TimeAnalysis {
    /// True when the machine slept long enough that the bytes we are about to
    /// credit did not happen "now".
    pub fn suspended(&self) -> bool {
        self.suspended_ms > SUSPEND_THRESHOLD_MS
    }

    /// True when the wall clock is not to be trusted for bucket keys.
    pub fn clock_stepped(&self) -> bool {
        self.clock_step_ms.abs() > CLOCK_STEP_THRESHOLD_MS
    }

    /// Either condition: the sample's timing is suspect even though its byte
    /// count is not.
    pub fn is_anomalous(&self) -> bool {
        self.suspended() || self.clock_stepped()
    }
}

/// Below this, the difference is scheduling noise, not a suspend.
const SUSPEND_THRESHOLD_MS: i64 = 2_000;
/// Below this, the difference is NTP slewing (which is gradual and harmless),
/// not a step.
const CLOCK_STEP_THRESHOLD_MS: i64 = 5_000;

/// Decompose the movement of the three clocks between two ticks.
pub fn analyze_time(
    monotonic_delta_ms: i64,
    boottime_delta_ms: i64,
    wall_delta_ms: i64,
) -> TimeAnalysis {
    TimeAnalysis {
        // BOOTTIME advances during suspend, MONOTONIC does not, so their
        // difference *is* the suspended duration. Exact, not inferred.
        suspended_ms: (boottime_delta_ms - monotonic_delta_ms).max(0),
        // BOOTTIME is immune to both suspend and NTP, so anything the wall
        // clock did beyond it was a step.
        clock_step_ms: wall_delta_ms - boottime_delta_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(rx: u64, tx: u64) -> NetworkCounters {
        NetworkCounters {
            rx_bytes: rx,
            tx_bytes: tx,
        }
    }

    #[test]
    fn normal_increment() {
        let d = counter_delta(c(1_000, 500), c(1_500, 900));
        assert_eq!(d.traffic, Traffic::new(500, 400));
        assert_eq!(d.kind, DeltaKind::Normal);
    }

    #[test]
    fn idle_interface_reports_zero_not_an_error() {
        let d = counter_delta(c(1_000, 500), c(1_000, 500));
        assert_eq!(d.traffic, Traffic::ZERO);
        assert_eq!(d.kind, DeltaKind::Normal);
    }

    #[test]
    fn first_observation_credits_nothing() {
        // The kernel counter is already at 4 GB when we first look; that is
        // history, not usage we measured.
        let d = first_observation();
        assert_eq!(d.traffic, Traffic::ZERO);
        assert_eq!(d.kind, DeltaKind::Baseline);
    }

    #[test]
    fn counter_reset_credits_post_reset_bytes_only() {
        // Device destroyed and recreated: counter restarted and has since
        // moved 3 MB. Those 3 MB are real and are on this interface.
        let d = counter_delta(c(4_440_857_267, 1_000_000_000), c(3_100_000, 512_000));
        assert_eq!(d.kind, DeltaKind::Reset);
        assert_eq!(d.traffic, Traffic::new(3_100_000, 512_000));
    }

    #[test]
    fn counter_reset_to_zero_credits_zero() {
        let d = counter_delta(c(9_999, 8_888), c(0, 0));
        assert_eq!(d.kind, DeltaKind::Reset);
        assert_eq!(d.traffic, Traffic::ZERO);
    }

    #[test]
    fn reset_in_one_direction_does_not_destroy_the_other() {
        // rx went backwards, tx advanced normally. The tx delta must survive.
        let d = counter_delta(c(1_000_000, 500), c(10, 900));
        assert_eq!(d.kind, DeltaKind::Reset);
        assert_eq!(d.traffic, Traffic::new(10, 400));
    }

    #[test]
    fn delta_is_never_negative_for_any_pair() {
        // Exhaustive over an awkward set including the 32-bit boundary, which
        // is where a "wrap correction" would previously have fired.
        let vals = [
            0u64,
            1,
            255,
            u32::MAX as u64 - 1,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            u64::MAX / 2,
            u64::MAX - 1,
            u64::MAX,
        ];
        for &a in &vals {
            for &b in &vals {
                let d = counter_delta(c(a, a), c(b, b));
                // No panic, no wrap, and never more than the kernel reported.
                assert!(d.traffic.rx_bytes <= b.max(b.saturating_sub(a)));
                if b >= a {
                    assert_eq!(d.traffic.rx_bytes, b - a);
                    assert_eq!(d.kind, DeltaKind::Normal);
                } else {
                    assert_eq!(d.traffic.rx_bytes, b);
                    assert_eq!(d.kind, DeltaKind::Reset);
                }
            }
        }
    }

    #[test]
    fn huge_counter_near_u64_max_does_not_overflow() {
        let d = counter_delta(c(u64::MAX - 10, 0), c(u64::MAX, 10));
        assert_eq!(d.traffic, Traffic::new(10, 10));
    }

    #[test]
    fn traffic_addition_saturates_instead_of_wrapping() {
        let a = Traffic::new(u64::MAX, u64::MAX);
        assert_eq!(a + Traffic::new(1, 1), a);
        assert_eq!(Traffic::new(u64::MAX, 0).total_bytes(), u64::MAX);
    }

    #[test]
    fn rate_is_none_for_a_zero_or_tiny_interval() {
        assert_eq!(rate(Traffic::new(1000, 0), Duration::ZERO), DataRate::UNKNOWN);
        assert_eq!(
            rate(Traffic::new(1000, 0), Duration::from_millis(49)),
            DataRate::UNKNOWN
        );
    }

    #[test]
    fn rate_is_bytes_per_second() {
        let r = rate(Traffic::new(10_000, 2_000), Duration::from_secs(5));
        assert_eq!(r.rx_bytes_per_sec, Some(2_000.0));
        assert_eq!(r.tx_bytes_per_sec, Some(400.0));
    }

    #[test]
    fn rate_is_always_finite_so_it_never_serializes_as_null() {
        let r = rate(Traffic::new(u64::MAX, u64::MAX), Duration::from_millis(50));
        assert!(r.rx_bytes_per_sec.expect("some").is_finite());
        assert!(r.tx_bytes_per_sec.expect("some").is_finite());
    }

    #[test]
    fn quiet_machine_has_no_time_anomaly() {
        let a = analyze_time(5_000, 5_000, 5_002);
        assert_eq!(a.suspended_ms, 0);
        assert!(!a.is_anomalous());
    }

    #[test]
    fn suspend_is_detected_exactly_and_not_confused_with_a_clock_step() {
        // 8 hours suspended: BOOTTIME and the wall clock both advanced 8h,
        // MONOTONIC only ticked the 5s the machine was awake.
        let eight_h = 8 * 3_600_000;
        let a = analyze_time(5_000, eight_h + 5_000, eight_h + 5_000);
        assert_eq!(a.suspended_ms, eight_h);
        assert!(a.suspended());
        assert!(!a.clock_stepped(), "a suspend is not a clock step");
    }

    #[test]
    fn ntp_step_is_detected_and_not_confused_with_a_suspend() {
        // Wall clock jumped forward a year; nothing else moved.
        let year = 365i64 * 24 * 3_600_000;
        let a = analyze_time(5_000, 5_000, 5_000 + year);
        assert_eq!(a.suspended_ms, 0);
        assert!(!a.suspended());
        assert!(a.clock_stepped());
        assert_eq!(a.clock_step_ms, year);
    }

    #[test]
    fn backward_clock_step_is_detected() {
        let a = analyze_time(5_000, 5_000, -3_600_000);
        assert!(a.clock_stepped());
        assert!(a.clock_step_ms < 0);
        assert_eq!(a.suspended_ms, 0, "a backward step is not a suspend");
    }

    #[test]
    fn suspend_and_clock_step_together_are_both_reported() {
        let hour = 3_600_000;
        // Suspended an hour, and the clock was also stepped 10 minutes forward.
        let a = analyze_time(5_000, hour + 5_000, hour + 5_000 + 600_000);
        assert_eq!(a.suspended_ms, hour);
        assert_eq!(a.clock_step_ms, 600_000);
        assert!(a.suspended() && a.clock_stepped());
    }

    #[test]
    fn ntp_slew_is_not_treated_as_a_step() {
        // Slewing adjusts by a few ms per tick; that must not flag an anomaly.
        let a = analyze_time(5_000, 5_000, 5_120);
        assert!(!a.is_anomalous());
    }

    #[test]
    fn totals_sum_correctly() {
        let t = total([Traffic::new(1, 2), Traffic::new(10, 20), Traffic::ZERO]);
        assert_eq!(t, Traffic::new(11, 22));
        assert_eq!(total([]), Traffic::ZERO);
    }
}
