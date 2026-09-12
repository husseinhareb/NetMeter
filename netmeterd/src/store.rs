//! Persistence for per-application usage.
//!
//! Its own database, because this one is written by a privileged service and
//! holds every user's rows. The connection setup, pragma order and migration
//! runner come from the GUI crate's `storage::database` -- that ordering was
//! established by measurement and is not worth rediscovering here.

use netmeter_lib::core::errors::StorageError;
use netmeter_lib::core::time;
use netmeter_lib::storage::database;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;

const SCHEMA: database::Schema = &[(1, include_str!("../migrations/001_apps.sql"))];

/// Applications kept per day after a day is complete. The rest collapse into
/// one `other` row: the tail of a busy desktop is dozens of one-request
/// helpers, and nobody reads row 40 of yesterday.
const TOP_APPS_PER_DAY: usize = 20;

/// The key `other` collapses into. Not a real application, so it never gets an
/// `exe_path`.
const OTHER: &str = "other";

/// One application's traffic in one hour, keyed the way it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BucketKey {
    pub hour_start_utc_ms: i64,
    pub local_date: String,
    pub uid: u32,
    pub app: String,
}

/// What one flush writes. Accumulated in memory between flushes so a busy
/// desktop costs one transaction every few seconds, not one per sample.
#[derive(Debug, Default)]
pub struct Batch {
    pub buckets: HashMap<BucketKey, (u64, u64)>,
    /// Executable path per app key, for the apps table. Best effort.
    pub exe_paths: HashMap<String, String>,
    /// Wire bytes no application owned, per local date.
    pub overhead: HashMap<String, (u64, u64)>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty() && self.overhead.is_empty()
    }

    pub fn add(&mut self, key: BucketKey, rx: u64, tx: u64) {
        let slot = self.buckets.entry(key).or_insert((0, 0));
        slot.0 = slot.0.saturating_add(rx);
        slot.1 = slot.1.saturating_add(tx);
    }

    pub fn add_overhead(&mut self, local_date: String, rx: u64, tx: u64) {
        let slot = self.overhead.entry(local_date).or_insert((0, 0));
        slot.0 = slot.0.saturating_add(rx);
        slot.1 = slot.1.saturating_add(tx);
    }
}

/// How long each tier is kept.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub hourly_days: u32,
    pub daily_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        // Hourly for just over a year, daily forever: a day row is ~50 bytes,
        // so "how much did Firefox use in 2027" stays answerable.
        Self {
            hourly_days: 400,
            daily_days: 0,
        }
    }
}

pub struct Store {
    conn: Connection,
    app_ids: HashMap<String, i64>,
}

impl Store {
    pub fn open(path: &Path) -> Result<(Self, Option<std::path::PathBuf>), StorageError> {
        let (conn, quarantined) = database::open_writer(path, SCHEMA)?;
        Ok((
            Self {
                conn,
                app_ids: HashMap::new(),
            },
            quarantined,
        ))
    }

