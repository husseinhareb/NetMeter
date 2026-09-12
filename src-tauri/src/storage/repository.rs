//! The persistence interface the rest of NetMeter sees.
//!
//! The monitor never writes SQL; it hands a [`FlushBatch`] to a [`Repository`].
//! That is the seam that lets the engine move into a standalone `netmeterd`
//! later, and it is what makes the engine testable against an in-memory
//! database.

use crate::core::errors::StorageError;
use crate::core::types::{InterfaceKind, Traffic};
use crate::monitor::sampling::{BucketKey, OfflineWindow, PersistedBaseline};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An interface as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredInterface {
    pub id: i64,
    pub name: String,
    pub kind: InterfaceKind,
    pub mac: Option<String>,
    pub first_seen_utc_ms: i64,
    pub last_seen_utc_ms: i64,
}

/// Everything one flush writes, as one atomic unit.
#[derive(Debug, Clone, Default)]
pub struct FlushBatch {
    /// Bucketed usage, keyed by interface, UTC hour and local date.
    pub buckets: HashMap<BucketKey, Traffic>,
    /// Traffic that happened while NetMeter was not watching.
    pub offline: Vec<OfflineWindow>,
    /// The counter readings to resume from after a restart.
    pub baselines: Vec<PersistedBaseline>,
    /// Interface metadata to insert or refresh.
    pub interfaces: Vec<(String, InterfaceKind, Option<String>)>,
    /// When the batch was assembled.
    pub at_utc_ms: i64,
}

impl FlushBatch {
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
            && self.offline.is_empty()
            && self.baselines.is_empty()
            && self.interfaces.is_empty()
    }
}

/// One row of a usage query: bytes for one interface in one bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub bucket_key: String,
    pub interface_id: i64,
    pub interface_name: String,
    pub kind: InterfaceKind,
    pub traffic: Traffic,
}

/// Traffic recorded while NetMeter was not running, overlapping a query range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineRow {
    pub interface_id: i64,
    pub interface_name: String,
    pub kind: InterfaceKind,
    pub from_utc_ms: i64,
    pub to_utc_ms: i64,
    pub traffic: Traffic,
}

/// What a retention pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub hour_rows: usize,
    pub day_rows: usize,
    pub offline_rows: usize,
    pub interfaces: usize,
}

/// Which table a query reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// `usage_hour`, keyed by UTC hour.
    Hour,
    /// `usage_day`, keyed by local calendar date.
    Day,
}

/// Persistence operations.
///
/// Split into a trait so the engine depends on the capability rather than on
/// SQLite, which is what makes the "move it into a daemon" transition a
/// substitution rather than a rewrite.
pub trait Repository: Send + 'static {
    /// Write one batch atomically. Either all of it lands or none of it does.
    fn flush(&mut self, batch: &FlushBatch) -> Result<(), StorageError>;

    /// The baselines to resume from at startup.
    fn baselines(&self) -> Result<Vec<PersistedBaseline>, StorageError>;

    /// Every interface ever recorded.
    fn interfaces(&self) -> Result<Vec<StoredInterface>, StorageError>;

    /// Usage grouped by bucket key and interface.
    ///
    /// `key_len` is how many leading characters of the row's key identify the
    /// bucket: 10 for a day, 7 for a month, 4 for a year.
    fn usage(
        &self,
        resolution: Resolution,
        from_key: &str,
        to_key: &str,
        key_len: usize,
    ) -> Result<Vec<UsageRow>, StorageError>;

    /// Offline windows overlapping an instant range.
    fn offline(&self, from_utc_ms: i64, to_utc_ms: i64) -> Result<Vec<OfflineRow>, StorageError>;

    /// Apply the retention policy.
    ///
    /// `timezone` is needed because the day tier is keyed by local calendar
    /// date: turning that cutoff back into an instant for the offline table is
    /// a calendar operation, not an arithmetic one.
    fn prune(
        &mut self,
        retention: &crate::core::config::RetentionPolicy,
        timezone: chrono_tz::Tz,
    ) -> Result<PruneReport, StorageError>;

    /// Read a bookkeeping value.
    fn meta(&self, key: &str) -> Result<Option<String>, StorageError>;

    /// Write a bookkeeping value.
    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StorageError>;
}

