//! Tauri commands.
//!
//! The frontend reaches the backend only through this file. It never opens
//! SQLite, never reads `/proc/net/dev`, never reads `/sys/class/net`.
//!
//! Every command that touches the database is `async` and does its work on a
//! blocking thread: a synchronous `#[tauri::command]` runs on the main thread,
//! and rusqlite is blocking, so a slow query would freeze the window.

use crate::api::models::{
    ConfigApplied, InterfaceInfo, LiveRates, Scope, StatusResponse, UsageQuery, UsageResponse,
    UsageTotals,
};
use crate::core::config::Config;
use crate::core::errors::{Error, Result};
use crate::core::time;
use crate::core::types::{Granularity, InterfaceKind, MonitorStatus, UsagePeriod};
use crate::storage::aggregation::{build_series, SeriesInput};
use crate::storage::repository::{Resolution, UsageRow};
use crate::system::state::AppState;
use tauri::State;

/// Resolve a request's calendar keys into instants, rejecting anything that
/// does not parse rather than guessing at the user's intent.
fn resolve_range(
    tz: chrono_tz::Tz,
    q: &UsageQuery,
) -> Result<(UsagePeriod, UsagePeriod)> {
    let from = time::period_for_key(tz, &q.from)
        .ok_or_else(|| Error::BadRequest(format!("unparseable `from` key: {:?}", q.from)))?;
    let to = time::period_for_key(tz, &q.to)
        .ok_or_else(|| Error::BadRequest(format!("unparseable `to` key: {:?}", q.to)))?;
    // `<=` not `<`: for from="2026-09-11", to="2026-09-10" the end of `to` is
    // exactly the start of `from`, so a strict comparison would accept it and
    // silently return an empty series rather than saying what was wrong.
    if to.end_utc_ms <= from.start_utc_ms {
        return Err(Error::BadRequest(format!(
            "`from` ({}) is after `to` ({})",
            q.from, q.to
        )));
    }
    Ok((from, to))
}

/// The bytes still in memory, shaped like stored rows so the series builder can
/// treat both the same way. This is what keeps "Today" from freezing between
/// flushes and then jumping.
fn pending_rows(state: &AppState, tz: chrono_tz::Tz, granularity: Granularity) -> Vec<UsageRow> {
    let Some(snapshot) = state.snapshot() else {
        return Vec::new();
    };
    snapshot
        .pending
        .iter()
        .map(|b| UsageRow {
            bucket_key: match granularity {
                Granularity::Hour => time::bucket_key(tz, granularity, b.hour_start_utc_ms),
                g => b.local_date[..g.key_len().min(b.local_date.len())].to_string(),
            },
            // Not yet persisted, so it has no row id. Negative to guarantee it
            // cannot collide with a real one when merging a breakdown.
            interface_id: -1,
            interface_name: b.interface.clone(),
            kind: snapshot
                .interfaces
                .iter()
                .find(|i| i.name == b.interface)
                .map(|i| i.kind)
                .unwrap_or(InterfaceKind::Virtual),
            traffic: b.traffic,
        })
        .collect()
}

/// Run a usage query. The single command behind every historical view.
async fn query_usage(state: &AppState, q: UsageQuery) -> Result<UsageResponse> {
    let config = state.config();
    let tz = config.timezone_or_system();
    let (from, to) = resolve_range(tz, &q)?;

    let resolution = match q.granularity {
        Granularity::Hour => Resolution::Hour,
        _ => Resolution::Day,
    };

    // Day/month/year all read `usage_day`, differing only in how many leading
    // characters of the local date identify the bucket.
    let (from_key, to_key) = match resolution {
        Resolution::Hour => (from.start_utc_ms.to_string(), to.end_utc_ms.to_string()),
        Resolution::Day => (
            time::local_date_key(tz, from.start_utc_ms),
            time::local_date_key(tz, to.end_utc_ms - 1),
        ),
    };
    let key_len = q.granularity.key_len();

    let db = state.db_path();
    let (rows, offline, data_since) = tauri::async_runtime::spawn_blocking({
        let db = db.clone();
        let range = (from.start_utc_ms, to.end_utc_ms);
        move || -> Result<_> {
            let repo = ReadOnlyRepo::open(&db)?;
            let rows = repo.usage(resolution, &from_key, &to_key, key_len)?;
            let offline = repo.offline(range.0, range.1)?;
            let data_since = repo.earliest_local_date()?;
            Ok((rows, offline, data_since))
        }
    })
    .await
    .map_err(|e| Error::Internal(format!("query task failed: {e}")))??;

    // `Scope::All` is expressed by widening the policy, not by a second query:
    // the same rows answer both questions.
    let policy = match q.scope {
        Scope::Included => config.resolve(),
        Scope::All => Config {
            interface_policy: crate::core::config::InterfacePolicy::AllExcept,
            excluded_interfaces: vec![],
            ..(*config).clone()
        }
        .resolve(),
    };

    Ok(build_series(SeriesInput {
        granularity: q.granularity,
        timezone: tz,
        from,
        to,
        rows,
        offline,
        policy: &policy,
        include_breakdown: q.include_breakdown,
        data_since,
        pending: pending_rows(state, tz, q.granularity),
        generated_at_utc_ms: time::now_utc_ms(),
    }))
}