    /// Write a batch in one transaction: either an interval is recorded or it
    /// is not, never half of it.
    pub fn flush(&mut self, batch: &Batch, now_utc_ms: i64) -> Result<(), StorageError> {
        if batch.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction().map_err(StorageError::from_sqlite)?;
        {
            for (key, (rx, tx_bytes)) in &batch.buckets {
                let app_id = app_id(
                    &tx,
                    &mut self.app_ids,
                    &key.app,
                    batch.exe_paths.get(&key.app).map(String::as_str),
                    now_utc_ms,
                )?;

                // Accumulating upsert: a flush adds to the hour it belongs to
                // rather than replacing it, so two flushes inside one hour do
                // not lose the first one's bytes.
                tx.execute(
                    "INSERT INTO usage_app_hour
                         (hour_start_utc_ms, uid, app_id, local_date, rx_bytes, tx_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (hour_start_utc_ms, uid, app_id) DO UPDATE SET
                         rx_bytes = rx_bytes + excluded.rx_bytes,
                         tx_bytes = tx_bytes + excluded.tx_bytes",
                    params![
                        key.hour_start_utc_ms,
                        key.uid,
                        app_id,
                        key.local_date,
                        rx,
                        tx_bytes
                    ],
                )
                .map_err(StorageError::from_sqlite)?;

                tx.execute(
                    "INSERT INTO usage_app_day (local_date, uid, app_id, rx_bytes, tx_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (local_date, uid, app_id) DO UPDATE SET
                         rx_bytes = rx_bytes + excluded.rx_bytes,
                         tx_bytes = tx_bytes + excluded.tx_bytes",
                    params![key.local_date, key.uid, app_id, rx, tx_bytes],
                )
                .map_err(StorageError::from_sqlite)?;
            }

            for (date, (rx, tx_bytes)) in &batch.overhead {
                tx.execute(
                    "INSERT INTO overhead_day (local_date, rx_bytes, tx_bytes)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (local_date) DO UPDATE SET
                         rx_bytes = rx_bytes + excluded.rx_bytes,
                         tx_bytes = tx_bytes + excluded.tx_bytes",
                    params![date, rx, tx_bytes],
                )
                .map_err(StorageError::from_sqlite)?;
            }
        }
        tx.commit().map_err(StorageError::from_sqlite)
    }

    /// Drop hourly rows past their retention, collapse each finished day's
    /// long tail into `other`, and forget applications that hold no history.
    pub fn prune(
        &mut self,
        retention: Retention,
        timezone: chrono_tz::Tz,
        now_utc_ms: i64,
    ) -> Result<(), StorageError> {
        let today = time::local_date_key(timezone, now_utc_ms);
        let tx = self.conn.transaction().map_err(StorageError::from_sqlite)?;

        if retention.hourly_days > 0 {
            let cutoff = cutoff_date(timezone, now_utc_ms, retention.hourly_days);
            tx.execute(
                "DELETE FROM usage_app_hour WHERE local_date < ?1",
                params![cutoff],
            )
            .map_err(StorageError::from_sqlite)?;
        }
        if retention.daily_days > 0 {
            let cutoff = cutoff_date(timezone, now_utc_ms, retention.daily_days);
            tx.execute(
                "DELETE FROM usage_app_day WHERE local_date < ?1",
                params![cutoff],
            )
            .map_err(StorageError::from_sqlite)?;
            tx.execute(
                "DELETE FROM overhead_day WHERE local_date < ?1",
                params![cutoff],
            )
            .map_err(StorageError::from_sqlite)?;
        }

        collapse_tails(&tx, &mut self.app_ids, &today, now_utc_ms)?;

        // An application with no usage left is just a name taking up space.
        tx.execute(
            "DELETE FROM apps
             WHERE id NOT IN (SELECT app_id FROM usage_app_day)
               AND id NOT IN (SELECT app_id FROM usage_app_hour)",
            [],
        )
        .map_err(StorageError::from_sqlite)?;

        tx.commit().map_err(StorageError::from_sqlite)
    }

    /// One day's applications, largest first. The read side of step 4 will
    /// grow from this; for now it is what the tests assert against.
    pub fn day_totals(&self, local_date: &str) -> Result<Vec<(String, u64, u64)>, StorageError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT a.app_key, d.rx_bytes, d.tx_bytes
                   FROM usage_app_day d JOIN apps a ON a.id = d.app_id
                  WHERE d.local_date = ?1
                  ORDER BY d.rx_bytes + d.tx_bytes DESC",
            )
            .map_err(StorageError::from_sqlite)?;
        let rows = stmt
            .query_map(params![local_date], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(StorageError::from_sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from_sqlite)?;
        Ok(rows)
    }

    pub fn overhead_for(&self, local_date: &str) -> Result<(u64, u64), StorageError> {
        self.conn
            .query_row(
                "SELECT rx_bytes, tx_bytes FROM overhead_day WHERE local_date = ?1",
                params![local_date],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok((0, 0)),
                other => Err(StorageError::from_sqlite(other)),
            })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>, StorageError> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(StorageError::from_sqlite(other)),
            })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StorageError> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(StorageError::from_sqlite)
    }

    pub fn hourly_rows(&self) -> Result<i64, StorageError> {
        self.conn
            .query_row("SELECT count(*) FROM usage_app_hour", [], |r| r.get(0))
            .map_err(StorageError::from_sqlite)
    }
}

