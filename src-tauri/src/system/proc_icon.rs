use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static ICON_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
static ICON_INDEX: OnceLock<IconIndex> = OnceLock::new();

#[derive(Default)]
struct IconIndex {
    process_to_icon_name: HashMap<String, String>,
    icon_name_to_path: HashMap<String, PathBuf>,
}

fn cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn icon_index() -> &'static IconIndex {
    ICON_INDEX.get_or_init(build_icon_index)
}

/// Resolves a process name or flatpak ID to a base64 image data URL (`data:image/...;base64,...`).
pub fn get_process_icon(name: &str) -> Option<String> {
    let lookup_key = name.to_lowercase();

    if let Ok(guard) = cache().lock() {
        if let Some(cached) = guard.get(&lookup_key) {
            return cached.clone();
        }
    }

    let result = resolve_icon(&lookup_key);

    if let Ok(mut guard) = cache().lock() {
        guard.insert(lookup_key, result.clone());
    }

    result
}

fn resolve_icon(process_name: &str) -> Option<String> {
    let index = icon_index();

    let icon_name = name_candidates(process_name)
        .into_iter()
        .find_map(|candidate| {
            index
                .process_to_icon_name
                .get(&candidate)
                .cloned()
                .or_else(|| {
                    index
                        .icon_name_to_path
                        .contains_key(&candidate)
                        .then_some(candidate)
                })
        })?;

    if icon_name.starts_with('/') {
        let absolute_path = Path::new(&icon_name);
        if absolute_path.exists() {
            return read_as_data_url(absolute_path);
        }
    }

    let icon_path = index.icon_name_to_path.get(&icon_name)?;
    read_as_data_url(icon_path)
}

/// Names to try, most specific first: the process name, its trailing component for
/// reverse-domain IDs (org.mozilla.firefox), then progressively stripped trailing
/// segments so version and hash suffixes fall away (minecraft-26.2-client -> minecraft,
/// webland_server-7dcb4c97 -> webland_server, git-remote-https -> git).
fn name_candidates(process_name: &str) -> Vec<String> {
    let mut candidates = vec![process_name.to_string()];

    if let Some(trailing) = process_name.rsplit('.').next() {
        if trailing != process_name {
            candidates.push(trailing.to_string());
        }
    }

    let mut stem = process_name;
    while let Some((head, _)) = stem.rsplit_once(['-', '_']) {
        if head.is_empty() {
            break;
        }
        candidates.push(head.to_string());
        stem = head;
    }

    candidates
}

fn build_icon_index() -> IconIndex {
    IconIndex {
        process_to_icon_name: build_process_icon_name_index(),
        icon_name_to_path: build_icon_path_index(),
    }
}

fn build_process_icon_name_index() -> HashMap<String, String> {
    let mut index = HashMap::new();

    for dir in desktop_search_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }

            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(mut icon_name) = extract_desktop_field(&content, "Icon") else {
                continue;
            };

            // If the icon is not an absolute path and includes an image extension, strip it.
            if !icon_name.starts_with('/') {
                if let Some(stripped) = icon_name.strip_suffix(".png")
                    .or_else(|| icon_name.strip_suffix(".svg"))
                    .or_else(|| icon_name.strip_suffix(".xpm"))
                {
                    icon_name = stripped.to_string();
                }
            }

            let mut names = HashSet::new();
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                let stem_lower = stem.to_lowercase();
                names.insert(stem_lower.clone());
                if let Some(trailing) = stem_lower.rsplit('.').next() {
                    names.insert(trailing.to_string());
                }
            }
            if let Some(wm_class) = extract_desktop_field(&content, "StartupWMClass") {
                names.insert(wm_class.to_lowercase());
            }
            names.extend(extract_exec_basenames(&content));

            for name in names {
                index.entry(name).or_insert_with(|| icon_name.clone());
            }
        }
    }

    index
}

fn build_icon_path_index() -> HashMap<String, PathBuf> {
    let mut index: HashMap<String, (usize, PathBuf)> = HashMap::new();

    for (priority, base) in preferred_icon_search_roots().into_iter().enumerate() {
        if let Ok(entries) = fs::read_dir(&base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let Some(ext) = path.extension().and_then(|value| value.to_str()) else {
                    continue;
                };
                if !matches!(ext, "png" | "svg" | "jpg" | "jpeg") {
                    continue;
                }

                let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };

                let key = stem.to_lowercase();
                match index.get_mut(&key) {
                    Some((existing_priority, existing_path)) => {
                        if priority < *existing_priority {
                            *existing_priority = priority;
                            *existing_path = path;
                        }
                    }
                    None => {
                        index.insert(key, (priority, path));
                    }
                }
            }
        }
    }

    index
        .into_iter()
        .map(|(icon_name, (_priority, path))| (icon_name, path))
        .collect()
}

fn preferred_icon_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    for base in [
        PathBuf::from("/usr/share/icons/hicolor"),
        PathBuf::from("/usr/local/share/icons/hicolor"),
        PathBuf::from("/var/lib/flatpak/exports/share/icons/hicolor"),
    ] {
        roots.extend(expand_icon_roots(&base));
    }

    if let Ok(home) = std::env::var("HOME") {
        roots.extend(expand_icon_roots(
            &PathBuf::from(&home).join(".local/share/icons/hicolor"),
        ));
        roots.extend(expand_icon_roots(
            &PathBuf::from(&home).join(".local/share/flatpak/exports/share/icons/hicolor"),
        ));
        roots.push(PathBuf::from(&home).join(".local/share/icons"));
        roots.push(PathBuf::from(&home).join(".icons"));
    }

    roots.push(PathBuf::from("/usr/share/icons"));
    roots.push(PathBuf::from("/usr/local/share/icons"));
    roots.push(PathBuf::from("/usr/share/pixmaps"));
    roots.push(PathBuf::from("/usr/local/share/pixmaps"));
    roots.push(PathBuf::from("/var/lib/snapd/desktop/assets"));

    roots
}