/// The SQLite-backed repository. Owns the writer connection.
///
/// There is exactly one of these per process, living on the sampling thread.
/// Reads from the command layer use their own read-only connections, so a query
/// never queues behind a flush or behind the daily prune.
pub struct SqliteRepository {
    conn: Connection,
    path: PathBuf,
    /// Interface name to row id, so a flush does not re-query for every bucket.
    ids: HashMap<String, i64>,
}

impl SqliteRepository {
    /// Open (creating if needed) the database at `path`.
    ///
    /// Returns the repository and the path of any quarantined predecessor.
    pub fn open(path: &Path) -> Result<(Self, Option<PathBuf>), StorageError> {
        let (conn, quarantined) = super::database::open_writer(path, super::database::MIGRATIONS)?;
        let mut repo = Self {
            conn,
            path: path.to_path_buf(),
            ids: HashMap::new(),
        };
        repo.load_ids()?;
        Ok((repo, quarantined))
    }

    /// An in-memory database, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory().map_err(StorageError::from_sqlite)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(StorageError::from_sqlite)?;
        conn.execute_batch(include_str!("../../migrations/001_init.sql"))
            .map_err(StorageError::from_sqlite)?;
        Ok(Self {
            conn,
            path: PathBuf::from(":memory:"),
            ids: HashMap::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_ids(&mut self) -> Result<(), StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM interfaces")
            .map_err(StorageError::from_sqlite)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(0)?)))
            .map_err(StorageError::from_sqlite)?;
        self.ids = rows.filter_map(|r| r.ok()).collect();
        Ok(())
    }

    /// Checkpoint the WAL and reclaim freed pages.
    ///
    /// Called from the daily retention pass, on the writer thread. This is what
    /// actually returns space to the filesystem after a prune -- and what stops
    /// the `-wal` file sitting at its historical high-water mark for ever.
    pub fn compact(&self) -> Result<(), StorageError> {
        // Best-effort: a checkpoint blocked by an active reader is not an
        // error, it just means we try again tomorrow.
        if let Err(e) = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
            tracing::debug!(error = %e, "wal checkpoint deferred");
        }
        if let Err(e) = self.conn.execute_batch("PRAGMA incremental_vacuum") {
            tracing::debug!(error = %e, "incremental vacuum deferred");
        }
        Ok(())
    }
}

/// Resolve an interface's row id, creating the row if it is new.
///
/// Cache-first, so the per-bucket lookups in a flush cost nothing. It
/// deliberately does NOT refresh metadata on a cache hit -- that is
/// [`upsert_interface`]'s job, and it runs once per flush rather than once per
/// bucket.
fn interface_id(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    name: &str,
    kind: InterfaceKind,
    mac: Option<&str>,
    now_utc_ms: i64,
) -> Result<i64, StorageError> {
    if let Some(id) = cache.get(name) {
        return Ok(*id);
    }
    upsert_interface(tx, cache, name, kind, mac, now_utc_ms)
}

