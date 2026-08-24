//! Theme, layout, and path config for wl-menu (TOML load, includes, color parsing).

use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tiny_skia::Color;

const LOG_PREFIX: &str = "wl-menu";

/// Seed written to `$XDG_CONFIG_HOME/wl-menu/wl-menu.toml` on first start.
const DEFAULT_CONFIG: &str = r#"# Default config written on first start.
# include files merge first; keys in this file win.
#
# include = [
#   "$XDG_CONFIG_HOME/wl-menu/theme.toml",
# ]
#
# menu = "menu.jsonc"

[layout]
width = 420
row_height = 36
padding = 12
max_rows = 12
border_radius = 10

[font]
family = "Hack Nerd Font Mono"
size = 16

[input]
repeat_delay_ms = 400
repeat_rate = 30

# Initial focus in confirm dialog: "cancel" (safe) or "action"
# [confirm]
# default = "cancel"

[theme]
scrim = "rgba(11, 12, 22, 0.5)"
background = "rgba(11, 12, 22, 0.95)"
text = "rgba(221, 247, 255, 255)"
border = "rgba(130, 250, 155, 89)"
selected_background = "rgba(221, 247, 255, 20)"
selected_text = "rgba(130, 250, 155, 255)"
"#;


/// Resolved menu settings (theme, layout, font, input, paths).
#[derive(Clone, Debug)]
pub struct Config {
    pub theme: Theme,
    pub layout: Layout,
    pub font: FontConfig,
    pub input: InputConfig,
    pub confirm: ConfirmConfig,
    /// Primary config file used on load and reload.
    pub source_path: Option<PathBuf>,
    /// JSONC menu tree path.
    pub menu_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub scrim: Color,
    pub background: Color,
    pub text: Color,
    pub border: Color,
    pub selected_background: Color,
    pub selected_text: Color,
    /// Optional override for `layout.border_radius`.
    pub border_radius: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub width: f32,
    pub row_height: f32,
    pub padding: f32,
    pub max_rows: usize,
    /// Corner radius for the menu card and selection highlights.
    pub border_radius: f32,
}

#[derive(Clone, Debug)]
pub struct FontConfig {
    pub family: String,
    pub size: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct InputConfig {
    pub repeat_delay_ms: u32,
    pub repeat_rate: u32,
}

/// Initial focus when an action opens the confirm dialog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConfirmDefault {
    /// Focus Cancel (index 1). Safer for destructive actions.
    #[default]
    Cancel,
    /// Focus the confirming action (index 0).
    Action,
}

impl ConfirmDefault {
    /// Row index in the confirm dialog (0 = action, 1 = cancel).
    pub fn selected_index(self) -> usize {
        match self {
            Self::Action => 0,
            Self::Cancel => 1,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cancel" | "no" => Some(Self::Cancel),
            "action" | "yes" | "confirm" => Some(Self::Action),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ConfirmConfig {
    pub default: ConfirmDefault,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme {
                scrim: parse_rgba("rgba(11, 12, 22, 0.5)").expect("scrim"),
                background: parse_rgba("rgba(11, 12, 22, 0.95)").expect("background"),
                text: parse_rgba("rgba(221, 247, 255, 255)").expect("text"),
                border: parse_rgba("rgba(130, 250, 155, 89)").expect("border"),
                selected_background: parse_rgba("rgba(221, 247, 255, 20)").expect("selected bg"),
                selected_text: parse_rgba("rgba(130, 250, 155, 255)").expect("selected text"),
                border_radius: None,
            },
            layout: Layout {
                width: 420.0,
                row_height: 36.0,
                padding: 12.0,
                max_rows: 12,
                border_radius: 10.0,
            },
            font: FontConfig {
                family: "Hack Nerd Font Mono".into(),
                size: 16.0,
            },
            input: InputConfig {
                repeat_delay_ms: 400,
                repeat_rate: 30,
            },
            confirm: ConfirmConfig::default(),
            source_path: None,
            menu_path: default_menu_path(),
        }
    }
}

impl Config {
    /// Load defaults, optional `include` files, then the primary file (local wins).
    /// When `path` is omitted and the default file is missing, write a seed copy.
    pub fn load(path: Option<&Path>) -> Self {
        let explicit = path.is_some();
        let source = path.map(PathBuf::from).unwrap_or_else(default_config_path);
        if !explicit {
            ensure_config_file(&source);
        }
        let base_dir = source
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut cfg = Self::default();
        cfg.source_path = Some(source.clone());
        cfg.menu_path = base_dir.join("menu.jsonc");

        let Some(file) = read_file_config(&source) else {
            crate::debug::log(format!(
                "config: source={} missing_or_invalid menu={}",
                source.display(),
                cfg.menu_path.display()
            ));
            return cfg;
        };

        let mut visited = HashSet::new();
        mark_visited(&source, &mut visited);
        let mut include_paths = Vec::new();
        if let Some(includes) = &file.include {
            for raw in includes {
                let inc_path = expand_path(raw, &base_dir);
                include_paths.push(inc_path.display().to_string());
                cfg = merge_included(cfg, &inc_path, &mut visited);
            }
        }
        let cfg = cfg.merge(file, &base_dir);
        crate::debug::log(format!(
            "config: source={} includes={:?} menu={}",
            source.display(),
            include_paths,
            cfg.menu_path.display()
        ));
        cfg
    }

