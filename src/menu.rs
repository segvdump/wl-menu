//! JSONC menu tree loader and navigation helpers.

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

const LOG_PREFIX: &str = "wl-menu";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Menu,
    Action,
    Link,
    App,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub id: String,
    pub parent: String,
    pub kind: ItemKind,
    pub icon: String,
    pub label: String,
    pub target: String,
    pub action: String,
    pub confirm: String,
    pub confirm_text: String,
    pub provider: String,
    pub when_expr: String,
    pub aliases: Vec<String>,
    pub order: usize,
}

#[derive(Clone, Debug)]
pub struct MenuTree {
    pub items: HashMap<String, MenuItem>,
    pub order: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct GuardResults {
    pub when_ok: HashMap<String, bool>,
}

#[derive(Clone, Debug)]
pub struct DisplayRow {
    pub id: String,
    pub icon: String,
    pub label: String,
    pub detail: String,
}

impl MenuTree {
    /// Load a single JSONC menu tree file.
    pub fn load(path: &Path) -> Self {
        let exists = path.is_file();
        let base_dir = path.parent().unwrap_or(Path::new("."));
        let raw: Vec<MenuItem> = read_jsonc_file(path)
            .into_iter()
            .map(|mut item| {
                item.icon = crate::icons::normalize_menu_icon(&item.icon, base_dir);
                item
            })
            .collect();
        let raw_count = raw.len();
        let tree = Self::merge(vec![raw]);
        let root_children: Vec<&str> = tree
            .children("root", &GuardResults::default())
            .into_iter()
            .map(|item| item.label.as_str())
            .collect();
        crate::debug::log(format!(
            "menu: path={} exists={} raw_items={} tree_items={} root_children={} preview={:?}",
            path.display(),
            exists,
            raw_count,
            tree.items.len(),
            root_children.len(),
            root_children.iter().take(12).collect::<Vec<_>>(),
        ));
        tree
    }

    pub(crate) fn merge(sources: Vec<Vec<MenuItem>>) -> Self {
        let mut items = HashMap::new();
        let mut order = Vec::new();
        for source in sources {
            for mut item in source {
                if !items.contains_key(&item.id) {
                    order.push(item.id.clone());
                }
                item.order = order.iter().position(|id| id == &item.id).unwrap_or(order.len());
                items.insert(item.id.clone(), item);
            }
        }
        if !items.contains_key("root") {
            order.insert(0, "root".into());
            items.insert(
                "root".into(),
                MenuItem {
                    id: "root".into(),
                    parent: String::new(),
                    kind: ItemKind::Menu,
                    icon: String::new(),
                    label: "Go".into(),
                    target: String::new(),
                    action: String::new(),
                    confirm: String::new(),
                    confirm_text: String::new(),
                    provider: String::new(),
                    when_expr: String::new(),
                    aliases: Vec::new(),
                    order: 0,
                },
            );
        }
        Self { items, order }
    }

    pub fn item(&self, id: &str) -> Option<&MenuItem> {
        self.items.get(id)
    }

    /// Resolve route string (id or alias) to a menu item id.
    pub fn resolve_route(&self, input: &str) -> String {
        let raw = input.trim().to_lowercase().replace('_', "-");
        if raw.is_empty() || raw == "go" || raw == "menu" || raw == "root" {
            return "root".into();
        }
        if self.items.contains_key(&raw) {
            return raw;
        }
        for id in &self.order {
            let Some(item) = self.items.get(id) else {
                continue;
            };
            if item.kind == ItemKind::App {
                continue;
            }
            for alias in &item.aliases {
                if alias.to_lowercase().replace('_', "-") == raw {
                    return id.clone();
                }
            }
        }
        raw
    }

    pub fn merge_apps(&mut self, apps: Vec<MenuItem>) {
        self.items.retain(|_, item| item.kind != ItemKind::App);
        self.order.retain(|id| self.items.get(id).is_some_and(|i| i.kind != ItemKind::App));
        for app in apps {
            if self.items.contains_key(&app.id) {
                continue;
            }
            let mut app = app;
            app.order = self.order.len();
            self.order.push(app.id.clone());
            self.items.insert(app.id.clone(), app);
        }
    }

