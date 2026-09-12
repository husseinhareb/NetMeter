//! Reading per-application usage back out, for the socket to serve.
//!
//! Read-only connections, opened per request, exactly as the GUI does for its
//! own database: the writer keeps its connection, readers never block it.

use netmeter_lib::api::ipc::{AppBucket, AppTraffic, AppUsage, Traffic};
use netmeter_lib::core::errors::StorageError;
use netmeter_lib::core::time;
use netmeter_lib::core::types::{Granularity, UsagePeriod};
use rusqlite::{params, Connection};
use std::collections::HashMap;

/// Why a query could not be answered.
///
/// Split from `StorageError` because the two mean different things to the
/// caller: a malformed calendar key is the request's fault and will fail the
/// same way if retried, a locked database is not.
#[derive(Debug)]
pub enum QueryError {
    BadRequest(String),
    Storage(StorageError),
}

impl QueryError {
    /// The `kind` the GUI branches on, so it never has to match on message
    /// text.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::Storage(_) => "storage",
        }
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) => write!(f, "{m}"),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<StorageError> for QueryError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// One application's bytes in one bucket, as it comes out of SQL.
struct Row {
    bucket: String,
    app: String,
    rx: u64,
    tx: u64,
}

/// Answer one `AppUsage` request.
///
/// `uid` comes from the connection's peer credentials. It is the only thing
/// restricting what a caller can see, so it is applied in the SQL rather than
/// filtered afterwards.
pub fn app_usage(
    conn: &Connection,
    uid: u32,
    granularity: Granularity,
    from: &str,
    to: &str,
    timezone: chrono_tz::Tz,
) -> Result<AppUsage, QueryError> {
    let from_period = time::period_for_key(timezone, from)
        .ok_or_else(|| QueryError::BadRequest(format!("not a calendar key: {from}")))?;
    let to_period = time::period_for_key(timezone, to)
        .ok_or_else(|| QueryError::BadRequest(format!("not a calendar key: {to}")))?;
    if to_period.end_utc_ms <= from_period.start_utc_ms {
        return Err(QueryError::BadRequest(format!(
            "range ends before it starts: {from}..{to}"
        )));
    }

    let keys = time::enumerate_keys(timezone, granularity, &from_period, &to_period);
    let rows = match granularity {
        Granularity::Hour => hourly_rows(conn, uid, &from_period, &to_period)?,
        _ => daily_rows(conn, uid, granularity, &from_period, &to_period)?,
    };

    // Group into buckets, then fill every key so the series is dense: a day
    // the machine was off is a zero bucket, not a gap the UI has to invent.
    let mut by_bucket: HashMap<String, HashMap<String, Traffic>> = HashMap::new();
    let mut totals: HashMap<String, Traffic> = HashMap::new();
    for row in rows {
        let slot = by_bucket
            .entry(row.bucket)
            .or_default()
            .entry(row.app.clone())
            .or_default();
        slot.rx_bytes += row.rx;
        slot.tx_bytes += row.tx;
        let total = totals.entry(row.app).or_default();
        total.rx_bytes += row.rx;
        total.tx_bytes += row.tx;
    }

    // Overhead is stored per day, so it is only placed in day-or-wider
    // buckets. Splitting a day's headers across its hours would be inventing
    // a distribution nobody measured.
    let overhead_by_bucket = if granularity == Granularity::Hour {
        HashMap::new()
    } else {
        overhead_rows(conn, granularity, &from_period, &to_period)?
    };
    let mut overhead_total = Traffic::default();
    for t in overhead_by_bucket.values() {
        overhead_total.rx_bytes += t.rx_bytes;
        overhead_total.tx_bytes += t.tx_bytes;
    }

    let buckets = keys
        .into_iter()
        .map(|(key, period)| AppBucket {
            apps: sorted(by_bucket.remove(&key).unwrap_or_default()),
            overhead: overhead_by_bucket.get(&key).copied().unwrap_or_default(),
            key,
            start_utc_ms: period.start_utc_ms,
            end_utc_ms: period.end_utc_ms,
        })
        .collect();

    Ok(AppUsage {
        granularity,
        timezone: timezone.name().to_string(),
        buckets,
        total: sorted(totals),
        overhead: overhead_total,
        data_since: earliest_date(conn)?,
        uid,
        generated_at_utc_ms: time::now_utc_ms(),
    })
}

