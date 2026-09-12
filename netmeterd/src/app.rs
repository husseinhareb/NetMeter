//! Which application a pid belongs to.
//!
//! A pid is useless as a historical key -- they recycle -- so every sample is
//! resolved to something stable at the moment it is drained.

use std::fs;
use std::path::{Path, PathBuf};

/// Runtimes that run someone else's code. Reporting these is reporting
/// nothing: three Electron apps are not one application called "electron".
const RUNTIMES: [&str; 16] = [
    "electron", "node", "python", "java", "mono", "wine", "bash", "sh", "perl", "ruby", "dotnet",
    "deno",
    // Proton ships a game's traffic through these; the game is up the tree.
    "wine-preloader", "wine64-preloader", "wineserver", "reaper",
];

/// Path components that carry no application name.
const NOISE: [&str; 14] = [
    "usr", "opt", "bin", "sbin", "lib", "lib64", "libexec", "local", "share", "app",
    // Where a versioned install puts its version: .../claude/versions/2.1.269
    "versions", "current", "node_modules", "files",
];

/// Resolved in this order: flatpak id, snap name, executable (refined when the
/// executable is a runtime or a version number), then the `comm` the kernel
/// recorded when the process first moved bytes. The last one is all that is
/// left for a process that exited between two drains.
pub fn identify(tgid: u32, comm: &str) -> String {
    flatpak_id(tgid)
        .or_else(|| snap_name(tgid))
        .or_else(|| inherited_name(tgid))
        .or_else(|| {
            // Nothing specific anywhere up the tree: the runtime's own name is
            // still better than nothing.
            let exe = fs::read_link(format!("/proc/{tgid}/exe")).ok()?;
            Some(exe.file_name()?.to_str()?.to_string())
        })
        .unwrap_or_else(|| comm.to_string())
}

/// How far up the process tree to look for a name.
///
/// An Electron helper is one hop from the app; a Proton game sits under a
/// launcher under a reaper under Steam. Four covers both without wandering
/// into systemd.
const MAX_PARENTS: usize = 4;

/// The first specific name found at this pid or above it.
///
/// Electron helpers carry only `--type=zygote`, and Proton's preloaders are
/// named after wine, so the process that moved the bytes frequently cannot
/// name itself. Its parent can.
fn inherited_name(tgid: u32) -> Option<String> {
    let mut pid = tgid;
    for _ in 0..=MAX_PARENTS {
        let args = cmdline(pid);
        let path = program_path(pid, &args);
        if let Some(name) = path.as_deref().and_then(|e| specific_name(e, &args)) {
            return Some(name);
        }
        match parent_of(pid) {
            // pid 1 is init: past there is the system, not the application.
            Some(parent) if parent > 1 => pid = parent,
            _ => break,
        }
    }
    None
}

fn parent_of(pid: u32) -> Option<u32> {
    // Field 4 of /proc/pid/stat, read after the last ')' because comm can
    // contain spaces and parentheses.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// The naming rules, separated from `/proc` so they can be tested against the
/// cases that actually turned up in a live run.
///
/// `None` means this process cannot name an application -- it is a runtime, or
/// a bare version number, with nothing better anywhere in its path or
/// arguments. The caller then tries the parent.
pub fn specific_name(exe: &Path, args: &[String]) -> Option<String> {
    // A process that re-executes itself has argv[0] "/proc/self/exe", which
    // names nothing -- observed live as an application called "exe". Anything
    // under /proc is a self-reference, so ask the parent instead.
    if exe.starts_with("/proc") {
        return None;
    }
    let base = exe.file_name()?.to_str()?;

    if !is_runtime(base) && !is_version(base) {
        return Some(base.to_string());
    }

    // A runtime's first real argument names the program it is running:
    // /usr/lib/electron43/electron /usr/lib/obsidian/app.asar -> obsidian
    if let Some(from_args) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-') && looks_like_path(a))
        .and_then(|a| meaningful(Path::new(a)))
    {
        return Some(from_args);
    }

    // A version-numbered executable is the application, installed under its
    // own name: /opt/teams/2.1.269/2.1.269 -> teams. A runtime is not -- its
    // path describes where the runtime lives, which is why walking it gave
    // "i386-unix" for a Proton game. For those, say nothing and let the
    // parent answer.
    if is_version(base) {
        return meaningful(exe);
    }
    None
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
        .map(|raw| split_cmdline(&raw))
        .unwrap_or_default()
}

/// `/proc/pid/cmdline` is documented as NUL-separated, but Electron and
/// Chrome rewrite their own argv into a single space-separated blob, so
/// Obsidian arrived as one argument and its app path was never seen. Handle
/// both shapes.
pub fn split_cmdline(raw: &[u8]) -> Vec<String> {
    let parts: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();

    match parts.as_slice() {
        [one] if one.contains(' ') => one.split_whitespace().map(str::to_string).collect(),
        _ => parts,
    }
}

/// Whether an argument is plausibly a path to the program being run, rather
/// than code or a value. `bash -c "source /x/y && ..."` otherwise named a
/// process after the last path inside a shell command.
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/') && !arg.chars().any(char::is_whitespace)
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
    program_path(tgid, &cmdline(tgid)).map(|p| p.display().to_string())
}

