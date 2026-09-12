//! The local socket the GUI reads through.
//!
//! A caller only ever sees its own uid's rows, and that uid comes from
//! `SO_PEERCRED` on the connection -- the kernel's word, not the client's. It
//! is the whole access-control story here, which is why it is applied in SQL
//! rather than filtered after the fact.

use netmeter_lib::api::ipc::{read_frame, write_frame, DaemonStatus, Request, Response};
use netmeter_lib::storage::database;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::query;

/// A client that connects and says nothing must not hold a thread forever.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// What the daemon publishes about itself, updated by the sampling loop.
pub type SharedStatus = Arc<Mutex<DaemonStatus>>;

pub struct Server {
    path: PathBuf,
    db: PathBuf,
    timezone: chrono_tz::Tz,
    status: SharedStatus,
}

impl Server {
    pub fn new(path: PathBuf, db: PathBuf, timezone: chrono_tz::Tz, status: SharedStatus) -> Self {
        Self {
            path,
            db,
            timezone,
            status,
        }
    }

    /// Bind and serve until the process ends.
    ///
    /// The socket is world read/write on purpose: peer-uid filtering means a
    /// caller can only reach its own data, so a group would add install
    /// friction without adding protection. Root's rows -- system services --
    /// are visible only to root, which is the same rule applied to uid 0.
    pub fn run(self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // A socket file left by a killed process would refuse the bind. It is
        // safe to remove: if another daemon were live, its lock on the
        // database would have stopped us before this point.
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::warn!(path = %self.path.display(), "removed a stale socket"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        let listener = UnixListener::bind(&self.path)?;
        std::fs::set_permissions(
            &self.path,
            std::os::unix::fs::PermissionsExt::from_mode(0o666),
        )?;
        tracing::info!(path = %self.path.display(), "listening");

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let db = self.db.clone();
                    let tz = self.timezone;
                    let status = Arc::clone(&self.status);
                    // A thread per connection: these are short, infrequent
                    // and few -- one desktop GUI, occasionally a CLI.
                    std::thread::spawn(move || {
                        if let Err(e) = handle(stream, &db, tz, &status) {
                            tracing::debug!(error = %e, "connection ended");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            }
        }
        Ok(())
    }
}

fn handle(
    mut stream: UnixStream,
    db: &Path,
    timezone: chrono_tz::Tz,
    status: &SharedStatus,
) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let uid = peer_uid(&stream)?;

    // One request per connection. Nothing here needs a session, and a
    // connection that cannot outlive its answer cannot leak one.
    let request: Request = read_frame(&mut stream)?;
    let response = answer(request, db, uid, timezone, status);
    write_frame(&mut stream, &response)
}

fn answer(
    request: Request,
    db: &Path,
    uid: u32,
    timezone: chrono_tz::Tz,
    status: &SharedStatus,
) -> Response {
    match request {
        Request::Status => match status.lock() {
            Ok(s) => Response::Status(s.clone()),
            // Poisoned means a previous holder panicked; the contents are
            // still a valid status, so report them rather than refusing.
            Err(e) => Response::Status(e.into_inner().clone()),
        },
        Request::AppUsage {
            granularity,
            from,
            to,
        } => {
            let conn = match database::open_reader(db) {
                Ok(c) => c,
                Err(e) => {
                    return Response::Error {
                        kind: "storage".into(),
                        message: e.to_string(),
                    }
                }
            };
            match query::app_usage(&conn, uid, granularity, &from, &to, timezone) {
                Ok(usage) => Response::AppUsage(usage),
                Err(e) => Response::Error {
                    kind: e.kind().into(),
                    message: e.to_string(),
                },
            }
        }
    }
}

/// The connecting process's uid, from the kernel.
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are sized correctly for SO_PEERCRED on a unix
    // socket, and the fd is owned by the borrowed stream.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            std::ptr::addr_of_mut!(len),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if cred.uid == u32::MAX {
        // Refuse rather than fall back to something permissive: without a uid
        // there is no basis for deciding what this caller may see.
        return Err(io::Error::other("no peer credentials on the connection"));
    }
    Ok(cred.uid)
}
