//! Configuration, with defaults that make NetMeter correct out of the box.
//!
//! The config lives in a JSON file rather than in the database, because it
//! contains `database_path` -- a setting that cannot be stored in the thing it
//! locates.

use super::errors::ConfigError;
use super::types::InterfaceKind;
use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which interfaces count toward the user's usage total.
///
/// The distinction this type exists to draw: NetMeter *observes* every
/// interface, but only *counts* the ones whose bytes are not already counted
/// somewhere else. See `docs/ACCOUNTING.md` for the full table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfacePolicy {
    /// Count physical interfaces (Ethernet, WiFi, WWAN) and nothing else.
    ///
    /// This is the default and it is the only setting that cannot double-count:
    /// a VPN tunnel's payload is re-counted on the NIC carrying the encrypted
    /// packets, and a container's traffic is counted again on the veth, the
    /// bridge, and the NIC.
    #[default]
    PhysicalOnly,
    /// Count exactly the interfaces matching `include`, minus `exclude`. For
    /// the user on a metered hotspot behind a VPN who wants the tunnel counted
    /// instead of the carrier.
    Manual,
    /// Count everything except `exclude`. Documented as double-counting; here
    /// because an escape hatch that cannot be reached is not an escape hatch.
    AllExcept,
}

/// How long each resolution of history is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Hour-resolution rows. Bounded, because this is the tier that grows:
    /// 24 rows per interface per day.
    pub hourly_days: u32,
    /// Day-resolution rows. `0` means keep forever, which is the default: a
    /// day row is ~40 bytes, so a decade of four interfaces is under a
    /// megabyte, and "how much did I use in 2021" is the kind of question this
    /// app exists to answer.
    pub daily_days: u32,
    /// Interfaces not seen for this long, and holding no usage history, are
    /// forgotten. This is what stops Docker's random `vethXXXXXXX` names from
    /// accumulating forever.
    pub interface_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            hourly_days: 400, // > 1 year, so "this hour last year" still works
            daily_days: 0,    // forever
            interface_days: 90,
        }
    }
}

/// All tunable behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// How often the kernel counters are read. Every tick is one `read()` of a
    /// ~1 KB pseudo-file; the cost of a shorter interval is wake-ups, not I/O.
    pub sampling_interval_seconds: u32,
    /// How often accumulated deltas are written to SQLite. Larger means fewer
    /// disk writes and less SSD wear; it does **not** cost accuracy, because
    /// buckets are keyed at sample time, and it does not risk data loss within
    /// a boot, because the kernel's cumulative counter lets a restart re-derive
    /// anything that was still in memory.
    pub flush_interval_seconds: u32,
    /// `None` uses the app data directory.
    pub database_path: Option<PathBuf>,
    /// IANA name, e.g. `"Europe/Paris"`. `None` follows the system.
    ///
    /// Changing this re-interprets stored day rows rather than rewriting them:
    /// history recorded while in Paris keeps its Paris day boundaries.
    pub timezone: Option<String>,
    pub interface_policy: InterfacePolicy,
    /// Glob patterns of interfaces to count. Only consulted by
    /// [`InterfacePolicy::Manual`].
    pub included_interfaces: Vec<String>,
    /// Glob patterns to never count, applied under every policy.
    pub excluded_interfaces: Vec<String>,
    pub retention: RetentionPolicy,
    /// `tracing` filter, e.g. `"info"` or `"netmeter=debug"`.
    pub logging_level: String,
    /// Emit at most one live-usage event per this many milliseconds. Coalescing
    /// matters because the webview pays a JSON deserialization for each one.
    pub event_interval_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sampling_interval_seconds: 5,
            flush_interval_seconds: 30,
            database_path: None,
            timezone: None,
            interface_policy: InterfacePolicy::default(),
            included_interfaces: Vec::new(),
            // Loopback is not network usage; the rest are belt-and-braces on
            // top of the classifier, for the case where a future interface
            // type classifies as physical but obviously should not count.
            excluded_interfaces: vec!["lo".into()],
            retention: RetentionPolicy::default(),
            logging_level: "info".into(),
            event_interval_ms: 1_000,
        }
    }
}

impl Config {
    pub const MIN_SAMPLING_SECONDS: u32 = 1;
    pub const MAX_SAMPLING_SECONDS: u32 = 3_600;
    pub const MIN_FLUSH_SECONDS: u32 = 1;
    pub const MAX_FLUSH_SECONDS: u32 = 3_600;