/// The best path we can get for a process, without asking for privileges we
/// should not have.
///
/// `/proc/<pid>/exe` is gated by `PTRACE_MODE_READ`, so reading another user's
/// is refused unless the caller holds `CAP_SYS_PTRACE` -- which this daemon
/// deliberately does not, because that capability also grants reading any
/// process's memory. Measured on the installed service: every lookup failed
/// and every application was named after a *thread* (`IOCP Thread 0`,
/// `tokio-rt-worker`), because the only thing left was the kernel's `comm`.
///
/// `/proc/<pid>/cmdline` is mode 444 and not ptrace-gated, and its first
/// argument is the program path for practically everything. Same answer, no
/// capability.
fn program_path(pid: u32, args: &[String]) -> Option<PathBuf> {
    path_from(fs::read_link(format!("/proc/{pid}/exe")).ok(), args)
}

/// Split out from `/proc` so the fallback itself can be tested.
fn path_from(exe: Option<PathBuf>, args: &[String]) -> Option<PathBuf> {
    if let Some(exe) = exe {
        return Some(exe);
    }
    let argv0 = args.first()?;
    // A login shell is "-bash", and a process can set argv[0] to anything;
    // neither is a path, but both are still better than a thread name.
    (!argv0.is_empty()).then(|| PathBuf::from(argv0.trim_start_matches('-')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(exe: &str, args: &[&str]) -> Option<String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        specific_name(Path::new(exe), &args)
    }

    /// What `identify` does with a process tree, without needing `/proc`:
    /// the first specific name at or above the process that moved the bytes.
    fn first_named(chain: &[(&str, &[&str])]) -> Option<String> {
        chain
            .iter()
            .take(MAX_PARENTS + 1)
            .find_map(|(exe, args)| name(exe, args))
    }

    #[test]
    fn a_plain_program_is_named_by_its_executable() {
        assert_eq!(name("/usr/bin/firefox", &["firefox"]).as_deref(), Some("firefox"));
        assert_eq!(name("/usr/bin/spotify", &[]).as_deref(), Some("spotify"));
    }

    #[test]
    fn a_runtime_is_named_by_what_it_runs() {
        // Observed live: this reported "electron" for Obsidian.
        assert_eq!(
            name(
                "/usr/lib/electron43/electron",
                &["/usr/lib/electron43/electron", "/usr/lib/obsidian/app.asar"]
            )
            .as_deref(),
            Some("obsidian")
        );
        assert_eq!(
            name("/usr/bin/python3.14", &["python3", "/home/u/tools/sync.py"]).as_deref(),
            Some("sync")
        );
    }

    #[test]
    fn a_version_numbered_executable_walks_up_to_the_application() {
        // Observed live: this reported "2.1.269".
        assert_eq!(name("/opt/teams/2.1.269/2.1.269", &[]).as_deref(), Some("teams"));
        assert_eq!(
            name("/opt/vendor/1.2.3/bin/1.2.3", &[]).as_deref(),
            Some("vendor")
        );
    }

    #[test]
    fn a_versions_directory_is_not_a_name() {
        // Observed live: this reported "versions".
        assert_eq!(
            name("/home/u/.local/share/claude/versions/2.1.269", &[]).as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn an_argv_rewritten_into_one_blob_is_still_split() {
        // Observed live: Obsidian's whole command line arrives as a single
        // NUL-terminated string with spaces in it.
        let raw = b"/usr/lib/electron43/electron /usr/lib/obsidian/app.asar\0";
        let args = split_cmdline(raw);
        assert_eq!(args.len(), 2, "{args:?}");
        assert_eq!(
            specific_name(Path::new("/usr/lib/electron43/electron"), &args).as_deref(),
            Some("obsidian")
        );
    }

    #[test]
    fn properly_separated_arguments_are_left_alone() {
        let raw = b"/bin/bash\0-c\0echo hello there\0";
        assert_eq!(split_cmdline(raw), vec!["/bin/bash", "-c", "echo hello there"]);
    }

    #[test]
    fn shell_code_is_not_mistaken_for_a_program_path() {
        // Observed live: a `bash -c` process was named "claude-7ae7-cwd"
        // after a redirect target inside the script.
        let args = split_cmdline(b"/bin/bash\0-c\0source /tmp/snap.sh && pwd >| /tmp/x-cwd\0");
        assert_eq!(specific_name(Path::new("/usr/bin/bash"), &args), None);
    }

    #[test]
    fn a_process_that_cannot_name_anything_says_so() {
        // Nothing specific here, so `identify` moves to the parent.
        assert_eq!(
            name(
                "/usr/lib/electron43/electron",
                &["electron", "--type=zygote", "--no-zygote-sandbox"]
            ),
            None
        );
        assert_eq!(name("/usr/bin/node", &["node"]), None);
    }

    #[test]
    fn a_name_survives_exe_being_unreadable() {
        // Observed on the installed service: with only CAP_BPF and
        // CAP_PERFMON, /proc/<pid>/exe is refused for other users' processes,
        // so argv[0] has to carry the name.
        let args: Vec<String> = ["/usr/lib/firefox/firefox", "-contentproc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let path = path_from(None, &args).expect("a path from argv[0]");
        assert_eq!(specific_name(&path, &args).as_deref(), Some("firefox"));
    }

    #[test]
    fn a_self_referencing_path_is_not_a_name() {
        // Observed live: an application row called "exe".
        assert_eq!(
            specific_name(Path::new("/proc/self/exe"), &["/proc/self/exe".to_string()]),
            None
        );
    }

    #[test]
    fn a_login_shells_leading_dash_is_not_part_of_its_name() {
        let args = vec!["-bash".to_string()];
        let path = path_from(None, &args).expect("a path");
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("bash"));
    }

    #[test]
    fn a_readable_exe_still_wins() {
        let args = vec!["/weird/argv0".to_string()];
        let exe = Some(PathBuf::from("/usr/bin/spotify"));
        assert_eq!(path_from(exe, &args), Some(PathBuf::from("/usr/bin/spotify")));
    }

    #[test]
    fn a_helper_inherits_its_parents_name() {
        // Observed live: Obsidian's helpers were counted as "electron".
        let chain: &[(&str, &[&str])] = &[
            ("/usr/lib/electron43/electron", &["electron", "--type=zygote"]),
            (
                "/usr/lib/electron43/electron",
                &["electron", "/usr/lib/obsidian/app.asar"],
            ),
        ];
        assert_eq!(first_named(chain).as_deref(), Some("obsidian"));
    }

    #[test]
    fn a_proton_game_is_not_named_after_wine() {
        // Observed live: 566 KiB went to "wine64-preloader".
        let steam = "/home/u/.local/share/Steam/ubuntu12_32/steam";
        let chain: &[(&str, &[&str])] = &[
            (
                "/home/u/.local/share/Steam/steamapps/common/Proton - Experimental/files/lib/wine/i386-unix/wine64-preloader",
                &[],
            ),
            ("/usr/bin/reaper", &["reaper"]),
            (steam, &["steam"]),
        ];
        assert_eq!(first_named(chain).as_deref(), Some("steam"));
    }

    #[test]
    fn the_walk_is_bounded() {
        // A chain of nothing but runtimes yields nothing, rather than walking
        // to pid 1 and calling the answer "systemd".
        let chain: &[(&str, &[&str])] = &[
            ("/usr/bin/node", &["node"]),
            ("/usr/bin/node", &["node"]),
            ("/usr/bin/node", &["node"]),
            ("/usr/bin/node", &["node"]),
            ("/usr/bin/node", &["node"]),
            ("/usr/bin/obsidian", &["obsidian"]),
        ];
        assert_eq!(first_named(chain), None, "stops before the sixth entry");
    }
}