    /// Corner radius used for the card and selection rows (`theme` wins over `layout`).
    pub fn border_radius(&self) -> f32 {
        self.theme
            .border_radius
            .unwrap_or(self.layout.border_radius)
            .max(0.0)
    }

    fn merge(mut self, file: FileConfig, base_dir: &Path) -> Self {
        if let Some(menu) = file.menu {
            self.menu_path = expand_path(&menu, base_dir);
        }
        if let Some(theme) = file.theme {
            self.theme = self.theme.merge(theme);
        }
        if let Some(layout) = file.layout {
            self.layout = self.layout.merge(layout);
        }
        if let Some(font) = file.font {
            self.font = self.font.merge(font);
        }
        if let Some(input) = file.input {
            self.input = self.input.merge(input);
        }
        if let Some(confirm) = file.confirm {
            self.confirm = self.confirm.merge(confirm);
        }
        self
    }
}

/// Merge an included TOML file (its own includes first, then its fields).
fn merge_included(mut cfg: Config, path: &Path, visited: &mut HashSet<PathBuf>) -> Config {
    if !mark_visited(path, visited) {
        return cfg;
    }
    let Some(file) = read_file_config(path) else {
        if path.exists() {
            return cfg;
        }
        eprintln!("{LOG_PREFIX}: include missing {}", path.display());
        return cfg;
    };
    let base = path.parent().unwrap_or(Path::new("."));
    if let Some(includes) = &file.include {
        for raw in includes {
            let inc_path = expand_path(raw, base);
            cfg = merge_included(cfg, &inc_path, visited);
        }
    }
    cfg.merge(file, base)
}

/// Record `path` in `visited`. Returns false when it was already seen.
fn mark_visited(path: &Path, visited: &mut HashSet<PathBuf>) -> bool {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    visited.insert(key)
}

impl Theme {
    fn merge(self, file: ThemeFile) -> Self {
        Self {
            scrim: parse_color_field(file.scrim, self.scrim),
            background: parse_color_field(file.background, self.background),
            text: parse_color_field(file.text, self.text),
            border: parse_color_field(file.border, self.border),
            selected_background: parse_color_field(file.selected_background, self.selected_background),
            selected_text: parse_color_field(file.selected_text, self.selected_text),
            border_radius: file.border_radius.or(self.border_radius),
        }
    }
}

impl Layout {
    fn merge(self, file: LayoutFile) -> Self {
        Self {
            width: file.width.unwrap_or(self.width),
            row_height: file.row_height.unwrap_or(self.row_height),
            padding: file.padding.unwrap_or(self.padding),
            max_rows: file.max_rows.unwrap_or(self.max_rows),
            border_radius: file.border_radius.unwrap_or(self.border_radius),
        }
    }
}

impl FontConfig {
    fn merge(self, file: FontFile) -> Self {
        Self {
            family: file.family.unwrap_or(self.family),
            size: file.size.unwrap_or(self.size),
        }
    }
}

impl InputConfig {
    fn merge(self, file: InputFile) -> Self {
        Self {
            repeat_delay_ms: file.repeat_delay_ms.unwrap_or(self.repeat_delay_ms),
            repeat_rate: file.repeat_rate.unwrap_or(self.repeat_rate),
        }
    }
}

impl ConfirmConfig {
    fn merge(self, file: ConfirmFile) -> Self {
        let default = file
            .default
            .as_deref()
            .and_then(ConfirmDefault::parse)
            .unwrap_or(self.default);
        Self { default }
    }
}

#[derive(Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    include: Option<Vec<String>>,
    menu: Option<String>,
    theme: Option<ThemeFile>,
    layout: Option<LayoutFile>,
    font: Option<FontFile>,
    input: Option<InputFile>,
    confirm: Option<ConfirmFile>,
}