fn sorted(map: HashMap<String, Traffic>) -> Vec<AppTraffic> {
    let mut out: Vec<AppTraffic> = map
        .into_iter()
        .map(|(app, t)| AppTraffic {
            app,
            rx_bytes: t.rx_bytes,
            tx_bytes: t.tx_bytes,
        })
        .collect();
    // Largest first, then by name so equal rows keep a stable order between
    // requests instead of shuffling under the reader.
    out.sort_by(|a, b| {
        (b.rx_bytes + b.tx_bytes)
            .cmp(&(a.rx_bytes + a.tx_bytes))
            .then_with(|| a.app.cmp(&b.app))
    });
    out
}

/// `local_date` prefix length that identifies a bucket at this granularity.
fn key_width(granularity: Granularity) -> usize {
    match granularity {
        Granularity::Year => 4,
        Granularity::Month => 7,
        _ => 10,
    }
}

fn daily_rows(
    conn: &Connection,
    uid: u32,
    granularity: Granularity,
    from: &UsagePeriod,
    to: &UsagePeriod,
) -> Result<Vec<Row>, StorageError> {
    let width = key_width(granularity) as i64;
    let mut stmt = conn
        .prepare(
            "SELECT substr(d.local_date, 1, ?1), a.app_key, d.rx_bytes, d.tx_bytes
               FROM usage_app_day d JOIN apps a ON a.id = d.app_id
              WHERE d.uid = ?2 AND d.local_date >= ?3 AND d.local_date <= ?4",
        )
        .map_err(StorageError::from_sqlite)?;
    let rows = stmt
        .query_map(
            params![width, uid, date_of(from.start_utc_ms), date_of(to.end_utc_ms - 1)],
            |r| {
                Ok(Row {
                    bucket: r.get(0)?,
                    app: r.get(1)?,
                    rx: r.get(2)?,
                    tx: r.get(3)?,
                })
            },
        )
        .map_err(StorageError::from_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from_sqlite)?;
    Ok(rows)
}

fn hourly_rows(
    conn: &Connection,
    uid: u32,
    from: &UsagePeriod,
    to: &UsagePeriod,
) -> Result<Vec<Row>, StorageError> {
    let mut stmt = conn
        .prepare(
            "SELECT h.hour_start_utc_ms, a.app_key, h.rx_bytes, h.tx_bytes
               FROM usage_app_hour h JOIN apps a ON a.id = h.app_id
              WHERE h.uid = ?1 AND h.hour_start_utc_ms >= ?2 AND h.hour_start_utc_ms < ?3",
        )
        .map_err(StorageError::from_sqlite)?;
    let rows = stmt
        .query_map(params![uid, from.start_utc_ms, to.end_utc_ms], |r| {
            let ms: i64 = r.get(0)?;
            Ok(Row {
                bucket: hour_key(ms),
                app: r.get(1)?,
                rx: r.get(2)?,
                tx: r.get(3)?,
            })
        })
        .map_err(StorageError::from_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from_sqlite)?;
    Ok(rows)
}

fn overhead_rows(
    conn: &Connection,
    granularity: Granularity,
    from: &UsagePeriod,
    to: &UsagePeriod,
) -> Result<HashMap<String, Traffic>, StorageError> {
    let width = key_width(granularity) as i64;
    let mut stmt = conn
        .prepare(
            "SELECT substr(local_date, 1, ?1), sum(rx_bytes), sum(tx_bytes)
               FROM overhead_day
              WHERE local_date >= ?2 AND local_date <= ?3
              GROUP BY 1",
        )
        .map_err(StorageError::from_sqlite)?;
    let rows = stmt
        .query_map(
            params![width, date_of(from.start_utc_ms), date_of(to.end_utc_ms - 1)],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Traffic {
                        rx_bytes: r.get(1)?,
                        tx_bytes: r.get(2)?,
                    },
                ))
            },
        )
        .map_err(StorageError::from_sqlite)?
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(StorageError::from_sqlite)?;
    Ok(rows)
}

fn earliest_date(conn: &Connection) -> Result<Option<String>, StorageError> {
    conn.query_row("SELECT min(local_date) FROM usage_app_day", [], |r| r.get(0))
        .map_err(StorageError::from_sqlite)
}

/// The stored `local_date` for an instant. Overhead and daily rows are keyed
/// by the date that was in force when they were written, so the bounds have to
/// be expressed the same way.
fn date_of(utc_ms: i64) -> String {
    // UTC here, not the request's zone: the column holds whatever local date
    // the writer recorded, and a range wide enough to include the boundary
    // days is what matters. `enumerate_keys` decides which buckets exist.
    chrono::DateTime::from_timestamp_millis(utc_ms)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn hour_key(utc_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(utc_ms)
        .map(|d| d.format("%Y-%m-%dT%H").to_string())
        .unwrap_or_default()
}
