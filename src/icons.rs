//! Freedesktop icon lookup and pixmap cache for application rows.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tiny_skia::{Pixmap, PixmapPaint, Transform};

/// Process-local decoded icons, keyed by (name, pixel size).
pub struct IconCache {
    map: RefCell<HashMap<(String, u32), Option<Arc<Pixmap>>>>,
    /// Parsed theme search order, filled on first lookup.
    themes: RefCell<Option<Vec<ThemeSearch>>>,
}

#[derive(Clone)]
struct ThemeSearch {
    dir: PathBuf,
    entries: Vec<ThemeDir>,
}

#[derive(Clone)]
struct ThemeDir {
    rel: String,
    size: u32,
    min_size: u32,
    max_size: u32,
    scalable: bool,
}

impl IconCache {
    pub fn new() -> Self {
        Self {
            map: RefCell::new(HashMap::new()),
            themes: RefCell::new(None),
        }
    }

    pub fn clear(&self) {
        self.map.borrow_mut().clear();
        self.themes.borrow_mut().take();
    }

    /// Return a decoded, scaled pixmap for `name`, or None if it is not an image icon.
    pub fn get(&self, name: &str, size: u32) -> Option<Arc<Pixmap>> {
        if name.is_empty() || size == 0 {
            return None;
        }
        let name = name.strip_prefix("theme:").unwrap_or(name).trim();
        let name = name.strip_prefix("file:").unwrap_or(name).trim();
        if name.is_empty() {
            return None;
        }
        let key = (name.to_string(), size);
        if let Some(hit) = self.map.borrow().get(&key) {
            return hit.clone();
        }
        let loaded = load_icon(name, size, self);
        self.map.borrow_mut().insert(key, loaded.clone());
        loaded
    }
}

fn load_icon(name: &str, size: u32, cache: &IconCache) -> Option<Arc<Pixmap>> {
    let path = resolve_icon_path(name, size, cache)?;
    let pixmap = decode_icon(&path, size)?;
    Some(Arc::new(pixmap))
}

/// Resolve a themed name or file path to an icon file on disk.
fn resolve_icon_path(name: &str, size: u32, cache: &IconCache) -> Option<PathBuf> {
    resolve_icon_path_in(
        name,
        size,
        &icon_roots(),
        &pixmap_dirs(),
        cache,
    )
}

fn resolve_icon_path_in(
    name: &str,
    size: u32,
    icon_roots: &[PathBuf],
    pixmap_dirs: &[PathBuf],
    cache: &IconCache,
) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if Path::new(name).is_absolute() {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    if !is_file_or_theme_icon(name) {
        return None;
    }
    let stem = strip_icon_ext(name);
    let themes = theme_search(icon_roots, cache);
    for theme in &themes {
        if let Some(path) = lookup_in_theme(theme, stem, size) {
            return Some(path);
        }
    }
    for dir in pixmap_dirs {
        if let Some(path) = first_existing(dir, stem) {
            return Some(path);
        }
    }
    None
}

/// True when `name` is a path or freedesktop icon name, not a font glyph.
pub fn is_file_or_theme_icon(name: &str) -> bool {
    let name = name
        .strip_prefix("theme:")
        .or_else(|| name.strip_prefix("file:"))
        .unwrap_or(name)
        .trim();
    if name.is_empty() {
        return false;
    }
    if name.contains('/') || name.starts_with("~/") || name.starts_with('$') {
        return true;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Normalize a menu.jsonc `icon` value relative to the menu file directory.
///
/// - `theme:name` -> freedesktop icon name
/// - `file:path` / absolute / `~/...` / paths with `/` -> filesystem path
/// - anything else (nerd font glyph, etc.) -> unchanged
pub fn normalize_menu_icon(raw: &str, base_dir: &Path) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    if let Some(name) = raw.strip_prefix("theme:") {
        return name.trim().to_string();
    }
    if let Some(path) = raw.strip_prefix("file:") {
        return crate::config::expand_path(path.trim(), base_dir)
            .to_string_lossy()
            .into_owned();
    }
    if raw.starts_with('/') || raw.starts_with("~/") || raw.contains('/') {
        return crate::config::expand_path(raw, base_dir)
            .to_string_lossy()
            .into_owned();
    }
    raw.to_string()
}

fn strip_icon_ext(name: &str) -> &str {
    for ext in [".png", ".svg", ".xpm", ".svgz"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name
}

fn icon_roots() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".icons"));
        dirs.push(PathBuf::from(&home).join(".local/share/icons"));
    }
    if let Ok(data_home) = env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("icons"));
    }
    if let Ok(data_dirs) = env::var("XDG_DATA_DIRS") {
        for part in data_dirs.split(':').filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(part).join("icons"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/local/share/icons"));
        dirs.push(PathBuf::from("/usr/share/icons"));
    }
    dirs
}