    pub fn evaluate_guards(&self) -> GuardResults {
        let script = guard_script(self);
        let Some(script) = script else {
            return GuardResults::default();
        };
        let output = Command::new("bash").arg("-lc").arg(script).output();
        let Ok(output) = output else {
            return GuardResults::default();
        };
        if !output.status.success() {
            return GuardResults::default();
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut when_ok = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((id, val)) = line.rsplit_once(':') else {
                continue;
            };
            let Some((id, tag)) = id.rsplit_once(':') else {
                continue;
            };
            if tag == "w" {
                when_ok.insert(id.to_string(), val == "1");
            }
        }
        GuardResults { when_ok }
    }

    pub fn is_visible(&self, id: &str, guards: &GuardResults) -> bool {
        let Some(item) = self.items.get(id) else {
            return false;
        };
        if item.when_expr.is_empty() {
            return true;
        }
        !matches!(guards.when_ok.get(id), Some(false))
    }

    pub fn children(&self, parent: &str, guards: &GuardResults) -> Vec<&MenuItem> {
        let mut rows: Vec<&MenuItem> = self
            .order
            .iter()
            .filter_map(|id| self.items.get(id))
            .filter(|item| item.parent == parent && self.is_visible(&item.id, guards))
            .collect();
        rows.sort_by_key(|item| item.order);
        rows
    }

    pub fn display_rows(
        &self,
        active: &str,
        filter: &str,
        guards: &GuardResults,
    ) -> Vec<DisplayRow> {
        let query = filter.trim().to_lowercase();
        if query.is_empty() {
            return self
                .children(active, guards)
                .into_iter()
                .map(|item| self.row_from_item(item))
                .collect();
        }

        let mut current = Vec::new();
        let mut drill = Vec::new();
        for id in &self.order {
            let Some(item) = self.items.get(id) else {
                continue;
            };
            if id == "root" || !self.is_visible(id, guards) {
                continue;
            }
            if !self.matches_query(item, &query) {
                continue;
            }
            let detail = self.parent_path(item.parent.clone());
            let row = DisplayRow {
                id: item.id.clone(),
                icon: item.icon.clone(),
                label: item.label.clone(),
                detail,
            };
            if item.parent == active {
                current.push(row);
            } else {
                drill.push(row);
            }
        }
        current.extend(drill);
        current
    }

    fn row_from_item(&self, item: &MenuItem) -> DisplayRow {
        DisplayRow {
            id: item.id.clone(),
            icon: item.icon.clone(),
            label: item.label.clone(),
            detail: String::new(),
        }
    }

    fn matches_query(&self, item: &MenuItem, query: &str) -> bool {
        let label = item.label.to_lowercase();
        if label.contains(query) {
            return true;
        }
        if item.id.to_lowercase().contains(query) {
            return true;
        }
        for alias in &item.aliases {
            if alias.to_lowercase().contains(query) {
                return true;
            }
        }
        false
    }