#[derive(Default, Deserialize)]
struct ThemeFile {
    scrim: Option<String>,
    background: Option<String>,
    text: Option<String>,
    border: Option<String>,
    selected_background: Option<String>,
    selected_text: Option<String>,
    border_radius: Option<f32>,
}

#[derive(Default, Deserialize)]
struct LayoutFile {
    width: Option<f32>,
    row_height: Option<f32>,
    padding: Option<f32>,
    max_rows: Option<usize>,
    border_radius: Option<f32>,
}

#[derive(Default, Deserialize)]
struct FontFile {
    family: Option<String>,
    size: Option<f32>,
}

#[derive(Default, Deserialize)]
struct InputFile {
    repeat_delay_ms: Option<u32>,
    repeat_rate: Option<u32>,
}

#[derive(Default, Deserialize)]
struct ConfirmFile {
    /// `"cancel"` or `"action"` (also accepts `no` / `yes` / `confirm`).
    default: Option<String>,
}

/// `$XDG_CONFIG_HOME/wl-menu`, or `~/.config/wl-menu`.
fn config_dir() -> PathBuf {
    xdg_config_home().join("wl-menu")
}

fn default_config_path() -> PathBuf {
    config_dir().join("wl-menu.toml")
}

fn default_menu_path() -> PathBuf {
    config_dir().join("menu.jsonc")
}

/// Create the default config file and parent directory when they are missing.
fn ensure_config_file(path: &Path) {
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent() {
        if let Err(err) = fs::create_dir_all(dir) {
            eprintln!("{LOG_PREFIX}: cannot create {}: {err}", dir.display());
            return;
        }
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            if let Err(err) = file.write_all(DEFAULT_CONFIG.as_bytes()) {
                eprintln!("{LOG_PREFIX}: cannot write {}: {err}", path.display());
                return;
            }
            eprintln!("{LOG_PREFIX}: wrote default config {}", path.display());
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            eprintln!("{LOG_PREFIX}: cannot create {}: {err}", path.display());
        }
    }
}

fn xdg_config_home() -> PathBuf {
    env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

/// Read and parse a TOML config file. Missing files are skipped.
fn read_file_config(path: &Path) -> Option<FileConfig> {
    let text = fs::read_to_string(path).ok()?;
    match toml::from_str::<FileConfig>(&text) {
        Ok(file) => Some(file),
        Err(err) => {
            eprintln!("{LOG_PREFIX}: config {}: {err}", path.display());
            None
        }
    }
}

/// Expand `~` and `$VAR` / `${VAR}`, then resolve relative paths against `base`.
pub(crate) fn expand_path(raw: &str, base: &Path) -> PathBuf {
    let expanded = expand_vars(raw.trim());
    let path = if expanded == "~" {
        env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("~"))
    } else if let Some(rest) = expanded.strip_prefix("~/") {
        match env::var("HOME") {
            Ok(home) => PathBuf::from(home).join(rest),
            Err(_) => PathBuf::from(&expanded),
        }
    } else {
        PathBuf::from(&expanded)
    };
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn expand_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '}' {
                    closed = true;
                    break;
                }
                name.push(ch);
            }
            if closed {
                match env::var(&name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push_str("${");
                        out.push_str(&name);
                        out.push('}');
                    }
                }
            } else {
                out.push_str("${");
                out.push_str(&name);
            }
            continue;
        }
        let mut name = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                name.push(ch);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            out.push('$');
            continue;
        }
        match env::var(&name) {
            Ok(v) => out.push_str(&v),
            Err(_) => {
                out.push('$');
                out.push_str(&name);
            }
        }
    }
    out
}

