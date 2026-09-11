//! Opening, configuring and migrating the SQLite database.

use crate::core::errors::StorageError;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Embedded so the binary is self-contained -- no migration files to ship,
/// no path to resolve at runtime.
const MIGRATIONS: &[(i32, &str)] = &[(1, include_str!("../../migrations/001_init.sql"))];

/// The schema version this build expects.
pub fn target_version() -> i32 {
    MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0)
}

/// Open the writer connection, creating and migrating the database if needed.
///
/// Returns the connection and, if the previous file had to be quarantined, the
/// path it was moved to -- surfaced to the user rather than silently swallowed,
/// because it means their history was reset.
pub fn open_writer(path: &Path) -> Result<(Connection, Option<PathBuf>), StorageError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| StorageError::DataDir {
            path: dir.display().to_string(),
            source,
        })?;
    }

    match try_open(path) {
        Ok(conn) => Ok((conn, None)),
        Err(e) if is_corrupt(&e) => {
            // A corrupt database is otherwise a permanent brick: the app would
            // fail to start on every launch, with no way out that does not
            // involve the user finding and deleting the file themselves. The
            // bad file is kept rather than deleted.
            let backup = quarantine(path)?;
            tracing::error!(
                quarantined = %backup.display(),
                error = %e,
                "database was unusable; moved aside and starting fresh"
            );
            Ok((try_open(path)?, Some(backup)))
        }
        Err(e) => Err(e),
    }
}

/// Open, configure, integrity-check and migrate.
///
/// SQLite opens lazily -- `Connection::open` on a file of garbage succeeds and
/// the error only surfaces at the first statement -- so every step that can
/// discover corruption lives inside this one function, and the caller retries
/// the whole thing after quarantining.
fn try_open(path: &Path) -> Result<Connection, StorageError> {
    let mut conn = Connection::open(path).map_err(StorageError::from_sqlite)?;
    configure(&conn)?;

    // Catch corruption that only shows up on read, before the first write hits
    // it mid-flush. Milliseconds on a database of this size.
    let healthy: String = conn
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .map_err(StorageError::from_sqlite)?;
    if healthy != "ok" {
        return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(format!("integrity check: {healthy}")),
        )));
    }

    migrate(&mut conn)?;
    Ok(conn)
}

/// Open an additional read-only connection.
///
/// Commands use these so that a read never waits behind the writer's flush or
/// behind the daily prune. That is the whole point of WAL, and sharing one
/// `Mutex<Connection>` between the sampler and the UI would throw it away.
pub fn open_reader(path: &Path) -> Result<Connection, StorageError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(StorageError::from_sqlite)?;
    // Readers get the timeout and the memory settings, but must not try to
    // change the journal mode -- that requires write access.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(StorageError::from_sqlite)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(StorageError::from_sqlite)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<(), StorageError> {
    // FIRST, before WAL and before any table exists.
    //
    // `auto_vacuum` can only be changed on a database with no tables, and
    // setting it *after* `journal_mode = WAL` is silently ignored -- the
    // pragma reports success and the database stays at NONE, which makes the
    // `incremental_vacuum` in `compact()` dead code that reclaims nothing.
    // Verified empirically: WAL-then-auto_vacuum yields `PRAGMA auto_vacuum`
    // = 0, auto_vacuum-then-WAL yields 2.
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .map_err(StorageError::from_sqlite)?;

    // WAL: readers do not block the writer and the writer does not block
    // readers. The right mode for a process that writes on a timer while a UI
    // reads on demand.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .map_err(StorageError::from_sqlite)?;
    if !mode.eq_ignore_ascii_case("wal") {
        // Happens on network filesystems, which do not support WAL's shared
        // memory. Not fatal -- the default rollback journal still works.
        tracing::warn!(mode = %mode, "WAL unavailable; continuing in this journal mode");
    }

    for (pragma, value) in [
        // NORMAL instead of FULL: in WAL mode this only risks losing the last
        // transaction on an OS crash or power loss, not corruption -- and
        // NetMeter can re-derive a lost transaction from the kernel counter.
        // FULL would mean an fsync on every 30-second flush, forever, on a
        // laptop.
        ("synchronous", "NORMAL"),
        // Wait rather than failing when another connection holds the write
        // lock.
        ("busy_timeout", "5000"),
        // ON so retention can delete an interface and have its usage rows go
        // with it, via ON DELETE CASCADE.
        ("foreign_keys", "ON"),
        // Checkpoint at ~4 MB of WAL.
        ("wal_autocheckpoint", "1000"),
        // This is the pragma that actually shrinks the -wal file after a
        // checkpoint. Without it the WAL stays at its historical high-water
        // mark forever, which on a months-long uptime is how a 3 MB database
        // ends up beside a multi-gigabyte journal.
        ("journal_size_limit", "8388608"),
        // ~2 MB page cache. Enough for the working set, small enough to not
        // matter on a laptop.
        ("cache_size", "-2000"),
        ("temp_store", "MEMORY"),
    ] {
        conn.pragma_update(None, pragma, value)
            .map_err(StorageError::from_sqlite)?;
    }
    Ok(())
}