fn expand_icon_roots(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };

    let mut sized: Vec<(u32, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let rank = icon_dir_rank(path.file_name()?.to_str()?);
            Some((rank, path))
        })
        .collect();
    sized.sort_by_key(|(rank, _)| *rank);

    sized
        .into_iter()
        .flat_map(|(_, dir)| ["apps", "devices", "mimetypes"].map(|subdir| dir.join(subdir)))
        .collect()
}

/// Ranks a theme size directory, lower first. `scalable` wins outright; sized
/// directories ("48x48", "512x512@2") rank by distance from 256px - large enough to
/// downscale cleanly, small enough that the base64 payload stays reasonable - with the
/// larger of two equidistant sizes preferred. Hardcoding the size list here is what hid
/// icons that a theme only ships at 512x512 or 1024x1024.
fn icon_dir_rank(dir_name: &str) -> u32 {
    if dir_name == "scalable" {
        return 0;
    }

    let (size, scale) = dir_name
        .split_once('@')
        .map_or((dir_name, 1), |(size, scale)| {
            (size, scale.parse().unwrap_or(1))
        });

    let Some(pixels) = size
        .split_once('x')
        .and_then(|(width, _)| width.parse::<u32>().ok())
        .map(|width| width * scale)
    else {
        return u32::MAX; // "symbolic", "index.theme" and anything else unrecognised
    };

    // Downscaling beats upscaling, so undersized icons carry the heavier penalty. A
    // 16x16 still outranks a 1024x1024, whose base64 payload dwarfs the 18px it renders at.
    let distance = pixels.abs_diff(256);
    1 + if pixels >= 256 { distance * 2 } else { distance * 3 }
}

fn desktop_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        PathBuf::from("/var/lib/snapd/desktop/applications"),
    ];

    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/share/applications"));
        dirs.push(PathBuf::from(&home).join(".local/share/flatpak/exports/share/applications"));
    }

    dirs
}

fn extract_exec_basenames(content: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut in_entry = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[Desktop Entry]" {
            in_entry = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_entry = false;
            continue;
        }
        if !in_entry {
            continue;
        }

        if let Some(exec_value) = trimmed.strip_prefix("Exec=") {
            if let Some(name) = parse_exec_basename(exec_value) {
                names.insert(name);
            }
        }
    }

    names
}

fn parse_exec_basename(exec_value: &str) -> Option<String> {
    let tokens = exec_value.split_whitespace();

    for token in tokens {
        if token == "env" {
            continue;
        }
        if token.contains('=') && !token.starts_with('/') {
            continue;
        }

        let basename = Path::new(token)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(token)
            .to_lowercase();

        if !basename.is_empty() {
            return Some(basename);
        }
    }

    None
}

pub fn extract_desktop_field(content: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    let mut in_entry = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[Desktop Entry]" {
            in_entry = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_entry = false;
            continue;
        }
        if in_entry {
            if let Some(value) = trimmed.strip_prefix(&prefix) {
                let value = value.trim().to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }

    None
}

fn read_as_data_url(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let ext = path.extension()?.to_str()?.to_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "xpm" => return None,
        _ => return None,
    };
    Some(format!("data:{mime};base64,{}", STANDARD.encode(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_desktop_field_only_in_main_entry() {
        let content = r#"
[Desktop Action New]
Icon=wrong-action-icon
[Desktop Entry]
Name=Firefox
Exec=firefox %u
Icon=firefox-brand
"#;
        assert_eq!(
            extract_desktop_field(content, "Icon"),
            Some("firefox-brand".into())
        );
        assert_eq!(
            extract_desktop_field(content, "Name"),
            Some("Firefox".into())
        );
    }

    #[test]
    fn parses_exec_basenames_stripping_env() {
        let content = r#"
[Desktop Entry]
Exec=env BAM=1 /usr/lib/chromium/chromium --some-flag
"#;
        let basenames = extract_exec_basenames(content);
        assert!(basenames.contains("chromium"));
    }

    #[test]
    fn strips_version_and_hash_suffixes() {
        assert_eq!(
            name_candidates("minecraft-26.2-client"),
            ["minecraft-26.2-client", "2-client", "minecraft-26.2", "minecraft"]
        );
        assert_eq!(
            name_candidates("org.mozilla.firefox"),
            ["org.mozilla.firefox", "firefox"]
        );
        assert_eq!(name_candidates("firefox"), ["firefox"]);
    }

    #[test]
    fn ranks_scalable_first_then_sizes_nearest_256() {
        let mut dirs = ["16x16", "1024x1024", "512x512", "scalable", "256x256", "symbolic", "128x128"];
        dirs.sort_by_key(|dir| icon_dir_rank(dir));
        assert_eq!(
            dirs,
            ["scalable", "256x256", "128x128", "512x512", "16x16", "1024x1024", "symbolic"]
        );
    }

    #[test]
    fn resolves_system_icon_or_handles_missing_cleanly() {
        assert_eq!(get_process_icon("definitely_nonexistent_process_12345"), None);
        if let Some(icon_url) = get_process_icon("chromium").or_else(|| get_process_icon("steam")) {
            assert!(icon_url.starts_with("data:image/"));
        }
    }
}