fn parse_color_field(value: Option<String>, fallback: Color) -> Color {
    let Some(raw) = value else {
        return fallback;
    };
    parse_rgba(&raw).or_else(|| parse_hex(&raw)).unwrap_or(fallback)
}

pub fn parse_rgba(raw: &str) -> Option<Color> {
    let raw = raw.trim();
    if let Some(hex) = raw.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    let inner = raw.strip_prefix("rgba(")?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return None;
    }
    let r: u8 = parts[0].parse().ok()?;
    let g: u8 = parts[1].parse().ok()?;
    let b: u8 = parts[2].parse().ok()?;
    let a: f32 = parts[3].parse().ok()?;
    let a = if a <= 1.0 {
        (a * 255.0).round().clamp(0.0, 255.0) as u8
    } else {
        a.round().clamp(0.0, 255.0) as u8
    };
    Some(Color::from_rgba8(r, g, b, a))
}

fn parse_hex(raw: &str) -> Option<Color> {
    parse_hex_color(raw.trim_start_matches('#'))
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::from_rgba8(r, g, b, 255))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Color::from_rgba8(r, g, b, a))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_then_local_wins() {
        let dir = env::temp_dir().join(format!("wl-menu-cfg-{}-include", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("base.toml"), "[layout]\nwidth = 100\nmax_rows = 4\n").unwrap();
        fs::write(
            dir.join("wl-menu.toml"),
            "include = [\"base.toml\"]\n[layout]\nwidth = 200\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&dir.join("wl-menu.toml")));
        assert_eq!(cfg.layout.width, 200.0);
        assert_eq!(cfg.layout.max_rows, 4);
        assert_eq!(cfg.menu_path, dir.join("menu.jsonc"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_border_radius_overrides_layout() {
        let dir = env::temp_dir().join(format!("wl-menu-cfg-{}-radius", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("wl-menu.toml"),
            "[layout]\nborder_radius = 10\n[theme]\nborder_radius = 4\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&dir.join("wl-menu.toml")));
        assert_eq!(cfg.layout.border_radius, 10.0);
        assert_eq!(cfg.theme.border_radius, Some(4.0));
        assert_eq!(cfg.border_radius(), 4.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_env_and_relative_menu() {
        env::set_var("WL_MENU_TEST_ROOT", "/tmp/wl-menu-test-root");
        let dir = env::temp_dir().join(format!("wl-menu-cfg-{}-menu", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("wl-menu.toml"),
            "menu = \"$WL_MENU_TEST_ROOT/custom.jsonc\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&dir.join("wl-menu.toml")));
        assert_eq!(
            cfg.menu_path,
            PathBuf::from("/tmp/wl-menu-test-root/custom.jsonc")
        );
        env::remove_var("WL_MENU_TEST_ROOT");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seeds_missing_config_once() {
        let dir = env::temp_dir().join(format!("wl-menu-cfg-{}-seed", std::process::id()));
        let path = dir.join("wl-menu.toml");
        let _ = fs::remove_dir_all(&dir);
        ensure_config_file(&path);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[layout]"));
        let cfg = Config::load(Some(&path));
        assert_eq!(cfg.layout.width, 420.0);
        fs::write(&path, "mutated\n").unwrap();
        ensure_config_file(&path);
        assert_eq!(fs::read_to_string(&path).unwrap(), "mutated\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
