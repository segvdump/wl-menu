//! Menu navigation state: routes, filter, selection, confirm dialog.

use crate::apps;
use crate::config::Config;
use crate::debug;
use crate::icons::IconCache;
use crate::menu::{run_action, DisplayRow, GuardResults, ItemKind, MenuTree};
use std::collections::HashSet;

/// Pending destructive action confirmation.
#[derive(Clone, Debug)]
pub struct ActionConfirm {
    pub item_id: String,
    pub selected: usize,
}

/// Live menu session owned by the Wayland daemon.
pub struct MenuSession {
    pub config: Config,
    pub tree: MenuTree,
    pub visible: bool,
    pub active: String,
    pub nav_stack: Vec<String>,
    pub filter: String,
    pub selected: usize,
    pub guards: GuardResults,
    pub rows: Vec<DisplayRow>,
    pub confirm: Option<ActionConfirm>,
    pub icons: IconCache,
    providers_loaded: HashSet<String>,
}

impl MenuSession {
    pub fn new(config: Config) -> Self {
        let tree = MenuTree::load(&config.menu_path);
        let guards = tree.evaluate_guards();
        let mut session = Self {
            config,
            tree,
            visible: false,
            active: "root".into(),
            nav_stack: Vec::new(),
            filter: String::new(),
            selected: 0,
            guards,
            rows: Vec::new(),
            confirm: None,
            icons: IconCache::new(),
            providers_loaded: HashSet::new(),
        };
        session.rebuild_rows();
        session
    }

    pub fn reload(&mut self) {
        self.tree = MenuTree::load(&self.config.menu_path);
        self.guards = self.tree.evaluate_guards();
        self.providers_loaded.clear();
        self.icons.clear();
        self.rebuild_rows();
    }

    pub fn open_route(&mut self, route: &str) {
        let id = self.tree.resolve_route(route);
        let Some(entry) = self.tree.item(&id).cloned() else {
            self.show_menu("root");
            return;
        };

        if entry.kind == ItemKind::Action && !entry.action.is_empty() {
            if !entry.confirm.is_empty() {
                let parent = if entry.parent.is_empty() {
                    "root".into()
                } else {
                    entry.parent.clone()
                };
                self.show_menu(&parent);
                self.request_confirm(&id);
                return;
            }
            self.close();
            run_action(&entry.action);
            return;
        }

        let target = if entry.kind == ItemKind::Link && !entry.target.is_empty() {
            entry.target.clone()
        } else {
            id
        };
        self.show_menu(&target);
    }

    pub fn toggle(&mut self, route: &str) {
        if self.visible {
            self.close();
        } else {
            self.open_route(route);
        }
    }

    pub fn show_menu(&mut self, id: &str) {
        let active = if self.tree.item(id).is_some() {
            id.to_string()
        } else {
            "root".into()
        };
        debug::log(format!(
            "show_menu: route={id:?} -> active={active:?} (was visible={})",
            self.visible
        ));
        self.visible = true;
        self.active = active.clone();
        self.nav_stack.clear();
        self.filter.clear();
        self.selected = 0;
        self.confirm = None;
        self.ensure_provider(&active);
        self.rebuild_rows();
        self.log_rows_snapshot("after show_menu");
    }

    pub fn close(&mut self) {
        debug::log(format!(
            "close: active={:?} rows={}",
            self.active,
            self.rows.len()
        ));
        self.visible = false;
        self.confirm = None;
        self.filter.clear();
    }

    fn request_confirm(&mut self, item_id: &str) {
        self.confirm = Some(ActionConfirm {
            item_id: item_id.to_string(),
            selected: self.config.confirm.default.selected_index(),
        });
    }

    fn ensure_provider(&mut self, menu_id: &str) {
        let provider = match self.tree.item(menu_id) {
            Some(entry) if !entry.provider.is_empty() => entry.provider.clone(),
            _ => return,
        };
        if self.providers_loaded.contains(menu_id) {
            debug::log(format!("ensure_provider: {menu_id:?} provider={provider:?} already loaded"));
            return;
        }
        debug::log(format!("ensure_provider: loading {provider:?} for menu {menu_id:?}"));
        if provider == "apps" {
            let apps = apps::scan_apps();
            debug::log(format!("ensure_provider: merge_apps count={}", apps.len()));
            self.tree.merge_apps(apps);
        }
        self.providers_loaded.insert(menu_id.to_string());
    }

