//! Which application a pid belongs to.
//!
//! A pid is useless as a historical key -- they recycle -- so every sample is
//! resolved to something stable at the moment it is drained.

use std::fs;
use std::path::Path;

/// Runtimes that run someone else's code. Reporting these is reporting
/// nothing: three Electron apps are not one application called "electron".
const RUNTIMES: [&str; 12] = [
    "electron", "node", "python", "java", "mono", "wine", "bash", "sh", "perl", "ruby", "dotnet",
    "deno",
];

/// Path components that carry no application name.
const NOISE: [&str; 10] = [
    "usr", "opt", "bin", "sbin", "lib", "lib64", "libexec", "local", "share", "app",
];

/// Resolved in this order: flatpak id, snap name, executable (refined when the
/// executable is a runtime or a version number), then the `comm` the kernel
/// recorded when the process first moved bytes. The last one is all that is
/// left for a process that exited between two drains.
pub fn identify(tgid: u32, comm: &str) -> String {
    flatpak_id(tgid)
        .or_else(|| snap_name(tgid))
        .or_else(|| {
            let exe = fs::read_link(format!("/proc/{tgid}/exe")).ok()?;
            name_from(&exe, &cmdline(tgid))
        })
        .unwrap_or_else(|| comm.to_string())
}

/// The naming rules, separated from `/proc` so they can be tested against the
/// cases that actually turned up in a live run.
pub fn name_from(exe: &Path, args: &[String]) -> Option<String> {
    let base = exe.file_name()?.to_str()?;

    if !is_runtime(base) && !is_version(base) {
        return Some(base.to_string());
    }

    // A runtime's first real argument names the program it is running:
    // /usr/lib/electron43/electron /usr/lib/obsidian/app.asar -> obsidian
    if let Some(from_args) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .and_then(|a| meaningful(Path::new(a)))
    {
        return Some(from_args);
    }

    // Otherwise walk up: /opt/teams/2.1.269/2.1.269 -> teams
    meaningful(exe).or_else(|| Some(base.to_string()))
}

/// The first component of a path, read from the end, that names something.
fn meaningful(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    // A bare file is named by its stem; app.asar and main.js are not names.
    if let Some(last) = parts.last() {
        let stem = Path::new(last).file_stem().and_then(|s| s.to_str());
        if let Some(stem) = stem {
            if !is_noise(stem) && !is_version(stem) && !is_runtime(stem) {
                return Some(stem.to_string());
            }
        }
        parts.pop();
    }
    parts
        .into_iter()
        .rev()
        .find(|p| !is_noise(p) && !is_version(p) && !is_runtime(p) && *p != "/")
        .map(str::to_string)
}

fn is_runtime(name: &str) -> bool {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    RUNTIMES.contains(&stem)
}

fn is_version(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn is_noise(name: &str) -> bool {
    NOISE.contains(&name)
}

fn cmdline(tgid: u32) -> Vec<String> {
    fs::read(format!("/proc/{tgid}/cmdline"))
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
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

/// The executable behind a pid, for the `apps` table. Best effort: a process
/// that has already exited has none.
pub fn exe_path(tgid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{tgid}/exe"))
        .ok()
        .map(|p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(exe: &str, args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        name_from(Path::new(exe), &args).expect("a name")
    }

    #[test]
    fn a_plain_program_is_named_by_its_executable() {
        assert_eq!(name("/usr/bin/firefox", &["firefox"]), "firefox");
        assert_eq!(name("/usr/bin/spotify", &[]), "spotify");
    }

    #[test]
    fn a_runtime_is_named_by_what_it_runs() {
        // Observed live: this reported "electron" for Obsidian.
        assert_eq!(
            name(
                "/usr/lib/electron43/electron",
                &["/usr/lib/electron43/electron", "/usr/lib/obsidian/app.asar"]
            ),
            "obsidian"
        );
        assert_eq!(
            name("/usr/bin/python3.14", &["python3", "/home/u/tools/sync.py"]),
            "sync"
        );
    }

    #[test]
    fn a_version_numbered_executable_walks_up_to_the_application() {
        // Observed live: this reported "2.1.269".
        assert_eq!(name("/opt/teams/2.1.269/2.1.269", &[]), "teams");
        assert_eq!(name("/opt/vendor/1.2.3/bin/1.2.3", &[]), "vendor");
    }

    #[test]
    fn flags_are_not_mistaken_for_a_program() {
        assert_eq!(
            name(
                "/usr/lib/electron43/electron",
                &["electron", "--type=zygote", "--no-zygote-sandbox"]
            ),
            "electron"
        );
    }

    #[test]
    fn nothing_useful_still_yields_something() {
        assert_eq!(name("/usr/bin/node", &["node"]), "node");
    }
}
