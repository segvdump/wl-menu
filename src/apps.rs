//! XDG .desktop application scan for the apps menu provider.

use crate::menu::{ItemKind, MenuItem};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const LOG_PREFIX: &str = "wl-menu";

/// Scan installed applications and return menu rows under `apps`.
pub fn scan_apps() -> Vec<MenuItem> {
    let desktop_env = desktop_env_string();
    let mut seen = HashSet::new();
    let mut rows = Vec::new();

    for dir in desktop_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(parsed) = parse_desktop(&path, &desktop_env) else {
                continue;
            };
            if !seen.insert(parsed.id.clone()) {
                continue;
            }
            rows.push(parsed);
        }
    }

    rows.sort_by(|a, b| {
        a.label
            .to_lowercase()
            .cmp(&b.label.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    crate::debug::log(format!("{LOG_PREFIX}: apps provider loaded {} entries", rows.len()));
    rows
}

fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(data_home) = env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    if let Ok(data_dirs) = env::var("XDG_DATA_DIRS") {
        for part in data_dirs.split(':') {
            if !part.is_empty() {
                dirs.push(PathBuf::from(part).join("applications"));
            }
        }
    } else {
        dirs.push(PathBuf::from("/usr/local/share/applications"));
        dirs.push(PathBuf::from("/usr/share/applications"));
    }
    dirs
}

fn desktop_env_string() -> String {
    ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP", "DESKTOP_SESSION"]
        .iter()
        .filter_map(|key| env::var(key).ok())
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>()
        .join(":")
}

fn parse_desktop(path: &Path, desktop_env: &str) -> Option<MenuItem> {
    let text = fs::read_to_string(path).ok()?;
    let map = parse_desktop_kv(&text);
    let entry_type = map.get("Type").map(|s| s.as_str()).unwrap_or("Application");
    if entry_type != "Application" {
        return None;
    }
    if is_truthy(map.get("Hidden")) || is_truthy(map.get("NoDisplay")) {
        return None;
    }
    if !matches_desktop_env(map.get("OnlyShowIn"), map.get("NotShowIn"), desktop_env) {
        return None;
    }
    let exec = map.get("Exec")?.trim();
    if exec.is_empty() {
        return None;
    }
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return None;
    }
    let label = map
        .get("Name")
        .cloned()
        .or_else(|| {
            map.iter()
                .find(|(k, _)| k.starts_with("Name["))
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_else(|| id.clone());
    // Themed names are resolved to PNG/SVG at render time.
    let icon = map.get("Icon").cloned().unwrap_or_default();
    let mut aliases = Vec::new();
    if let Some(generic) = map.get("GenericName") {
        if !generic.is_empty() {
            aliases.push(generic.clone());
        }
    }
    if let Some(keywords) = map.get("Keywords") {
        for word in keywords.split(';').map(str::trim).filter(|w| !w.is_empty()) {
            aliases.push(word.to_string());
        }
    }
    let action = format!(
        "uwsm-app -- gtk-launch {}",
        shell_quote(&format!("{id}.desktop"))
    );
    Some(MenuItem {
        id: format!("apps.{id}"),
        parent: "apps".into(),
        kind: ItemKind::App,
        icon,
        label,
        target: String::new(),
        action,
        confirm: String::new(),
        confirm_text: String::new(),
        provider: String::new(),
        when_expr: String::new(),
        aliases,
        order: 0,
    })
}

fn parse_desktop_kv(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // Prefer the first assignment (unlocalized Name= before Name[xx]=).
        map.entry(key.trim().to_string())
            .or_insert_with(|| unescape_desktop_value(value.trim()));
    }
    map
}

fn unescape_desktop_value(raw: &str) -> String {
    raw.replace("\\s", " ")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r")
        .replace("\\\\", "\\")
}

fn is_truthy(value: Option<&String>) -> bool {
    matches!(
        value.map(|v| v.as_str()),
        Some("true") | Some("1") | Some("yes")
    )
}

fn matches_desktop_env(
    only_show_in: Option<&String>,
    not_show_in: Option<&String>,
    desktop_env: &str,
) -> bool {
    let envs: HashSet<&str> = desktop_env.split(':').filter(|s| !s.is_empty()).collect();
    if let Some(only) = only_show_in {
        let allowed: HashSet<&str> = only
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if !allowed.is_empty() && envs.intersection(&allowed).next().is_none() {
            return false;
        }
    }
    if let Some(not) = not_show_in {
        let blocked: HashSet<&str> = not
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if envs.intersection(&blocked).next().is_some() {
            return false;
        }
    }
    true
}

fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_system_apps() {
        let apps = scan_apps();
        assert!(
            !apps.is_empty(),
            "expected at least one .desktop app under XDG data dirs"
        );
        assert!(apps.iter().all(|a| a.parent == "apps" && a.kind == ItemKind::App));
    }

    #[test]
    fn desktop_keeps_icon_name() {
        let dir = env::temp_dir().join(format!("wl-menu-desktop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wl-menu-icon-test.desktop");
        fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Test\nExec=true\nIcon=utilities-terminal\n",
        )
        .unwrap();
        let item = parse_desktop(&path, "").unwrap();
        assert_eq!(item.icon, "utilities-terminal");
        let _ = fs::remove_dir_all(&dir);
    }
}