    pub fn rebuild_rows(&mut self) {
        self.rows = self.tree.display_rows(&self.active, &self.filter, &self.guards);
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn log_rows_snapshot(&self, tag: &str) {
        let preview: Vec<String> = self
            .rows
            .iter()
            .take(5)
            .enumerate()
            .map(|(i, row)| format!("[{i}] {} ({})", row.label, row.id))
            .collect();
        debug::log(format!(
            "{tag}: active={:?} selected={} rows={} preview=[{}]",
            self.active,
            self.selected,
            self.rows.len(),
            preview.join(", ")
        ));
    }

    pub fn row_debug(&self, index: usize) -> String {
        let Some(row) = self.rows.get(index) else {
            return format!("row[{index}]=<none>");
        };
        let kind = self
            .tree
            .item(&row.id)
            .map(|item| format!("{:?}", item.kind))
            .unwrap_or_else(|| "?".into());
        format!(
            "row[{index}] id={} label={:?} kind={kind}",
            row.id, row.label
        )
    }

    /// First visible row index so `selected` stays inside a `visible_rows` window.
    pub fn scroll_offset(&self, visible_rows: usize) -> usize {
        let visible = visible_rows.max(1);
        if self.rows.len() <= visible {
            return 0;
        }
        let max_off = self.rows.len() - visible;
        self.selected.saturating_sub(visible - 1).min(max_off)
    }

    pub fn header_title(&self) -> String {
        if self.confirm.is_some() {
            return self.confirm_lines().0;
        }
        if !self.filter.is_empty() {
            return self.filter.clone();
        }
        self.tree
            .item(&self.active)
            .map(|item| item.label.clone())
            .unwrap_or_else(|| "Go".into())
    }

    pub fn breadcrumb(&self) -> String {
        let mut parts = Vec::new();
        let mut current = self.active.as_str();
        while !current.is_empty() && current != "root" {
            if let Some(item) = self.tree.item(current) {
                parts.push(item.label.clone());
                current = item.parent.as_str();
            } else {
                break;
            }
        }
        parts.reverse();
        parts.join(" › ")
    }

    pub fn select_delta(&mut self, delta: i32) {
        if self.confirm.is_some() {
            let confirm = self.confirm.as_mut().unwrap();
            confirm.selected = if confirm.selected == 0 { 1 } else { 0 };
            return;
        }
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    /// True when Right arrow may drill into the selected row (Menu or Link only).
    pub fn selected_opens_submenu(&self) -> bool {
        if self.confirm.is_some() {
            return false;
        }
        let Some(row) = self.rows.get(self.selected) else {
            return false;
        };
        let Some(item) = self.tree.item(&row.id) else {
            return false;
        };
        matches!(item.kind, ItemKind::Menu | ItemKind::Link)
    }

    pub fn activate_selected(&mut self) {
        debug::log(format!(
            "activate_selected: entry active={:?} selected={} {}",
            self.active,
            self.selected,
            self.row_debug(self.selected)
        ));
        if let Some(confirm) = self.confirm.clone() {
            debug::log(format!(
                "activate_selected: confirm dialog item={:?} choice={}",
                confirm.item_id, confirm.selected
            ));
            if confirm.selected == 0 {
                if let Some(item) = self.tree.item(&confirm.item_id) {
                    let action = item.action.clone();
                    self.close();
                    run_action(&action);
                } else {
                    self.confirm = None;
                }
            } else {
                self.confirm = None;
            }
            return;
        }

        let Some(row) = self.rows.get(self.selected).cloned() else {
            debug::log("activate_selected: no row at selected index");
            return;
        };
        let Some(item) = self.tree.item(&row.id).cloned() else {
            debug::log(format!("activate_selected: tree missing id={}", row.id));
            return;
        };

        match item.kind {
            ItemKind::Menu => {
                debug::log(format!("activate_selected: enter menu {:?}", item.id));
                self.nav_stack.push(self.active.clone());
                self.active = item.id.clone();
                self.filter.clear();
                self.selected = 0;
                self.ensure_provider(&self.active.clone());
                self.rebuild_rows();
                self.log_rows_snapshot("after enter menu");
            }
            ItemKind::Link => {
                let target = if item.target.is_empty() {
                    item.id.clone()
                } else {
                    item.target.clone()
                };
                debug::log(format!(
                    "activate_selected: follow link {:?} -> {target:?}",
                    item.id
                ));
                self.nav_stack.push(self.active.clone());
                self.active = target;
                self.filter.clear();
                self.selected = 0;
                self.ensure_provider(&self.active.clone());
                self.rebuild_rows();
                self.log_rows_snapshot("after follow link");
            }
            ItemKind::App => {
                debug::log(format!(
                    "activate_selected: launch app id={} label={:?}",
                    item.id, item.label
                ));
                let action = item.action.clone();
                self.close();
                run_action(&action);
            }
            ItemKind::Action => {
                if !item.confirm.is_empty() {
                    debug::log(format!(
                        "activate_selected: confirm action id={} label={:?}",
                        item.id, item.label
                    ));
                    self.request_confirm(&item.id);
                    return;
                }
                debug::log(format!(
                    "activate_selected: run action id={} label={:?}",
                    item.id, item.label
                ));
                let action = item.action.clone();
                self.close();
                run_action(&action);
            }
        }
    }

    pub fn go_back(&mut self) -> bool {
        if self.confirm.is_some() {
            self.confirm = None;
            return true;
        }
        if !self.filter.is_empty() {
            self.filter.pop();
            self.selected = 0;
            self.rebuild_rows();
            return true;
        }
        if let Some(prev) = self.nav_stack.pop() {
            debug::log(format!(
                "go_back: nav_stack -> active={prev:?} (stack remaining={})",
                self.nav_stack.len()
            ));
            self.active = prev;
            self.selected = 0;
            self.rebuild_rows();
            return true;
        }
        if self.active != "root" {
            let parent = self
                .tree
                .item(&self.active)
                .map(|item| {
                    if item.parent.is_empty() {
                        "root".to_string()
                    } else {
                        item.parent.clone()
                    }
                })
                .unwrap_or_else(|| "root".to_string());
            debug::log(format!(
                "go_back: parent fallback {:?} -> {parent:?}",
                self.active
            ));
            self.active = parent;
            self.selected = 0;
            self.rebuild_rows();
            return true;
        }
        debug::log("go_back: already at root");
        false
    }

    pub fn push_char(&mut self, ch: char) {
        if self.confirm.is_some() || !ch.is_ascii() && !ch.is_alphanumeric() {
            return;
        }
        self.filter.push(ch);
        self.selected = 0;
        if self.filter.trim().len() == 1 {
            self.load_providers_for_search();
        }
        self.rebuild_rows();
    }

    pub fn backspace(&mut self) {
        if self.confirm.is_some() {
            self.confirm = None;
            return;
        }
        if self.filter.is_empty() {
            self.go_back();
            return;
        }
        self.filter.pop();
        self.selected = 0;
        self.rebuild_rows();
    }

    fn load_providers_for_search(&mut self) {
        let active = self.active.clone();
        for id in self.tree.order.clone() {
            let provider_id = match self.tree.item(&id) {
                Some(item) if !item.provider.is_empty() && !self.providers_loaded.contains(&item.id) => {
                    if active != "root" && item.id != active && !is_descendant(&self.tree, &item.id, &active) {
                        continue;
                    }
                    item.id.clone()
                }
                _ => continue,
            };
            self.ensure_provider(&provider_id);
        }
    }

    pub fn confirm_lines(&self) -> (String, String, String) {
        let Some(confirm) = &self.confirm else {
            return (String::new(), String::new(), String::new());
        };
        let Some(item) = self.tree.item(&confirm.item_id) else {
            return (String::new(), String::new(), String::new());
        };
        let message = if item.confirm.is_empty() {
            format!("Run {}?", item.label)
        } else {
            item.confirm.clone()
        };
        let yes = if item.confirm_text.is_empty() {
            "Confirm".into()
        } else {
            item.confirm_text.clone()
        };
        (message, yes, "Cancel".into())
    }
}

fn is_descendant(tree: &MenuTree, id: &str, ancestor: &str) -> bool {
    let mut current = tree.item(id).map(|item| item.parent.as_str()).unwrap_or("");
    while !current.is_empty() {
        if current == ancestor {
            return true;
        }
        current = tree
            .item(current)
            .map(|item| item.parent.as_str())
            .unwrap_or("");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::menu::{parse_menu_jsonc, GuardResults, MenuTree};
    use std::collections::HashSet;

    #[test]
    fn go_back_uses_parent_when_nav_stack_empty() {
        let tree = MenuTree::merge(vec![parse_menu_jsonc(
            r#"{
                "apps": { "label": "Apps", "provider": "apps" },
                "trigger": { "label": "Trigger" },
                "trigger.capture": { "label": "Capture" }
            }"#,
        )]);
        let guards = GuardResults::default();
        let mut session = MenuSession {
            config: Config::default(),
            tree,
            visible: true,
            active: "trigger.capture".into(),
            nav_stack: Vec::new(),
            filter: String::new(),
            selected: 0,
            guards,
            rows: Vec::new(),
            confirm: None,
            icons: IconCache::new(),
            providers_loaded: HashSet::new(),
        };
        session.rebuild_rows();
        assert!(session.go_back());
        assert_eq!(session.active, "trigger");
        assert!(session.go_back());
        assert_eq!(session.active, "root");
        assert!(!session.go_back());
        assert!(session.visible);
    }

    #[test]
    fn selected_opens_submenu_only_for_menu_and_link() {
        let tree = MenuTree::merge(vec![parse_menu_jsonc(
            r#"{
                "folder": { "label": "Folder" },
                "jump": { "label": "Jump", "target": "folder" },
                "run": { "label": "Run", "action": "true", "confirm": "Sure?" }
            }"#,
        )]);
        let mut session = MenuSession {
            config: Config::default(),
            tree,
            visible: true,
            active: "root".into(),
            nav_stack: Vec::new(),
            filter: String::new(),
            selected: 0,
            guards: GuardResults::default(),
            rows: Vec::new(),
            confirm: None,
            icons: IconCache::new(),
            providers_loaded: HashSet::new(),
        };
        session.rebuild_rows();

        let index_of = |session: &MenuSession, id: &str| {
            session
                .rows
                .iter()
                .position(|row| row.id == id)
                .expect(id)
        };

        session.selected = index_of(&session, "folder");
        assert!(session.selected_opens_submenu());

        session.selected = index_of(&session, "jump");
        assert!(session.selected_opens_submenu());

        session.selected = index_of(&session, "run");
        assert!(!session.selected_opens_submenu());

        session.request_confirm("run");
        assert!(!session.selected_opens_submenu());
    }
}
