//! Which application a pid belongs to.
//!
//! A pid is useless as a historical key -- they recycle -- so every sample is
//! resolved to something stable at the moment it is drained.

use std::fs;
use std::path::PathBuf;

/// Resolved in this order: flatpak id, snap name, executable path, then the
/// `comm` the kernel recorded when the process first moved bytes. The last one
/// is all that is left for a process that exited between two drains.
pub fn identify(tgid: u32, comm: &str) -> String {
    flatpak_id(tgid)
        .or_else(|| snap_name(tgid))
        .or_else(|| exe_name(tgid))
        .unwrap_or_else(|| comm.to_string())
}

fn flatpak_id(tgid: u32) -> Option<String> {
    // Readable only because this process is privileged; it is the sandbox's
    // own manifest, seen from outside.
    let info = fs::read_to_string(format!("/proc/{tgid}/root/.flatpak-info")).ok()?;
    let mut in_app = false;
    for line in info.lines() {
        if line.starts_with('[') {
            in_app = line == "[Application]";
        } else if in_app {
            if let Some(name) = line.strip_prefix("name=") {
                return Some(name.trim().to_string());
            }
        }
    }
    None
}

fn snap_name(tgid: u32) -> Option<String> {
    let cgroup = fs::read_to_string(format!("/proc/{tgid}/cgroup")).ok()?;
    let at = cgroup.find("snap.")?;
    let rest = &cgroup[at + 5..];
    let end = rest.find(['.', '/', '\n'])?;
    Some(rest[..end].to_string())
}

fn exe_name(tgid: u32) -> Option<String> {
    let exe: PathBuf = fs::read_link(format!("/proc/{tgid}/exe")).ok()?;
    exe.file_name()?.to_str().map(str::to_string)
}