/// Insert or refresh an interface row, always writing.
///
/// Going through the id cache here would freeze `kind`, `mac` and
/// `last_seen_utc_ms` at whatever they were on first sight for the life of the
/// process -- and `last_seen_utc_ms` is what the retention GC uses to decide an
/// interface is gone, so a stale one is not cosmetic.
fn upsert_interface(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    name: &str,
    kind: InterfaceKind,
    mac: Option<&str>,
    now_utc_ms: i64,
) -> Result<i64, StorageError> {
    // Upsert so a reappearing interface keeps its id -- and therefore its
    // history -- rather than starting a second identity under the same name.
    tx.execute(
        "INSERT INTO interfaces(name, kind, mac, first_seen_utc_ms, last_seen_utc_ms)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(name) DO UPDATE SET
             kind = excluded.kind,
             mac = COALESCE(excluded.mac, interfaces.mac),
             last_seen_utc_ms = excluded.last_seen_utc_ms",
        rusqlite::params![name, kind.as_str(), mac, now_utc_ms],
    )
    .map_err(StorageError::from_sqlite)?;

    let id: i64 = tx
        .query_row("SELECT id FROM interfaces WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .map_err(StorageError::from_sqlite)?;
    cache.insert(name.to_string(), id);
    Ok(id)
}

impl Repository for SqliteRepository {
    fn flush(&mut self, batch: &FlushBatch) -> Result<(), StorageError> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut cache = std::mem::take(&mut self.ids);
        let result = (|| -> Result<(), StorageError> {
            let tx = self.conn.transaction().map_err(StorageError::from_sqlite)?;

            // Always writes, so `last_seen_utc_ms` tracks reality and the
            // retention GC cannot mistake a live interface for a dead one.
            for (name, kind, mac) in &batch.interfaces {
                upsert_interface(&tx, &mut cache, name, *kind, mac.as_deref(), batch.at_utc_ms)?;
            }

            for (key, traffic) in &batch.buckets {
                let kind = batch
                    .interfaces
                    .iter()
                    .find(|(n, _, _)| *n == key.interface)
                    .map(|(_, k, _)| *k)
                    .unwrap_or(InterfaceKind::Virtual);
                let id =
                    interface_id(&tx, &mut cache, &key.interface, kind, None, batch.at_utc_ms)?;

                // Accumulating upserts. Replacing instead of accumulating would
                // make each bucket hold only the last flush's bytes, so the day
                // total would collapse to near zero while the counters still
                // looked right.
                tx.execute(
                    "INSERT INTO usage_hour(hour_start_utc_ms, interface_id, local_date, rx_bytes, tx_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(hour_start_utc_ms, interface_id) DO UPDATE SET
                         rx_bytes = rx_bytes + excluded.rx_bytes,
                         tx_bytes = tx_bytes + excluded.tx_bytes",
                    rusqlite::params![
                        key.hour_start_utc_ms,
                        id,
                        key.local_date,
                        traffic.rx_bytes,
                        traffic.tx_bytes
                    ],
                )
                .map_err(StorageError::from_sqlite)?;

                tx.execute(
                    "INSERT INTO usage_day(local_date, interface_id, rx_bytes, tx_bytes)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(local_date, interface_id) DO UPDATE SET
                         rx_bytes = rx_bytes + excluded.rx_bytes,
                         tx_bytes = tx_bytes + excluded.tx_bytes",
                    rusqlite::params![key.local_date, id, traffic.rx_bytes, traffic.tx_bytes],
                )
                .map_err(StorageError::from_sqlite)?;
            }

            for w in &batch.offline {
                let id = interface_id(
                    &tx,
                    &mut cache,
                    &w.interface,
                    InterfaceKind::Virtual,
                    None,
                    batch.at_utc_ms,
                )?;
                tx.execute(
                    "INSERT INTO usage_offline(from_utc_ms, interface_id, to_utc_ms, rx_bytes, tx_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(from_utc_ms, interface_id) DO UPDATE SET
                         to_utc_ms = MAX(to_utc_ms, excluded.to_utc_ms),
                         rx_bytes = rx_bytes + excluded.rx_bytes,
                         tx_bytes = tx_bytes + excluded.tx_bytes",
                    rusqlite::params![
                        w.from_utc_ms,
                        id,
                        w.to_utc_ms,
                        w.traffic.rx_bytes,
                        w.traffic.tx_bytes
                    ],
                )
                .map_err(StorageError::from_sqlite)?;
            }

            // Written in the same transaction as the usage above: that is what
            // makes a crash lose nothing. If the process dies before the next
            // flush, this baseline is equally stale, so the kernel's cumulative
            // counter still holds the difference.
            for b in &batch.baselines {
                let id = interface_id(
                    &tx,
                    &mut cache,
                    &b.interface,
                    InterfaceKind::Virtual,
                    None,
                    batch.at_utc_ms,
                )?;
                tx.execute(
                    "INSERT INTO counter_state(interface_id, ifindex, last_rx_bytes, last_tx_bytes, last_seen_utc_ms, boot_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(interface_id) DO UPDATE SET
                         ifindex = excluded.ifindex,
                         last_rx_bytes = excluded.last_rx_bytes,
                         last_tx_bytes = excluded.last_tx_bytes,
                         last_seen_utc_ms = excluded.last_seen_utc_ms,
                         boot_id = excluded.boot_id",
                    rusqlite::params![
                        id,
                        b.ifindex,
                        b.counters.rx_bytes,
                        b.counters.tx_bytes,
                        b.at_utc_ms,
                        b.boot_id
                    ],
                )
                .map_err(StorageError::from_sqlite)?;
            }

            tx.commit().map_err(StorageError::from_sqlite)
        })();

        self.ids = cache;
        if result.is_err() {
            // A rolled-back transaction may have handed out ids that never
            // landed. Re-read rather than trusting the cache.
            let _ = self.load_ids();
        }
        result
    }

    fn baselines(&self) -> Result<Vec<PersistedBaseline>, StorageError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT i.name, c.ifindex, c.last_rx_bytes, c.last_tx_bytes, c.last_seen_utc_ms, c.boot_id
                 FROM counter_state c JOIN interfaces i ON i.id = c.interface_id",
            )
            .map_err(StorageError::from_sqlite)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PersistedBaseline {
                    interface: r.get(0)?,
                    ifindex: r.get(1)?,
                    counters: crate::core::types::NetworkCounters {
                        rx_bytes: r.get(2)?,
                        tx_bytes: r.get(3)?,
                    },
                    at_utc_ms: r.get(4)?,
                    boot_id: r.get(5)?,
                })
            })
            .map_err(StorageError::from_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from_sqlite)
    }

    fn interfaces(&self) -> Result<Vec<StoredInterface>, StorageError> {
        query_interfaces(&self.conn)
    }

    fn usage(
        &self,
        resolution: Resolution,
        from_key: &str,
        to_key: &str,
        key_len: usize,
    ) -> Result<Vec<UsageRow>, StorageError> {
        query_usage(&self.conn, resolution, from_key, to_key, key_len)
    }

    fn offline(&self, from_utc_ms: i64, to_utc_ms: i64) -> Result<Vec<OfflineRow>, StorageError> {
        query_offline(&self.conn, from_utc_ms, to_utc_ms)
    }

    fn prune(
        &mut self,
        retention: &crate::core::config::RetentionPolicy,
        timezone: chrono_tz::Tz,
    ) -> Result<PruneReport, StorageError> {
        let mut report = PruneReport::default();
        let tx = self.conn.transaction().map_err(StorageError::from_sqlite)?;

        // Cutoffs are anchored to the newest row we actually hold, never to the
        // system clock. If the clock is stepped forward a year, a now()-based
        // cutoff would delete every row in the table on the next startup, and
        // there is no source to re-derive that history from -- the kernel keeps
        // a cumulative counter, not a history.
        if retention.hourly_days > 0 {
            let newest: Option<i64> = tx
                .query_row("SELECT MAX(hour_start_utc_ms) FROM usage_hour", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(StorageError::from_sqlite)?
                .flatten();
            if let Some(newest) = newest {
                let cutoff = newest - retention.hourly_days as i64 * 86_400_000;
                report.hour_rows = tx
                    .execute(
                        "DELETE FROM usage_hour WHERE hour_start_utc_ms < ?1",
                        [cutoff],
                    )
                    .map_err(StorageError::from_sqlite)?;
            }
        }

        if retention.daily_days > 0 {
            let newest: Option<String> = tx
                .query_row("SELECT MAX(local_date) FROM usage_day", [], |r| r.get(0))
                .optional()
                .map_err(StorageError::from_sqlite)?
                .flatten();
            if let Some(newest) = newest {
                if let Ok(d) = chrono::NaiveDate::parse_from_str(&newest, "%Y-%m-%d") {
                    let cutoff = (d - chrono::TimeDelta::days(retention.daily_days as i64))
                        .format("%Y-%m-%d")
                        .to_string();
                    report.day_rows = tx
                        .execute("DELETE FROM usage_day WHERE local_date < ?1", [&cutoff])
                        .map_err(StorageError::from_sqlite)?;
                    // The cutoff is a LOCAL calendar date, so it must be
                    // converted to an instant in the user's zone, not treated
                    // as if midnight local were midnight UTC. Getting this
                    // wrong drops up to a day's worth of offline traffic for a
                    // date whose `usage_day` row is being kept.
                    let cutoff_ms = chrono::NaiveDate::parse_from_str(&cutoff, "%Y-%m-%d")
                        .map(|d| crate::core::time::day_start(timezone, d).timestamp_millis())
                        .unwrap_or(i64::MIN);
                    report.offline_rows = tx
                        .execute("DELETE FROM usage_offline WHERE to_utc_ms < ?1", [cutoff_ms])
                        .map_err(StorageError::from_sqlite)?;
                }
            }
        }

        // Forget interfaces that are gone and hold no history. This is what
        // stops Docker's random `vethXXXXXXX` names from accumulating for ever
        // on a developer's laptop.
        if retention.interface_days > 0 {
            let newest: Option<i64> = tx
                .query_row("SELECT MAX(last_seen_utc_ms) FROM interfaces", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(StorageError::from_sqlite)?
                .flatten();
            if let Some(newest) = newest {
                let cutoff = newest - retention.interface_days as i64 * 86_400_000;
                report.interfaces = tx
                    .execute(
                        "DELETE FROM interfaces
                         WHERE last_seen_utc_ms < ?1
                           AND id NOT IN (SELECT interface_id FROM usage_day)
                           AND id NOT IN (SELECT interface_id FROM usage_hour)
                           AND id NOT IN (SELECT interface_id FROM usage_offline)",
                        [cutoff],
                    )
                    .map_err(StorageError::from_sqlite)?;
            }
        }

        tx.commit().map_err(StorageError::from_sqlite)?;
        if report.interfaces > 0 {
            self.load_ids()?;
        }
        Ok(report)
    }

    fn meta(&self, key: &str) -> Result<Option<String>, StorageError> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(StorageError::from_sqlite)
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StorageError> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value],
            )
            .map_err(StorageError::from_sqlite)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Read queries, as free functions over a connection.
