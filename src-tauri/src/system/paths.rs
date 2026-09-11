//! Where NetMeter keeps its files, and the guard that stops two copies of it
//! writing to the same database.

use crate::core::errors::{MonitorError, StorageError};
use std::path::{Path, PathBuf};

pub const DATABASE_FILE: &str = "netmeter.db";
pub const CONFIG_FILE: &str = "config.json";
pub const LOCK_FILE: &str = "netmeter.lock";

/// Ensure a directory exists.
pub fn ensure_dir(dir: &Path) -> Result<(), StorageError> {
    std::fs::create_dir_all(dir).map_err(|source| StorageError::DataDir {
        path: dir.display().to_string(),
        source,
    })
}

/// An exclusive advisory lock held for the lifetime of the process.
///
/// Two NetMeter instances sharing a database is not a crash -- WAL serializes
/// them quite happily -- it is silent double counting: both read the same
/// baseline, both compute the same delta from the same kernel counter, and both
/// add it to the same accumulating row. Every byte ends up counted twice, in
/// the one table that is kept forever.
///
/// `flock` rather than a PID file because the kernel releases it when the
/// process dies, however it dies, so there is no stale-lock case to reason
/// about.
#[derive(Debug)]
pub struct InstanceLock {
    file: std::fs::File,
    path: PathBuf,
}

impl InstanceLock {
    /// Acquire the lock, or report that someone else holds it.
    pub fn acquire(dir: &Path) -> Result<Self, MonitorError> {
        let path = dir.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| MonitorError::Read {
                path: path.display().to_string(),
                source,
            })?;

        // SAFETY: a valid fd owned by `file`, which outlives the call.
        let rc = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EWOULDBLOCK) => Err(MonitorError::AlreadyLocked),
                _ => Err(MonitorError::Read {
                    path: path.display().to_string(),
                    source: e,
                }),
            };
        }
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Released implicitly when the fd closes; unlocking explicitly makes
        // the intent obvious and releases it a moment earlier.
        let _ = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&self.file),
                libc::LOCK_UN,
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_in_the_same_directory_is_refused() {
        let d = tempfile::tempdir().expect("tempdir");
        let first = InstanceLock::acquire(d.path()).expect("first acquires");
        assert!(first.path().exists());
        // The real defect this prevents: a second instance silently doubling
        // every recorded byte.
        assert!(
            matches!(
                InstanceLock::acquire(d.path()),
                Err(MonitorError::AlreadyLocked)
            ),
            "a second instance must be refused, not allowed to double-count"
        );
    }

    #[test]
    fn the_lock_is_released_when_dropped() {
        let d = tempfile::tempdir().expect("tempdir");
        {
            let _first = InstanceLock::acquire(d.path()).expect("first");
        }
        InstanceLock::acquire(d.path()).expect("must be re-acquirable after drop");
    }

    #[test]
    fn different_directories_do_not_contend() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let _la = InstanceLock::acquire(a.path()).expect("a");
        let _lb = InstanceLock::acquire(b.path()).expect("b");
    }

    #[test]
    fn ensure_dir_creates_nested_paths() {
        let d = tempfile::tempdir().expect("tempdir");
        let nested = d.path().join("a/b/c");
        ensure_dir(&nested).expect("creates");
        assert!(nested.is_dir());
        ensure_dir(&nested).expect("idempotent");
    }
}