fn pixmap_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/pixmaps"));
    }
    if let Ok(data_home) = env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("pixmaps"));
    }
    dirs.push(PathBuf::from("/usr/local/share/pixmaps"));
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs
}

fn theme_search(icon_roots: &[PathBuf], cache: &IconCache) -> Vec<ThemeSearch> {
    if let Some(existing) = cache.themes.borrow().as_ref() {
        return existing.clone();
    }
    let mut names = Vec::new();
    if let Some(preferred) = preferred_theme_name() {
        names.push(preferred);
    }
    names.push("hicolor".into());
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < names.len() {
        let name = names[i].clone();
        i += 1;
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(dir) = find_theme_dir(&name, icon_roots) else {
            continue;
        };
        let (entries, inherits) = parse_index_theme(&dir.join("index.theme"));
        ordered.push(ThemeSearch { dir, entries });
        for parent in inherits {
            if !seen.contains(&parent) {
                names.push(parent);
            }
        }
    }
    *cache.themes.borrow_mut() = Some(ordered.clone());
    ordered
}

fn preferred_theme_name() -> Option<String> {
    if let Ok(name) = env::var("WL_MENU_ICON_THEME").or_else(|_| env::var("GTK_ICON_THEME")) {
        if !name.is_empty() {
            return Some(name);
        }
    }
    for rel in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
        if let Some(name) = gtk_icon_theme_from(&xdg_config_home().join(rel)) {
            return Some(name);
        }
    }
    None
}

