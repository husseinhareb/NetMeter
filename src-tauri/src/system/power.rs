//! Clock sources and what they say about what the machine did.
//!
//! Three clocks, because no one of them can answer the question alone:
//!
//! | clock | advances while suspended | affected by NTP |
//! |---|---|---|
//! | `CLOCK_MONOTONIC` | no  | no  |
//! | `CLOCK_BOOTTIME`  | yes | no  |
//! | wall clock        | yes | yes |
//!
//! So `BOOTTIME - MONOTONIC` is exactly the time spent suspended, and
//! `wall - BOOTTIME` is exactly how far the wall clock was stepped. Both are
//! measured, not inferred from a threshold on a single clock.
//!
//! This also avoids resting correctness on `std::time::Instant`, whose
//! documentation explicitly declines to specify whether suspend counts as
//! elapsed time -- it happens to be `CLOCK_MONOTONIC` on Linux today, but a
//! usage meter should not depend on that staying true.

use crate::monitor::sampling::Clocks;

/// Read a POSIX clock in milliseconds.
///
/// Returns `None` if the clock is unavailable, which callers treat as "fall
/// back to the clock we do have" rather than as a failure.
fn clock_ms(id: libc::clockid_t) -> Option<i64> {
    // SAFETY: `ts` is a plain POD struct that `clock_gettime` fully
    // initializes on success; we only read it when the call returns 0.
    let mut ts = unsafe { std::mem::zeroed::<libc::timespec>() };
    let rc = unsafe { libc::clock_gettime(id, &mut ts) };
    if rc != 0 {
        return None;
    }
    Some((ts.tv_sec as i64).saturating_mul(1_000) + (ts.tv_nsec as i64) / 1_000_000)
}

/// `CLOCK_MONOTONIC`: stops during suspend. The correct rate denominator,
/// because a rate should be bytes per second of *awake* time.
pub fn monotonic_ms() -> i64 {
    clock_ms(libc::CLOCK_MONOTONIC).unwrap_or(0)
}

/// `CLOCK_BOOTTIME`: includes suspended time, immune to NTP.
pub fn boottime_ms() -> i64 {
    // Falling back to MONOTONIC means suspends stop being detectable, but
    // nothing else breaks: the wall-vs-boottime comparison then simply sees a
    // suspend as a clock step, and a clock step is already handled safely.
    clock_ms(libc::CLOCK_BOOTTIME).unwrap_or_else(monotonic_ms)
}

/// Read all three clocks at as close to the same instant as possible.
pub fn now() -> Clocks {
    Clocks {
        monotonic_ms: monotonic_ms(),
        boottime_ms: boottime_ms(),
        wall_utc_ms: crate::core::time::now_utc_ms(),
    }
}

/// The kernel's boot identifier.
///
/// Used to decide whether a stored counter baseline still describes the
/// counters the kernel currently has: across a reboot they restart from zero,
/// so a stored reading is meaningless. An unreadable boot id yields a value
/// that compares unequal to anything stored, which skips carry-over -- the safe
/// direction.
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            tracing::warn!("boot_id unavailable; counter baselines will not be carried over");
            "unknown".to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boottime_never_runs_behind_monotonic() {
        // The invariant the suspend detector is built on.
        let m = monotonic_ms();
        let b = boottime_ms();
        assert!(b >= m, "BOOTTIME {b} < MONOTONIC {m}");
    }

    #[test]
    fn clocks_advance() {
        let a = now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = now();
        assert!(b.monotonic_ms >= a.monotonic_ms);
        assert!(b.boottime_ms >= a.boottime_ms);
        assert!(b.wall_utc_ms >= a.wall_utc_ms);
    }

    #[test]
    fn a_quiet_interval_shows_neither_suspend_nor_step() {
        let a = now();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let b = now();
        let t = crate::core::statistics::analyze_time(
            b.monotonic_ms - a.monotonic_ms,
            b.boottime_ms - a.boottime_ms,
            b.wall_utc_ms - a.wall_utc_ms,
        );
        assert!(!t.is_anomalous(), "a 30 ms sleep must look ordinary: {t:?}");
    }

    #[test]
    fn boot_id_is_stable_within_a_process() {
        let a = boot_id();
        assert!(!a.is_empty());
        assert_eq!(a, boot_id());
    }
}