fn cutoff_date(timezone: chrono_tz::Tz, now_utc_ms: i64, days: u32) -> String {
    let today = time::local_date(timezone, now_utc_ms);
    let cutoff = today - chrono::Duration::days(days as i64);
    cutoff.format("%Y-%m-%d").to_string()
}

fn app_id(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    app_key: &str,
    exe_path: Option<&str>,
    now_utc_ms: i64,
) -> Result<i64, StorageError> {
    if let Some(id) = cache.get(app_key) {
        // Still worth keeping `last_seen` current: it is what the UI shows as
        // "last used" and what retention would eventually judge.
        tx.execute(
            "UPDATE apps SET last_seen_utc_ms = ?2 WHERE id = ?1",
            params![id, now_utc_ms],
        )
        .map_err(StorageError::from_sqlite)?;
        return Ok(*id);
    }

    tx.execute(
        "INSERT INTO apps
             (app_key, display_name, exe_path, first_seen_utc_ms, last_seen_utc_ms)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (app_key) DO UPDATE SET
             last_seen_utc_ms = excluded.last_seen_utc_ms,
             exe_path = coalesce(excluded.exe_path, exe_path)",
        params![app_key, app_key, exe_path, now_utc_ms],
    )
    .map_err(StorageError::from_sqlite)?;

    let id: i64 = tx
        .query_row(
            "SELECT id FROM apps WHERE app_key = ?1",
            params![app_key],
            |r| r.get(0),
        )
        .map_err(StorageError::from_sqlite)?;
    cache.insert(app_key.to_string(), id);
    Ok(id)
}

/// For every finished day holding more than `TOP_APPS_PER_DAY` applications,
/// sum the tail into `other` and delete it.
fn collapse_tails(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    today: &str,
    now_utc_ms: i64,
) -> Result<(), StorageError> {
    let dates: Vec<String> = {
        let mut stmt = tx
            .prepare(
                "SELECT local_date FROM usage_app_day
                  WHERE local_date < ?1
                  GROUP BY local_date, uid
                 HAVING count(*) > ?2",
            )
            .map_err(StorageError::from_sqlite)?;
        let rows = stmt
            .query_map(params![today, TOP_APPS_PER_DAY as i64], |r| r.get(0))
            .map_err(StorageError::from_sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from_sqlite)?;
        rows
    };

    if dates.is_empty() {
        return Ok(());
    }
    let other_id = app_id(tx, cache, OTHER, None, now_utc_ms)?;

    for date in dates {
        // The tail: everything outside the top N for that day, `other`
        // excluded so repeated runs stay idempotent.
        let (rx, tx_bytes): (u64, u64) = tx
            .query_row(
                "SELECT coalesce(sum(rx_bytes), 0), coalesce(sum(tx_bytes), 0) FROM (
                     SELECT rx_bytes, tx_bytes FROM usage_app_day
                      WHERE local_date = ?1 AND app_id != ?2
                      ORDER BY rx_bytes + tx_bytes DESC
                      LIMIT -1 OFFSET ?3
                 )",
                params![date, other_id, TOP_APPS_PER_DAY as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(StorageError::from_sqlite)?;

        if rx == 0 && tx_bytes == 0 {
            continue;
        }

        tx.execute(
            "DELETE FROM usage_app_day
              WHERE local_date = ?1 AND app_id != ?2
                AND app_id IN (
                    SELECT app_id FROM usage_app_day
                     WHERE local_date = ?1 AND app_id != ?2
                     ORDER BY rx_bytes + tx_bytes DESC
                     LIMIT -1 OFFSET ?3
                )",
            params![date, other_id, TOP_APPS_PER_DAY as i64],
        )
        .map_err(StorageError::from_sqlite)?;

        tx.execute(
            "INSERT INTO usage_app_day (local_date, uid, app_id, rx_bytes, tx_bytes)
             VALUES (?1, 0, ?2, ?3, ?4)
             ON CONFLICT (local_date, uid, app_id) DO UPDATE SET
                 rx_bytes = rx_bytes + excluded.rx_bytes,
                 tx_bytes = tx_bytes + excluded.tx_bytes",
            params![date, other_id, rx, tx_bytes],
        )
        .map_err(StorageError::from_sqlite)?;
    }
    Ok(())
}
