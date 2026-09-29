//! File explorer sidebar.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use egui::{Align2, Color32, FontId, Sense, Ui, pos2, vec2};

use crate::theme::Theme;

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

/// A visible row: (depth, directory listing, index in it).
type Row = (usize, Rc<[Entry]>, usize);

pub enum TreeAction {
    Open(PathBuf),
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
}

pub struct FileTree {
    pub root: PathBuf,
    expanded: HashSet<PathBuf>,
    /// Directory listings, shared (not cloned) by the visible-rows list.
    cache: HashMap<PathBuf, Rc<[Entry]>>,
    /// Flattened visible rows: (depth, listing, index in listing). Rebuilt only when needed.
    rows: Option<Vec<Row>>,
    pub selected: Option<PathBuf>,
    scroll_to_selected: bool,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        Self { root, expanded: HashSet::new(), cache: HashMap::new(), rows: None, selected: None, scroll_to_selected: false }
    }

    pub fn invalidate(&mut self) {
        self.cache.clear();
        self.rows = None;
    }

    /// Expand all ancestors of `path` and select it.
    pub fn reveal(&mut self, path: &Path) {
        let mut p = path.parent();
        while let Some(dir) = p {
            if !dir.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
            p = dir.parent();
        }
        self.selected = Some(path.to_path_buf());
        self.scroll_to_selected = true;
        self.rows = None;
    }

    fn children(&mut self, dir: &Path) -> Rc<[Entry]> {
        if let Some(c) = self.cache.get(dir) {
            return c.clone();
        }
        let mut entries: Vec<Entry> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name() != ".git")
                    .map(|e| {
                        let path = e.path();
                        // The directory entry already knows its type; only symlinks need a stat.
                        let is_dir = match e.file_type() {
                            Ok(t) if t.is_symlink() => path.is_dir(),
                            Ok(t) => t.is_dir(),
                            Err(_) => path.is_dir(),
                        };
                        Entry { name: e.file_name().to_string_lossy().to_string(), path, is_dir }
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
        let entries: Rc<[Entry]> = entries.into();
        self.cache.insert(dir.to_path_buf(), entries.clone());
        entries
    }

    fn visible_rows(&mut self) -> Vec<Row> {
        if let Some(r) = &self.rows {
            return r.clone();
        }
        let mut rows = Vec::new();
        let root = self.children(&self.root.clone());
        let mut stack: Vec<(usize, Rc<[Entry]>, usize)> = (0..root.len()).rev().map(|i| (0, root.clone(), i)).collect();
        while let Some((depth, list, i)) = stack.pop() {
            let e = &list[i];
            if e.is_dir && self.expanded.contains(&e.path) {
                let kids = self.children(&e.path);
                for k in (0..kids.len()).rev() {
                    stack.push((depth + 1, kids.clone(), k));
                }
            }
            rows.push((depth, list, i));
        }
        self.rows = Some(rows.clone());
        rows
    }

    /// Visible rows as (depth, entry), for tests.
    #[cfg(test)]
    fn flat_rows(&mut self) -> Vec<(usize, Entry)> {
        self.visible_rows().into_iter().map(|(d, l, i)| (d, l[i].clone())).collect()
    }

    pub fn show(&mut self, ui: &mut Ui, theme: &Theme, colors: &HashMap<PathBuf, Color32>) -> Option<TreeAction> {
        let mut action = None;
        let row_h = 22.0;
        let font = FontId::proportional(13.5);
        let rows = self.visible_rows();
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_to_selected)
            && let Some(i) = rows.iter().position(|(_, l, k)| self.selected.as_ref() == Some(&l[*k].path))
        {
            // Rows outside the view aren't laid out, so scroll by position instead of scroll_to_me.
            area = area.vertical_scroll_offset((i as f32 * row_h - ui.available_height() / 2.0).max(0.0));
        }
        let total = rows.len();
        area.show_rows(ui, row_h, total, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let last_row = range.end == total;
            for (depth, list, i) in &rows[range] {
                let (depth, e) = (*depth, &list[*i]);
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                let selected = self.selected.as_ref() == Some(&e.path);
                if selected {
                    ui.painter().rect_filled(rect, 0.0, theme.selection);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 0.0, theme.bg_highlight);
                }
                let x = rect.left() + 8.0 + depth as f32 * 14.0;
                let color = colors.get(&e.path).copied().unwrap_or(if e.is_dir { theme.fg } else { theme.fg_dim });
                if e.is_dir {
                    let c = pos2(x + 4.0, rect.center().y);
                    let pts = if self.expanded.contains(&e.path) {
                        vec![c + vec2(-4.0, -2.0), c + vec2(4.0, -2.0), c + vec2(0.0, 3.0)]
                    } else {
                        vec![c + vec2(-2.0, -4.0), c + vec2(3.0, 0.0), c + vec2(-2.0, 4.0)]
                    };
                    ui.painter().add(egui::Shape::convex_polygon(pts, theme.comment, egui::Stroke::NONE));
                }
                ui.painter().text(pos2(x + 14.0, rect.center().y), Align2::LEFT_CENTER, &e.name, font.clone(), color);

                if resp.clicked() {
                    self.selected = Some(e.path.clone());
                    if e.is_dir {
                        if !self.expanded.remove(&e.path) {
                            self.expanded.insert(e.path.clone());
                        }
                        self.rows = None;
                    } else {
                        action = Some(TreeAction::Open(e.path.clone()));
                    }
                }
                resp.context_menu(|ui| {
                    let dir = if e.is_dir { e.path.clone() } else { e.path.parent().unwrap_or(&self.root).to_path_buf() };
                    if ui.button("New File…").clicked() {
                        action = Some(TreeAction::NewFile(dir.clone()));
                    }
                    if ui.button("New Folder…").clicked() {
                        action = Some(TreeAction::NewFolder(dir));
                    }
                    ui.separator();
                    if ui.button("Rename…").clicked() {
                        action = Some(TreeAction::Rename(e.path.clone()));
                    }
                    if ui.button("Move to Trash").clicked() {
                        action = Some(TreeAction::Delete(e.path.clone()));
                    }
                    ui.separator();
                    if ui.button("Copy Path").clicked() {
                        ui.ctx().copy_text(e.path.to_string_lossy().to_string());
                    }
                });
            }
            if !last_row {
                return;
            }
            // Empty space below the tree: root-level actions.
            let rest = ui.available_rect_before_wrap();
            let resp = ui.allocate_rect(rest.with_max_y(rest.max.y.max(rest.min.y + 40.0)), Sense::click());
            resp.context_menu(|ui| {
                if ui.button("New File…").clicked() {
                    action = Some(TreeAction::NewFile(self.root.clone()));
                }
                if ui.button("New Folder…").clicked() {
                    action = Some(TreeAction::NewFolder(self.root.clone()));
                }
            });
        });
        action
    }
}

#[cfg(test)]
#[path = "validate/tree_tests.rs"]
mod validate_tests;