/// Apply any migrations the file has not seen.
///
/// `user_version` rather than a migrations table: it is a single integer in the
/// database header, costs no row, and cannot itself get out of sync with the
/// schema it describes.
fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    let current: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(StorageError::from_sqlite)?;

    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.transaction().map_err(StorageError::from_sqlite)?;
        tx.execute_batch(sql)
            .map_err(|source| StorageError::Migration {
                version: *version,
                source,
            })?;
        // Not a bound parameter: PRAGMA does not accept one. The value is a
        // compile-time constant from MIGRATIONS, never user input.
        tx.pragma_update(None, "user_version", *version)
            .map_err(|source| StorageError::Migration {
                version: *version,
                source,
            })?;
        tx.commit().map_err(StorageError::from_sqlite)?;
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}

/// True when the file on disk cannot be used as a database at all, as opposed
/// to a transient failure like a busy lock or a full disk.
fn is_corrupt(e: &StorageError) -> bool {
    matches!(
        e,
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            | StorageError::Migration {
                source: rusqlite::Error::SqliteFailure(f, _),
                ..
            }
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            )
    )
}

/// Move a damaged database aside, keeping it for forensics rather than
/// deleting the user's history outright.
fn quarantine(path: &Path) -> Result<PathBuf, StorageError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_extension(format!("corrupt-{stamp}"));
    std::fs::rename(path, &backup).map_err(|source| StorageError::DataDir {
        path: path.display().to_string(),
        source,
    })?;
    // WAL and shared-memory siblings belong to the old file; leaving them would
    // make SQLite try to recover the quarantined database into the new one.
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("netmeter.db");
        (d, p)
    }

    #[test]
    fn opening_creates_the_schema_and_sets_the_version() {
        let (_d, p) = temp();
        let (conn, quarantined) = open_writer(&p).expect("open");
        assert!(quarantined.is_none());
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(v, target_version());
        for table in [
            "interfaces",
            "usage_hour",
            "usage_day",
            "usage_offline",
            "counter_state",
            "meta",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .expect("query");
            assert_eq!(n, 1, "{table} must exist");
        }
    }

    #[test]
    fn the_pragmas_that_matter_are_actually_set() {
        let (_d, p) = temp();
        let (conn, _) = open_writer(&p).expect("open");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("mode");
        assert_eq!(mode.to_lowercase(), "wal");
        let limit: i64 = conn
            .query_row("PRAGMA journal_size_limit", [], |r| r.get(0))
            .expect("limit");
        assert_eq!(limit, 8_388_608, "without this the WAL never shrinks");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .expect("fk");
        assert_eq!(fk, 1, "retention relies on ON DELETE CASCADE");
    }

    #[test]
    fn auto_vacuum_is_actually_enabled_and_not_silently_ignored() {
        // Setting auto_vacuum after journal_mode=WAL reports success and does
        // nothing, leaving the database at NONE -- so `compact()` would never
        // return a byte to the filesystem after a retention prune.
        let (_d, p) = temp();
        let (conn, _) = open_writer(&p).expect("open");
        let av: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .expect("auto_vacuum");
        assert_eq!(av, 2, "expected INCREMENTAL (2), got {av}; pragma order matters");
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let (_d, p) = temp();
        {
            let (conn, _) = open_writer(&p).expect("first open");
            conn.execute(
                "INSERT INTO interfaces(name, kind, first_seen_utc_ms, last_seen_utc_ms) \
                 VALUES ('wlan0','wifi',1,1)",
                [],
            )
            .expect("insert");
        }
        let (conn, quarantined) = open_writer(&p).expect("second open");
        assert!(quarantined.is_none());
        let n: i64 = conn
            .query_row("SELECT count(*) FROM interfaces", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 1, "reopening must not wipe data");
    }

    #[test]
    fn a_corrupt_file_is_quarantined_rather_than_bricking_the_app() {
        let (_d, p) = temp();
        std::fs::write(&p, b"this is definitely not a sqlite database").expect("write garbage");
        let (conn, quarantined) = open_writer(&p).expect("must recover, not fail");
        let backup = quarantined.expect("the bad file is kept for forensics");
        assert!(backup.exists());
        // And the fresh database is usable.
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(v, target_version());
    }

    #[test]
    fn a_missing_parent_directory_is_created() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("nested/deeper/netmeter.db");
        open_writer(&p).expect("must create the tree");
        assert!(p.exists());
    }

    #[test]
    fn a_reader_can_read_while_the_writer_holds_a_transaction() {
        // The property WAL exists for, and the reason commands do not share
        // the writer's connection.
        let (_d, p) = temp();
        let (mut writer, _) = open_writer(&p).expect("open");
        let reader = open_reader(&p).expect("open reader");

        let tx = writer.transaction().expect("begin");
        tx.execute(
            "INSERT INTO interfaces(name, kind, first_seen_utc_ms, last_seen_utc_ms) \
             VALUES ('wlan0','wifi',1,1)",
            [],
        )
        .expect("insert");

        // Uncommitted, so the reader sees the previous snapshot -- and is not
        // blocked.
        let n: i64 = reader
            .query_row("SELECT count(*) FROM interfaces", [], |r| r.get(0))
            .expect("read during write");
        assert_eq!(n, 0);

        tx.commit().expect("commit");
        let n: i64 = reader
            .query_row("SELECT count(*) FROM interfaces", [], |r| r.get(0))
            .expect("read after commit");
        assert_eq!(n, 1);
    }

    #[test]
    fn a_reader_cannot_write() {
        let (_d, p) = temp();
        open_writer(&p).expect("open");
        let reader = open_reader(&p).expect("open reader");
        assert!(
            reader
                .execute("DELETE FROM interfaces", [])
                .is_err(),
            "read-only really must be read-only"
        );
    }

    #[test]
    fn deleting_an_interface_cascades_to_its_usage() {
        let (_d, p) = temp();
        let (conn, _) = open_writer(&p).expect("open");
        conn.execute_batch(
            "INSERT INTO interfaces(id, name, kind, first_seen_utc_ms, last_seen_utc_ms) \
                 VALUES (1,'veth0','enslaved',1,1);
             INSERT INTO usage_day(local_date, interface_id, rx_bytes, tx_bytes) \
                 VALUES ('2026-09-11',1,10,20);
             INSERT INTO counter_state VALUES (1, 7, 1, 2, 3, 'boot');",
        )
        .expect("seed");
        conn.execute("DELETE FROM interfaces WHERE id = 1", [])
            .expect("delete");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM usage_day", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0, "orphan usage rows would grow forever");
    }
}
