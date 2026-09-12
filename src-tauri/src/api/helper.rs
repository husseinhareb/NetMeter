//! Talking to `netmeterd` over its local socket.
//!
//! Per-application accounting needs privileges the GUI does not have, so that
//! data comes from a helper service (see `docs/PER_APP.md`). The helper is
//! optional: everything else in NetMeter works without it, and this module's
//! job is to make "not installed" an ordinary answer rather than an error.

use crate::api::ipc::{self, read_frame, write_frame, AppUsage, DaemonStatus, Request, Response};
use crate::core::types::Granularity;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Short: the helper is on the same machine, and a GUI waiting on a socket is
/// a GUI that looks broken.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What the GUI knows about the helper.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HelperState {
    /// Running, with its own report of itself.
    Running(Box<DaemonStatus>),
    /// Nothing is listening. Expected, and not an error: the helper is opt-in.
    NotInstalled,
    /// Listening, but the conversation failed. Worth showing, because it means
    /// something is wrong rather than absent.
    Unreachable { message: String },
}

fn socket_path() -> PathBuf {
    // Overridable so a developer can run the helper without installing it.
    std::env::var_os("NETMETERD_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(ipc::SOCKET_PATH))
}

fn ask(request: &Request) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

/// Whether the helper is there, without making its absence look like a fault.
pub fn state() -> HelperState {
    match ask(&Request::Status) {
        Ok(Response::Status(s)) => HelperState::Running(Box::new(s)),
        Ok(Response::Error { message, .. }) => HelperState::Unreachable { message },
        Ok(other) => HelperState::Unreachable {
            message: format!("unexpected reply: {other:?}"),
        },
        Err(e) if is_absent(&e) => HelperState::NotInstalled,
        Err(e) => HelperState::Unreachable {
            message: e.to_string(),
        },
    }
}

/// No socket file, or nothing accepting on it. Both mean "not installed"
/// rather than "broken", and the GUI shows an invitation instead of an error.
fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Per-application usage for a range, or an explanation of why not.
pub fn app_usage(
    granularity: Granularity,
    from: String,
    to: String,
) -> Result<AppUsage, HelperError> {
    match ask(&Request::AppUsage {
        granularity,
        from,
        to,
    }) {
        Ok(Response::AppUsage(usage)) => Ok(usage),
        Ok(Response::Error { kind, message }) => Err(HelperError { kind, message }),
        Ok(other) => Err(HelperError {
            kind: "internal".into(),
            message: format!("unexpected reply: {other:?}"),
        }),
        Err(e) if is_absent(&e) => Err(HelperError {
            kind: "not_installed".into(),
            message: "the per-application helper is not running".into(),
        }),
        Err(e) => Err(HelperError {
            kind: "unreachable".into(),
            message: e.to_string(),
        }),
    }
}

/// Mirrors the shape the Tauri commands already reject with, so the frontend
/// branches on `kind` and never on message text.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HelperError {
    pub kind: String,
    pub message: String,
}

impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_socket_reads_as_not_installed_rather_than_an_error() {
        // Nothing is listening on this path, which is the normal case for a
        // machine where the helper was never installed.
        temp_env_socket("/nonexistent/netmeter/does-not-exist.sock");
        assert_eq!(state(), HelperState::NotInstalled);

        let err = app_usage(Granularity::Day, "2026-09-12".into(), "2026-09-12".into())
            .expect_err("must fail");
        assert_eq!(err.kind, "not_installed");
    }

    fn temp_env_socket(path: &str) {
        // SAFETY: the test process is single-threaded at this point, and the
        // variable is only read by this module.
        unsafe { std::env::set_var("NETMETERD_SOCKET", path) };
    }
}