    /// Reject values that would make the engine misbehave, before they are
    /// persisted. Called on load and on every `set_config`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(Self::MIN_SAMPLING_SECONDS..=Self::MAX_SAMPLING_SECONDS)
            .contains(&self.sampling_interval_seconds)
        {
            return Err(ConfigError::OutOfRange {
                field: "sampling_interval_seconds",
                min: Self::MIN_SAMPLING_SECONDS as u64,
                max: Self::MAX_SAMPLING_SECONDS as u64,
                value: self.sampling_interval_seconds as u64,
            });
        }
        if !(Self::MIN_FLUSH_SECONDS..=Self::MAX_FLUSH_SECONDS).contains(&self.flush_interval_seconds)
        {
            return Err(ConfigError::OutOfRange {
                field: "flush_interval_seconds",
                min: Self::MIN_FLUSH_SECONDS as u64,
                max: Self::MAX_FLUSH_SECONDS as u64,
                value: self.flush_interval_seconds as u64,
            });
        }
        if let Some(tz) = &self.timezone {
            if tz.parse::<chrono_tz::Tz>().is_err() {
                return Err(ConfigError::Timezone(tz.clone()));
            }
        }
        // Compiling the globs here means a bad pattern is rejected at the API
        // boundary instead of silently matching nothing at runtime.
        for p in self.included_interfaces.iter().chain(&self.excluded_interfaces) {
            Glob::new(p).map_err(|e| ConfigError::Pattern {
                pattern: p.clone(),
                detail: e.to_string(),
            })?;
        }
        Ok(())
    }

    pub fn timezone_or_system(&self) -> chrono_tz::Tz {
        self.timezone
            .as_deref()
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(super::time::system_timezone)
    }

    pub fn sampling_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.sampling_interval_seconds.max(1) as u64)
    }

    pub fn flush_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.flush_interval_seconds.max(1) as u64)
    }

    /// Compile the patterns once, so the per-tick decision is a slice scan and
    /// not a regex build.
    pub fn resolve(&self) -> ResolvedPolicy {
        let compile = |pats: &[String]| -> Vec<GlobMatcher> {
            pats.iter()
                .filter_map(|p| Glob::new(p).ok().map(|g| g.compile_matcher()))
                .collect()
        };
        ResolvedPolicy {
            policy: self.interface_policy.clone(),
            include: compile(&self.included_interfaces),
            exclude: compile(&self.excluded_interfaces),
        }
    }
}

/// A [`Config`]'s interface rules with the globs already compiled.
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    policy: InterfacePolicy,
    include: Vec<GlobMatcher>,
    exclude: Vec<GlobMatcher>,
}