/// A read-only connection, opened per query.
///
/// Opening a connection to an existing WAL database is well under a
/// millisecond, and commands run at human rates, so this is cheaper than the
/// alternative: sharing the writer behind a mutex, where a read would queue
/// behind the flush and behind the daily prune.
struct ReadOnlyRepo {
    conn: rusqlite::Connection,
}

impl ReadOnlyRepo {
    fn open(path: &std::path::Path) -> Result<Self> {
        Ok(Self {
            conn: crate::storage::database::open_reader(path)?,
        })
    }

    fn earliest_local_date(&self) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT MIN(local_date) FROM usage_day", [], |r| r.get(0))
            .optional()
            .map_err(crate::core::errors::StorageError::from_sqlite)?
            .flatten())
    }
}

// Reads go through the same trait the writer implements, so a query cannot
// accidentally acquire write behaviour.
impl ReadOnlyRepo {
    fn usage(
        &self,
        resolution: Resolution,
        from: &str,
        to: &str,
        key_len: usize,
    ) -> Result<Vec<UsageRow>> {
        Ok(crate::storage::repository::query_usage(
            &self.conn, resolution, from, to, key_len,
        )?)
    }

    fn offline(
        &self,
        from: i64,
        to: i64,
    ) -> Result<Vec<crate::storage::repository::OfflineRow>> {
        Ok(crate::storage::repository::query_offline(&self.conn, from, to)?)
    }

