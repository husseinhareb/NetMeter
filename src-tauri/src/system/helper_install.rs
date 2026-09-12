//! Installing the per-application helper, with the user\'s consent.
//!
//! One polkit prompt, not a terminal and not a root shell. The script it runs
//! is the same one documented in `packaging/README.md`; the GUI only supplies
//! the escalation.

use crate::core::errors::ConfigError;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the script lands when NetMeter is installed, then the locations a
/// checkout has it, so a development build can install the helper too.
fn script_path() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("/usr/lib/netmeter/install-helper.sh"),
        PathBuf::from("/usr/local/lib/netmeter/install-helper.sh"),
    ];
    if let Some(found) = candidates.iter().find(|p| p.is_file()) {
        return Some(found.clone());
    }

    // A checkout: walk up from the running binary looking for the repository
    // layout. target/debug/netmeter and target/release/netmeter are both four
    // levels below the root.
    let exe = std::env::current_exe().ok()?;
    let mut dir: &Path = exe.parent()?;
    for _ in 0..5 {
        let candidate = dir.join("packaging/install-helper.sh");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
    None
}

/// True when there is a script to run at all.
pub fn is_available() -> bool {
    script_path().is_some()
}

/// Run the installer under pkexec and wait for it.
///
/// Blocking: the caller is a `spawn_blocking` task, and the user is looking at
/// a password prompt for the duration anyway.
pub fn install() -> Result<(), ConfigError> {
    let script = script_path().ok_or_else(|| ConfigError::Write {
        path: "install-helper.sh".into(),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no helper installer found; see docs/PER_APP.md",
        ),
    })?;

    let status = Command::new("pkexec")
        .arg(&script)
        .status()
        .map_err(|source| ConfigError::Write {
            path: "pkexec".into(),
            source,
        })?;

    if status.success() {
        return Ok(());
    }

    // 126 is polkit\'s "not authorised", 127 "pkexec is missing"; both are
    // ordinary outcomes rather than faults, and the message says which.
    let detail = match status.code() {
        Some(126) => "the authentication was dismissed or refused".to_string(),
        Some(127) => "pkexec is not installed".to_string(),
        Some(code) => format!("the installer exited with status {code}"),
        None => "the installer was killed".to_string(),
    };
    Err(ConfigError::Write {
        path: script.display().to_string(),
        source: std::io::Error::other(detail),
    })
}