    fn parent_path(&self, mut parent: String) -> String {
        let mut parts = Vec::new();
        while !parent.is_empty() && parent != "root" {
            if let Some(item) = self.items.get(&parent) {
                parts.push(item.label.clone());
                parent = item.parent.clone();
            } else {
                break;
            }
        }
        parts.reverse();
        parts.join(" › ")
    }
}

fn read_jsonc_file(path: &Path) -> Vec<MenuItem> {
    match fs::read_to_string(path) {
        Ok(text) => {
            crate::debug::log(format!(
                "menu: read {} bytes from {}",
                text.len(),
                path.display()
            ));
            parse_menu_jsonc(&text)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("{LOG_PREFIX}: menu not found: {}", path.display());
            Vec::new()
        }
        Err(err) => {
            eprintln!("{LOG_PREFIX}: cannot read menu {}: {err}", path.display());
            Vec::new()
        }
    }
}

pub(crate) fn parse_menu_jsonc(raw: &str) -> Vec<MenuItem> {
    let stripped = strip_jsonc(raw);
    if stripped.trim().is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(&stripped) else {
        eprintln!("{LOG_PREFIX}: invalid menu JSON");
        return Vec::new();
    };
    let obj = match value {
        Value::Object(map) => map,
        _ => return Vec::new(),
    };
    let source = if let Some(Value::Object(items)) = obj.get("items") {
        items.clone()
    } else {
        obj
    };
    let mut out = Vec::new();
    for (id, entry) in source {
        let Some(raw_item) = RawMenuItem::from_value(entry) else {
            continue;
        };
        out.push(normalize_item(id, raw_item));
    }
    out
}

#[derive(Default, Deserialize)]
struct RawMenuItem {
    icon: Option<String>,
    label: Option<String>,
    action: Option<String>,
    target: Option<String>,
    confirm: Option<String>,
    #[serde(rename = "confirmText")]
    confirm_text: Option<String>,
    provider: Option<String>,
    when: Option<String>,
    aliases: Option<Aliases>,
    parent: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Aliases {
    One(String),
    Many(Vec<String>),
}

impl RawMenuItem {
    fn from_value(value: Value) -> Option<Self> {
        serde_json::from_value(value).ok()
    }
}

fn normalize_item(id: String, raw: RawMenuItem) -> MenuItem {
    let parent = raw.parent.unwrap_or_else(|| {
        if id.contains('.') {
            id.rsplit_once('.').map(|(p, _)| p.to_string()).unwrap_or_default()
        } else {
            "root".into()
        }
    });
    let kind = if raw.action.is_some() {
        ItemKind::Action
    } else if raw.target.is_some() {
        ItemKind::Link
    } else {
        ItemKind::Menu
    };
    let aliases = match raw.aliases {
        Some(Aliases::One(v)) => vec![v],
        Some(Aliases::Many(v)) => v,
        None => Vec::new(),
    };
    MenuItem {
        id: id.clone(),
        parent,
        kind,
        icon: raw.icon.unwrap_or_default(),
        label: raw.label.unwrap_or(id),
        target: raw.target.unwrap_or_default(),
        action: raw.action.unwrap_or_default(),
        confirm: raw.confirm.unwrap_or_default(),
        confirm_text: raw.confirm_text.unwrap_or_default(),
        provider: raw.provider.unwrap_or_default(),
        when_expr: raw.when.unwrap_or_default(),
        aliases,
        order: 0,
    }
}

fn strip_jsonc(raw: &str) -> String {
    static RE_TRAIL: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static RE_LINE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re_trail = RE_TRAIL.get_or_init(|| Regex::new(r",(\s*[}\]])").expect("regex"));
    let re_line = RE_LINE.get_or_init(|| Regex::new(r"(?m)^\s*//[^\n]*").expect("regex"));
    let no_comments = re_line.replace_all(raw, "");
    re_trail.replace_all(&no_comments, "$1").into_owned()
}

fn guard_script(tree: &MenuTree) -> Option<String> {
    let mut body = String::new();
    for (id, item) in &tree.items {
        if !item.when_expr.is_empty() {
            // `when` is a bash expression: visible when it exits 0.
            body.push_str(&format!(
                "if {when}; then printf '%s\\n' '{id}:w:1'; else printf '%s\\n' '{id}:w:0'; fi\n",
                when = item.when_expr,
                id = id.replace('\'', "'\\''"),
            ));
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(format!("set +e\n{body}"))
}

pub fn run_action(action: &str) {
    if action.is_empty() {
        crate::debug::log("run_action: skipped (empty action)");
        return;
    }
    crate::debug::log(format!("run_action: spawn bash -lc {action:?}"));
    let _ = Command::new("bash").arg("-lc").arg(action).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_route_alias() {
        let tree = MenuTree::merge(vec![parse_menu_jsonc(
            r#"{"system":{"label":"System","aliases":["power-menu"]}}"#,
        )]);
        assert_eq!(tree.resolve_route("power-menu"), "system");
    }

    #[test]
    fn preserves_jsonc_key_order_for_root() {
        let tree = MenuTree::merge(vec![parse_menu_jsonc(
            r#"{
                "apps": { "label": "Apps" },
                "trigger": { "label": "Trigger" },
                "style": { "label": "Style" },
                "system": { "label": "System" }
            }"#,
        )]);
        let guards = GuardResults::default();
        let labels: Vec<_> = tree
            .children("root", &guards)
            .into_iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels, ["Apps", "Trigger", "Style", "System"]);
    }

    #[test]
    fn missing_menu_file_still_has_root() {
        let path = std::env::temp_dir().join(format!(
            "wl-menu-missing-{}.jsonc",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let tree = MenuTree::load(&path);
        assert!(tree.item("root").is_some());
        assert!(tree.children("root", &GuardResults::default()).is_empty());
    }
}