//
// Shared by the writer repository and by the command layer's per-call
// read-only connections, so there is exactly one copy of each statement.
// ---------------------------------------------------------------------------

/// Usage grouped by bucket key and interface.
///
/// `GROUP BY` rather than a bare aggregate: an empty range returns zero rows,
/// which the series builder fills with zeros. A bare `SUM()` would return one
/// row containing NULL and fail the `u64` decode -- an error toast on the
/// Today tile every morning before the first flush.
pub fn query_usage(
    conn: &Connection,
    resolution: Resolution,
    from_key: &str,
    to_key: &str,
    key_len: usize,
) -> Result<Vec<UsageRow>, StorageError> {
    let sql = match resolution {
        Resolution::Hour => {
            "SELECT substr(strftime('%Y-%m-%dT%H', u.hour_start_utc_ms / 1000, 'unixepoch'), 1, ?3) AS k,
                    u.interface_id, i.name, i.kind,
                    SUM(u.rx_bytes), SUM(u.tx_bytes)
             FROM usage_hour u JOIN interfaces i ON i.id = u.interface_id
             WHERE u.hour_start_utc_ms >= ?1 AND u.hour_start_utc_ms < ?2
             GROUP BY k, u.interface_id
             ORDER BY k"
        }
        Resolution::Day => {
            "SELECT substr(u.local_date, 1, ?3) AS k,
                    u.interface_id, i.name, i.kind,
                    SUM(u.rx_bytes), SUM(u.tx_bytes)
             FROM usage_day u JOIN interfaces i ON i.id = u.interface_id
             WHERE u.local_date >= ?1 AND u.local_date <= ?2
             GROUP BY k, u.interface_id
             ORDER BY k"
        }
    };

    let mut stmt = conn.prepare_cached(sql).map_err(StorageError::from_sqlite)?;
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<UsageRow> {
        Ok(UsageRow {
            bucket_key: r.get(0)?,
            interface_id: r.get(1)?,
            interface_name: r.get(2)?,
            kind: InterfaceKind::from_str_lossy(&r.get::<_, String>(3)?),
            traffic: Traffic::new(r.get(4)?, r.get(5)?),
        })
    };

    let rows = match resolution {
        // Hour keys are instants, so they bind as integers.
        Resolution::Hour => {
            let from: i64 = from_key.parse().unwrap_or(i64::MIN);
            let to: i64 = to_key.parse().unwrap_or(i64::MAX);
            stmt.query_map(rusqlite::params![from, to, key_len as i64], map)
                .map_err(StorageError::from_sqlite)?
                .collect::<Result<Vec<_>, _>>()
        }
        // Day keys are zero-padded ISO strings, which compare
        // lexicographically in chronological order -- so this is a range seek
        // on the primary key, never a LIKE scan.
        Resolution::Day => stmt
            .query_map(rusqlite::params![from_key, to_key, key_len as i64], map)
            .map_err(StorageError::from_sqlite)?
            .collect::<Result<Vec<_>, _>>(),
    };
    rows.map_err(StorageError::from_sqlite)
}