    fn interfaces(&self) -> Result<Vec<crate::storage::repository::StoredInterface>> {
        Ok(crate::storage::repository::query_interfaces(&self.conn)?)
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Whether the monitor is sampling, and how healthily.
#[tauri::command]
pub async fn get_monitor_status(state: State<'_, AppState>) -> Result<StatusResponse> {
    Ok(state.status())
}

/// Start sampling. Idempotent: calling it twice does not spawn a second
/// sampler, which would double-count every byte.
#[tauri::command]
pub async fn start_monitor(state: State<'_, AppState>) -> Result<MonitorStatus> {
    state.start_monitor()?;
    Ok(state.status())
}

/// Stop sampling, flushing what is buffered before returning.
#[tauri::command]
pub async fn stop_monitor(state: State<'_, AppState>) -> Result<MonitorStatus> {
    state.stop_monitor();
    Ok(state.status())
}

/// Every interface NetMeter knows about, live or historical, each labelled with
/// whether it counts toward the usage total.
#[tauri::command]
pub async fn get_interfaces(state: State<'_, AppState>) -> Result<Vec<InterfaceInfo>> {
    let config = state.config();
    let policy = config.resolve();
    let db = state.db_path();

    let stored = tauri::async_runtime::spawn_blocking(move || -> Result<_> {
        ReadOnlyRepo::open(&db)?.interfaces()
    })
    .await
    .map_err(|e| Error::Internal(format!("query task failed: {e}")))??;

    let live = state.live_interfaces();

    let mut out: Vec<InterfaceInfo> = stored
        .into_iter()
        .map(|s| InterfaceInfo {
            included: policy.counts(&s.name, s.kind),
            present: live.contains_key(&s.name),
            id: Some(s.id),
            name: s.name,
            kind: s.kind,
            mac: s.mac,
            first_seen_utc_ms: Some(s.first_seen_utc_ms),
            last_seen_utc_ms: Some(s.last_seen_utc_ms),
        })
        .collect();

    // Interfaces seen this session but not yet flushed still belong in the
    // list, or a freshly connected NIC would be invisible until the next flush.
    for (name, seen) in live {
        if out.iter().any(|i| i.name == name) {
            continue;
        }
        out.push(InterfaceInfo {
            id: None,
            included: policy.counts(&name, seen.kind),
            name,
            kind: seen.kind,
            mac: seen.mac,
            present: true,
            first_seen_utc_ms: None,
            last_seen_utc_ms: None,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The live rate readout. Served from memory; touches no database.
///
/// The payload is byte-for-byte the `network-usage-updated` event, so the
/// frontend has one parser whether it polls or subscribes.
#[tauri::command]
pub async fn get_live_rates(state: State<'_, AppState>) -> Result<LiveRates> {
    Ok(state.live_rates())
}

/// The one historical query. Serves today, yesterday, last 7 days, last 30
/// days, a month, a year, a custom range and the per-interface breakdown.
#[tauri::command]
pub async fn get_usage_series(
    state: State<'_, AppState>,
    query: UsageQuery,
) -> Result<UsageResponse> {
    query_usage(&state, query).await
}

/// Totals for a period, without the per-bucket series.
#[tauri::command]
pub async fn get_usage_totals(
    state: State<'_, AppState>,
    query: UsageQuery,
) -> Result<UsageTotals> {
    let series = query_usage(&state, query).await?;
    Ok(UsageTotals::from(&series))
}

/// Usage so far in the current local day, including bytes not yet flushed.
#[tauri::command]
pub async fn get_today_usage(state: State<'_, AppState>) -> Result<UsageResponse> {
    let tz = state.config().timezone_or_system();
    let today = time::local_date_key(tz, time::now_utc_ms());
    query_usage(
        &state,
        UsageQuery {
            granularity: Granularity::Day,
            from: today.clone(),
            to: today,
            scope: Scope::Included,
            include_breakdown: true,
        },
    )
    .await
}

/// The current configuration.
#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<Config> {
    Ok((*state.config()).clone())
}

/// Replace the configuration.
///
/// Validated before it is written, applied to the running engine without a
/// restart, and echoed back with the resolved interface set so the UI never has
/// to re-implement glob matching.
#[tauri::command]
pub async fn set_config(state: State<'_, AppState>, config: Config) -> Result<ConfigApplied> {
    state.set_config(config)
}

/// Whether the per-application helper is installed and running.
///
/// Absence is a normal answer, not an error: the helper is opt-in, and the
/// rest of NetMeter works without it.
#[tauri::command]
pub async fn get_helper_state() -> crate::api::helper::HelperState {
    tauri::async_runtime::spawn_blocking(crate::api::helper::state)
        .await
        .unwrap_or_else(|e| crate::api::helper::HelperState::Unreachable {
            message: format!("helper query failed: {e}"),
        })
}

/// Per-application usage, from the helper rather than the local database.
///
/// The frontend never opens the helper's socket itself, for the same reason it
/// never opens SQLite itself.
#[tauri::command]
pub async fn get_app_usage(
    granularity: Granularity,
    from: String,
    to: String,
) -> std::result::Result<crate::api::ipc::AppUsage, crate::api::helper::HelperError> {
    // Blocking socket IO on its own thread: a slow or wedged helper must not
    // stall the async runtime the rest of the commands share.
    tauri::async_runtime::spawn_blocking(move || {
        crate::api::helper::app_usage(granularity, from, to)
    })
        .await
        .unwrap_or_else(|e| {
            Err(crate::api::helper::HelperError {
                kind: "internal".into(),
                message: format!("helper query failed: {e}"),
            })
        })
}

/// Whether NetMeter starts with the desktop session.
#[tauri::command]
pub async fn get_autostart() -> bool {
    crate::system::autostart::is_enabled()
}

/// Turn starting at login on or off.
#[tauri::command]
pub async fn set_autostart(enabled: bool) -> Result<bool> {
    crate::system::autostart::set_enabled(enabled)?;
    Ok(crate::system::autostart::is_enabled())
}

/// Whether this build can offer to install the helper.
#[tauri::command]
pub async fn can_install_helper() -> bool {
    crate::system::helper_install::is_available()
}

/// Install the per-application helper, asking for authorisation through
/// polkit. Returns the helper's state afterwards so the UI refreshes itself.
#[tauri::command]
pub async fn install_helper() -> Result<crate::api::helper::HelperState> {
    tauri::async_runtime::spawn_blocking(crate::system::helper_install::install)
        .await
        .map_err(|e| Error::Internal(format!("install task failed: {e}")))??;

    // systemd returns before the socket is necessarily accepting.
    for _ in 0..20 {
        let state = crate::api::helper::state();
        if matches!(state, crate::api::helper::HelperState::Running(_)) {
            return Ok(state);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    Ok(crate::api::helper::state())
}

/// Resolves a process or application name to a system icon base64 data URL.
#[tauri::command]
pub async fn get_process_icon(name: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || crate::system::proc_icon::get_process_icon(&name))
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;

    fn q(g: Granularity, from: &str, to: &str) -> UsageQuery {
        UsageQuery {
            granularity: g,
            from: from.into(),
            to: to.into(),
            scope: Scope::Included,
            include_breakdown: false,
        }
    }

    fn tz() -> chrono_tz::Tz {
        "Europe/Paris".parse().expect("tz")
    }

    #[test]
    fn a_reversed_range_is_rejected_with_a_clear_message() {
        let e = resolve_range(tz(), &q(Granularity::Day, "2026-09-11", "2026-09-01"))
            .expect_err("must be rejected");
        assert_eq!(e.kind(), "bad_request");
        assert!(e.to_string().contains("after"));
    }

    #[test]
    fn a_malformed_key_is_rejected_rather_than_silently_returning_nothing() {
        for bad in ["", "yesterday", "2026-13-01", "11/09/2026"] {
            let e = resolve_range(tz(), &q(Granularity::Day, bad, "2026-09-11"))
                .expect_err("must be rejected");
            assert_eq!(e.kind(), "bad_request", "{bad:?}");
        }
    }

    #[test]
    fn a_range_reversed_by_exactly_one_day_is_rejected_not_silently_empty() {
        let e = resolve_range(tz(), &q(Granularity::Day, "2026-09-11", "2026-09-10"))
            .expect_err("off-by-one reversal must still be an error");
        assert_eq!(e.kind(), "bad_request");
    }

    #[test]
    fn a_single_day_range_resolves_to_that_local_day() {
        let (from, to) = resolve_range(tz(), &q(Granularity::Day, "2026-09-11", "2026-09-11"))
            .expect("valid");
        assert_eq!(from, to);
        assert_eq!(to.end_utc_ms - from.start_utc_ms, 24 * 3_600_000);
    }

    #[test]
    fn a_month_range_spans_the_whole_month_inclusive() {
        let (from, to) =
            resolve_range(tz(), &q(Granularity::Month, "2026-01", "2026-12")).expect("valid");
        assert_eq!(time::local_date_key(tz(), from.start_utc_ms), "2026-01-01");
        assert_eq!(time::local_date_key(tz(), to.end_utc_ms - 1), "2026-12-31");
    }

    #[test]
    fn scope_all_widens_the_policy_instead_of_needing_a_second_query() {
        let cfg = Config::default();
        let all = Config {
            interface_policy: crate::core::config::InterfacePolicy::AllExcept,
            excluded_interfaces: vec![],
            ..cfg.clone()
        }
        .resolve();
        assert!(all.counts("tailscale0", InterfaceKind::Vpn));
        assert!(!cfg.resolve().counts("tailscale0", InterfaceKind::Vpn));
        // Loopback stays out even under the widest scope.
        assert!(!all.counts("lo", InterfaceKind::Loopback));
    }

    #[test]
    fn totals_are_derived_from_the_series_without_a_second_query() {
        use crate::core::types::{Traffic, UsageBucket, UsageSeries, UsageSummary};
        let series = UsageSeries {
            granularity: Granularity::Day,
            timezone: "Europe/Paris".into(),
            buckets: vec![
                UsageBucket {
                    key: "2026-09-10".into(),
                    start_utc_ms: 0,
                    end_utc_ms: 1,
                    summary: UsageSummary {
                        included: Traffic::new(10, 1),
                        observed: Traffic::new(20, 2),
                    },
                    by_interface: vec![],
                },
                UsageBucket {
                    key: "2026-09-11".into(),
                    start_utc_ms: 1,
                    end_utc_ms: 2,
                    summary: UsageSummary::ZERO,
                    by_interface: vec![],
                },
            ],
            total: UsageSummary {
                included: Traffic::new(10, 1),
                observed: Traffic::new(20, 2),
            },
            offline_total: UsageSummary::ZERO,
            offline_windows: vec![],
            included_interfaces: vec!["wlp7s0".into()],
            data_since: Some("2026-09-01".into()),
            includes_pending: true,
            generated_at_utc_ms: 0,
        };
        let t = UsageTotals::from(&series);
        assert_eq!(t.key_from, "2026-09-10");
        assert_eq!(t.key_to, "2026-09-11");
        assert_eq!(t.included, Traffic::new(10, 1));
        assert!(t.includes_pending);
    }
}
