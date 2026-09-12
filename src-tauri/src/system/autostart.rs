//! Starting with the desktop session.
//!
//! An XDG autostart entry, written directly. `tauri-plugin-autostart` exists,
//! but on Linux it writes this same file: a dependency to produce fifteen
//! lines is not worth the supply chain.
//!
//! This matters more than a convenience toggle. A usage meter that only counts
//! while its window is open does not measure a day -- on the machine this was
//! written on, ten hours of one day went unattributed because nothing was
//! watching.

use crate::core::errors::ConfigError;
use std::path::PathBuf;

const FILE: &str = "netmeter.desktop";

fn entry_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join(FILE))
}

pub fn is_enabled() -> bool {
    entry_path().map(|p| p.exists()).unwrap_or(false)
}

/// Write or remove the entry.
///
/// The command points at whatever binary is running, so a development build
/// autostarts itself rather than a release that may not be installed.
pub fn set_enabled(enabled: bool) -> Result<(), ConfigError> {
    let path = entry_path().ok_or_else(|| ConfigError::Write {
        path: "$XDG_CONFIG_HOME".into(),
        source: std::io::Error::other("no config directory"),
    })?;

    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ConfigError::Write {
                path: path.display().to_string(),
                source,
            }),
        };
    }

    let exe = std::env::current_exe().map_err(|source| ConfigError::Write {
        path: "current executable".into(),
        source,
    })?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
            path: dir.display().to_string(),
            source,
        })?;
    }

    std::fs::write(&path, desktop_entry(&exe.display().to_string())).map_err(|source| {
        ConfigError::Write {
            path: path.display().to_string(),
            source,
        }
    })
}

/// `--hidden` so a login does not throw a window in the user's face; the tray
/// icon is how it announces itself.
fn desktop_entry(exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=NetMeter\n\
         Comment=Network usage monitor\n\
         Exec={exec} --hidden\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_starts_hidden_and_points_at_this_binary() {
        let entry = desktop_entry("/usr/bin/netmeter");
        assert!(entry.contains("Exec=/usr/bin/netmeter --hidden"));
        assert!(entry.starts_with("[Desktop Entry]"));
        assert!(entry.ends_with('\n'), "desktop files need a trailing newline");
    }
}