fn xdg_config_home() -> PathBuf {
    env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

fn gtk_icon_theme_from(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix("gtk-icon-theme-name") else {
            continue;
        };
        let value = value.trim().trim_start_matches('=').trim();
        let value = value.trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn find_theme_dir(name: &str, icon_roots: &[PathBuf]) -> Option<PathBuf> {
    for root in icon_roots {
        let dir = root.join(name);
        if dir.join("index.theme").is_file() || dir.is_dir() {
            return Some(dir);
        }
    }
    None
}

fn parse_index_theme(path: &Path) -> (Vec<ThemeDir>, Vec<String>) {
    let Ok(text) = fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };
    let mut directories = Vec::new();
    let mut inherits = Vec::new();
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = name.to_string();
            sections.entry(current.clone()).or_default();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        sections
            .entry(current.clone())
            .or_default()
            .insert(key.trim().to_string(), value.trim().to_string());
    }
    if let Some(theme) = sections.get("Icon Theme") {
        if let Some(list) = theme.get("Directories") {
            directories = list
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(list) = theme.get("Inherits") {
            inherits = list
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    let mut entries = Vec::new();
    for rel in directories {
        let meta = sections.get(&rel);
        let size = meta
            .and_then(|m| m.get("Size"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(48);
        let min_size = meta
            .and_then(|m| m.get("MinSize"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(size);
        let max_size = meta
            .and_then(|m| m.get("MaxSize"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(size);
        let kind = meta
            .and_then(|m| m.get("Type"))
            .map(|s| s.as_str())
            .unwrap_or("Threshold");
        entries.push(ThemeDir {
            rel,
            size,
            min_size,
            max_size,
            scalable: kind.eq_ignore_ascii_case("Scalable"),
        });
    }
    (entries, inherits)
}

fn lookup_in_theme(theme: &ThemeSearch, stem: &str, size: u32) -> Option<PathBuf> {
    let mut dirs: Vec<&ThemeDir> = theme.entries.iter().collect();
    dirs.sort_by_key(|dir| dir_score(dir, size));
    for dir in dirs {
        if let Some(path) = first_existing(&theme.dir.join(&dir.rel), stem) {
            return Some(path);
        }
    }
    first_existing(&theme.dir, stem)
}

fn dir_score(dir: &ThemeDir, target: u32) -> (u8, u32) {
    if dir.scalable && target >= dir.min_size && target <= dir.max_size {
        return (1, 0);
    }
    if dir.size == target {
        return (0, 0);
    }
    (2, dir.size.abs_diff(target))
}

fn first_existing(dir: &Path, stem: &str) -> Option<PathBuf> {
    for ext in ["png", "svg"] {
        let path = dir.join(format!("{stem}.{ext}"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn decode_icon(path: &Path, size: u32) -> Option<Pixmap> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let pixmap = match ext.as_str() {
        "png" => Pixmap::load_png(path).ok()?,
        "svg" => load_svg(path, size)?,
        _ => return None,
    };
    Some(scale_to(pixmap, size))
}

fn load_svg(path: &Path, size: u32) -> Option<Pixmap> {
    let data = fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let tree_size = tree.size();
    let tw = tree_size.width().max(1.0);
    let th = tree_size.height().max(1.0);
    let mut pixmap = Pixmap::new(size, size)?;
    let scale = size as f32 / tw.max(th);
    let tx = (size as f32 - tw * scale) * 0.5;
    let ty = (size as f32 - th * scale) * 0.5;
    resvg::render(
        &tree,
        Transform::from_row(scale, 0.0, 0.0, scale, tx, ty),
        &mut pixmap.as_mut(),
    );
    Some(pixmap)
}

fn scale_to(pixmap: Pixmap, size: u32) -> Pixmap {
    if pixmap.width() == size && pixmap.height() == size {
        return pixmap;
    }
    let mut out = match Pixmap::new(size, size) {
        Some(p) => p,
        None => return pixmap,
    };
    let pw = pixmap.width() as f32;
    let ph = pixmap.height() as f32;
    let scale = size as f32 / pw.max(ph).max(1.0);
    let tx = (size as f32 - pw * scale) * 0.5;
    let ty = (size as f32 - ph * scale) * 0.5;
    out.draw_pixmap(
        0,
        0,
        pixmap.as_ref(),
        &PixmapPaint::default(),
        Transform::from_row(scale, 0.0, 0.0, scale, tx, ty),
        None,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_are_not_theme_names() {
        assert!(!is_file_or_theme_icon("󰀻"));
        assert!(is_file_or_theme_icon("firefox"));
        assert!(is_file_or_theme_icon("theme:firefox"));
        assert!(is_file_or_theme_icon("file:/tmp/a.png"));
        assert!(is_file_or_theme_icon("org.kde.dolphin"));
        assert!(is_file_or_theme_icon("/usr/share/pixmaps/foo.png"));
    }

    #[test]
    fn normalize_menu_icon_prefixes() {
        let base = PathBuf::from("/cfg/wl-menu");
        assert_eq!(normalize_menu_icon("theme:folder", &base), "folder");
        assert_eq!(
            normalize_menu_icon("file:icons/a.png", &base),
            "/cfg/wl-menu/icons/a.png"
        );
        assert_eq!(normalize_menu_icon("󰀻", &base), "󰀻");
        assert_eq!(normalize_menu_icon("firefox", &base), "firefox");
    }

    #[test]
    fn finds_png_in_hicolor_apps_dir() {
        let root = env::temp_dir().join(format!("wl-menu-icons-{}", std::process::id()));
        let apps = root.join("hicolor/48x48/apps");
        fs::create_dir_all(&apps).unwrap();
        fs::write(
            root.join("hicolor/index.theme"),
            "[Icon Theme]\nDirectories=48x48/apps\n\n[48x48/apps]\nSize=48\nType=Fixed\n",
        )
        .unwrap();
        let mut pix = Pixmap::new(48, 48).unwrap();
        pix.fill(tiny_skia::Color::from_rgba8(10, 20, 30, 255));
        pix.save_png(apps.join("wl-menu-test-app.png")).unwrap();

        let cache = IconCache::new();
        *cache.themes.borrow_mut() = Some(vec![ThemeSearch {
            dir: root.join("hicolor"),
            entries: vec![ThemeDir {
                rel: "48x48/apps".into(),
                size: 48,
                min_size: 48,
                max_size: 48,
                scalable: false,
            }],
        }]);
        let path = resolve_icon_path_in(
            "wl-menu-test-app",
            32,
            &[root.clone()],
            &[],
            &cache,
        );
        assert_eq!(path, Some(apps.join("wl-menu-test-app.png")));
        let _ = fs::remove_dir_all(&root);
    }
}