/// Offline windows overlapping an instant range.
pub fn query_offline(
    conn: &Connection,
    from_utc_ms: i64,
    to_utc_ms: i64,
) -> Result<Vec<OfflineRow>, StorageError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT o.interface_id, i.name, i.kind, o.from_utc_ms, o.to_utc_ms, o.rx_bytes, o.tx_bytes
             FROM usage_offline o JOIN interfaces i ON i.id = o.interface_id
             WHERE o.from_utc_ms < ?2 AND o.to_utc_ms > ?1
             ORDER BY o.from_utc_ms",
        )
        .map_err(StorageError::from_sqlite)?;
    let rows = stmt
        .query_map(rusqlite::params![from_utc_ms, to_utc_ms], |r| {
            Ok(OfflineRow {
                interface_id: r.get(0)?,
                interface_name: r.get(1)?,
                kind: InterfaceKind::from_str_lossy(&r.get::<_, String>(2)?),
                from_utc_ms: r.get(3)?,
                to_utc_ms: r.get(4)?,
                traffic: Traffic::new(r.get(5)?, r.get(6)?),
            })
        })
        .map_err(StorageError::from_sqlite)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from_sqlite)
}

/// Every interface ever recorded.
pub fn query_interfaces(conn: &Connection) -> Result<Vec<StoredInterface>, StorageError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, name, kind, mac, first_seen_utc_ms, last_seen_utc_ms
             FROM interfaces ORDER BY name",
        )
        .map_err(StorageError::from_sqlite)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(StoredInterface {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: InterfaceKind::from_str_lossy(&r.get::<_, String>(2)?),
                mac: r.get(3)?,
                first_seen_utc_ms: r.get(4)?,
                last_seen_utc_ms: r.get(5)?,
            })
        })
        .map_err(StorageError::from_sqlite)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from_sqlite)
}