impl ResolvedPolicy {
    /// Whether an interface's bytes count toward the user's usage total.
    ///
    /// Exclusion always wins, under every policy, so `excluded_interfaces` is
    /// a rule the user can rely on rather than a hint.
    pub fn counts(&self, name: &str, kind: InterfaceKind) -> bool {
        if self.exclude.iter().any(|m| m.is_match(name)) {
            return false;
        }
        match self.policy {
            InterfacePolicy::PhysicalOnly => kind.is_physical(),
            InterfacePolicy::Manual => self.include.iter().any(|m| m.is_match(name)),
            // Loopback is never usage regardless of policy: those bytes never
            // touched a network.
            InterfacePolicy::AllExcept => kind != InterfaceKind::Loopback,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_count_only_physical_interfaces() {
        let c = Config::default();
        c.validate().expect("shipped defaults must be valid");
        let p = c.resolve();
        assert!(p.counts("wlp7s0", InterfaceKind::Wifi));
        assert!(p.counts("enp8s0", InterfaceKind::Ethernet));
        assert!(p.counts("wwan0", InterfaceKind::Wwan));
        // The double-counters, all off by default.
        assert!(!p.counts("tailscale0", InterfaceKind::Vpn));
        assert!(!p.counts("wg0", InterfaceKind::Vpn));
        assert!(!p.counts("docker0", InterfaceKind::Bridge));
        assert!(!p.counts("br-ee189c1b10a3", InterfaceKind::Bridge));
        assert!(!p.counts("vethed62bc1", InterfaceKind::Enslaved));
        assert!(!p.counts("eth0.100", InterfaceKind::Vlan));
        assert!(!p.counts("lo", InterfaceKind::Loopback));
        assert!(!p.counts("weird0", InterfaceKind::Virtual));
    }

    #[test]
    fn exclusion_overrides_every_policy() {
        for policy in [
            InterfacePolicy::PhysicalOnly,
            InterfacePolicy::Manual,
            InterfacePolicy::AllExcept,
        ] {
            let c = Config {
                interface_policy: policy.clone(),
                included_interfaces: vec!["wlp7s0".into()],
                excluded_interfaces: vec!["wlp*".into()],
                ..Config::default()
            };
            assert!(
                !c.resolve().counts("wlp7s0", InterfaceKind::Wifi),
                "exclude must win under {policy:?}"
            );
        }
    }

    #[test]
    fn manual_policy_counts_a_vpn_instead_of_the_carrier() {
        // The metered-hotspot-behind-a-VPN case from the accounting doc.
        let c = Config {
            interface_policy: InterfacePolicy::Manual,
            included_interfaces: vec!["wg0".into()],
            excluded_interfaces: vec![],
            ..Config::default()
        };
        let p = c.resolve();
        assert!(p.counts("wg0", InterfaceKind::Vpn));
        assert!(!p.counts("wlp7s0", InterfaceKind::Wifi), "carrier not counted");
    }

    #[test]
    fn manual_policy_with_no_includes_counts_nothing_rather_than_everything() {
        let c = Config {
            interface_policy: InterfacePolicy::Manual,
            included_interfaces: vec![],
            ..Config::default()
        };
        assert!(!c.resolve().counts("wlp7s0", InterfaceKind::Wifi));
    }

    #[test]
    fn all_except_still_refuses_loopback() {
        let c = Config {
            interface_policy: InterfacePolicy::AllExcept,
            excluded_interfaces: vec![],
            ..Config::default()
        };
        let p = c.resolve();
        assert!(p.counts("tailscale0", InterfaceKind::Vpn));
        assert!(
            !p.counts("lo", InterfaceKind::Loopback),
            "loopback bytes never touched a network"
        );
    }

    #[test]
    fn glob_patterns_match_prefixes() {
        let c = Config {
            excluded_interfaces: vec!["veth*".into(), "br-*".into()],
            interface_policy: InterfacePolicy::AllExcept,
            ..Config::default()
        };
        let p = c.resolve();
        assert!(!p.counts("vethed62bc1", InterfaceKind::Enslaved));
        assert!(!p.counts("br-ee189c1b10a3", InterfaceKind::Bridge));
        assert!(p.counts("enp8s0", InterfaceKind::Ethernet));
    }

    #[test]
    fn out_of_range_intervals_are_rejected() {
        for bad in [0, Config::MAX_SAMPLING_SECONDS + 1] {
            let c = Config {
                sampling_interval_seconds: bad,
                ..Config::default()
            };
            assert!(c.validate().is_err(), "{bad} must be rejected");
        }
        let c = Config {
            flush_interval_seconds: 0,
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn unknown_timezone_is_rejected_at_the_boundary() {
        let c = Config {
            timezone: Some("Mars/Olympus_Mons".into()),
            ..Config::default()
        };
        assert!(matches!(c.validate(), Err(ConfigError::Timezone(_))));

        let ok = Config {
            timezone: Some("Asia/Kolkata".into()),
            ..Config::default()
        };
        ok.validate().expect("real zone accepted");
        assert_eq!(ok.timezone_or_system().name(), "Asia/Kolkata");
    }

    #[test]
    fn malformed_glob_is_rejected_at_the_boundary() {
        let c = Config {
            excluded_interfaces: vec!["[".into()],
            ..Config::default()
        };
        assert!(matches!(c.validate(), Err(ConfigError::Pattern { .. })));
    }

    #[test]
    fn config_round_trips_through_json_and_tolerates_missing_fields() {
        let c = Config::default();
        let s = serde_json::to_string(&c).expect("serializes");
        assert_eq!(serde_json::from_str::<Config>(&s).expect("parses"), c);
        // A config file written by an older version must still load.
        let partial: Config = serde_json::from_str(r#"{"sampling_interval_seconds": 10}"#)
            .expect("unknown/missing fields fall back to defaults");
        assert_eq!(partial.sampling_interval_seconds, 10);
        assert_eq!(partial.flush_interval_seconds, c.flush_interval_seconds);
    }
}
