//! Command palette / quick open popup.

use egui::{Align2, FontId, Id, Key, Modifiers, Order, Sense, TextEdit, pos2, vec2};

use crate::text::{FuzzyQuery, FuzzyTarget};
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Files,
    Commands,
    Themes { original: String },
    GotoLine,
    Branches,
    /// Pick a file to `@`-mention in the Claude prompt.
    Mention,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub label: String,
    pub detail: String,
}

impl Item {
    pub fn new(id: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), detail: detail.into() }
    }
}

pub enum Outcome {
    None,
    Accept(Mode, Option<Item>, String),
    Preview(String),
    Cancel(Mode),
}

pub struct Palette {
    pub open: bool,
    pub mode: Mode,
    query: String,
    items: Vec<Item>,
    /// Prepared once per item, so typing doesn't redo per-candidate work (50k files).
    targets: Vec<FuzzyTarget>,
    /// Every item matching `matched_query` (not truncated), to narrow down as the query grows.
    matched: Vec<usize>,
    matched_query: Option<String>,
    filtered: Vec<usize>,
    selected: usize,
    focus: bool,
    scroll: bool,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            open: false,
            mode: Mode::Commands,
            query: String::new(),
            items: vec![],
            targets: vec![],
            matched: vec![],
            matched_query: None,
            filtered: vec![],
            selected: 0,
            focus: false,
            scroll: false,
        }
    }
}

impl Palette {
    pub fn open(&mut self, mode: Mode, items: Vec<Item>, selected: usize) {
        self.open = true;
        self.mode = mode;
        self.items = items;
        self.set_targets();
        self.query.clear();
        self.focus = true;
        self.refilter();
        self.selected = selected.min(self.filtered.len().saturating_sub(1));
        self.scroll = true;
    }

    /// Replace the items (e.g. when a background file listing arrives), keeping the query.
    pub fn set_items(&mut self, items: Vec<Item>) {
        let selected = self.selected;
        self.items = items;
        self.set_targets();
        self.refilter();
        self.selected = selected.min(self.filtered.len().saturating_sub(1));
    }

    fn set_targets(&mut self) {
        self.targets = self.items.iter().map(|it| FuzzyTarget::new(&it.label)).collect();
        self.matched_query = None;
    }

    fn refilter(&mut self) {
        let q = FuzzyQuery::new(&self.query);
        // A longer query can only match a subset of what the shorter one matched.
        let candidates: Vec<usize> = match &self.matched_query {
            Some(prev) if self.query.starts_with(prev.as_str()) => std::mem::take(&mut self.matched),
            _ => (0..self.items.len()).collect(),
        };
        let mut scored: Vec<(i64, usize)> = candidates.into_iter().filter_map(|i| self.targets[i].score(&q).map(|s| (s, i))).collect();
        self.matched = scored.iter().map(|(_, i)| *i).collect();
        self.matched_query = Some(self.query.clone());
        if !self.query.is_empty() {
            scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        }
        self.filtered = scored.into_iter().map(|(_, i)| i).take(500).collect();
        self.selected = 0;
    }

    fn hint(&self) -> &'static str {
        match self.mode {
            Mode::Files => "Search files by name",
            Mode::Commands => "Type a command",
            Mode::Themes { .. } => "Select color theme",
            Mode::GotoLine => "Line number",
            Mode::Branches => "Checkout branch",
            Mode::Mention => "Mention a file",
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, theme: &Theme) -> Outcome {
        if !self.open {
            return Outcome::None;
        }
        let mut outcome = Outcome::None;
        let prev_selected = self.selected;
        let n = self.filtered.len();
        let (down, up, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if n > 0 {
            if down {
                self.selected = (self.selected + 1) % n;
            }
            if up {
                self.selected = (self.selected + n - 1) % n;
            }
        }
        let mut accept = enter;
        let screen = ctx.content_rect();
        let width = 620f32.min(screen.width() - 32.0);
        let area = egui::Area::new(Id::new("rim-palette"))
            .order(Order::Foreground)
            .fixed_pos(pos2(screen.center().x - width / 2.0, screen.top() + 48.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).fill(theme.bg_float).inner_margin(8.0).show(ui, |ui| {
                    ui.set_width(width);
                    let hint = self.hint();
                    let te = TextEdit::singleline(&mut self.query)
                        .hint_text(hint)
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(15.0));
                    let resp = ui.add(te);
                    if self.focus {
                        resp.request_focus();
                        self.focus = false;
                    }
                    if resp.changed() {
                        self.refilter();
                    }
                    if self.mode == Mode::GotoLine {
                        return;
                    }
                    ui.add_space(4.0);
                    let row_h = 24.0;
                    egui::ScrollArea::vertical().max_height(row_h * 14.0).auto_shrink([false, true]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (i, &idx) in self.filtered.iter().enumerate() {
                            let item = &self.items[idx];
                            let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                            if i == self.selected {
                                ui.painter().rect_filled(rect, 4.0, theme.selection);
                                if self.scroll || self.selected != prev_selected {
                                    r.scroll_to_me(None);
                                }
                            } else if r.hovered() {
                                ui.painter().rect_filled(rect, 4.0, theme.bg_highlight);
                            }
                            let y = rect.center().y;
                            ui.painter().text(pos2(rect.left() + 8.0, y), Align2::LEFT_CENTER, &item.label, FontId::proportional(14.0), theme.fg);
                            if !item.detail.is_empty() {
                                ui.painter().text(pos2(rect.right() - 8.0, y), Align2::RIGHT_CENTER, &item.detail, FontId::proportional(12.5), theme.comment);
                            }
                            if r.clicked() {
                                self.selected = i;
                                accept = true;
                            }
                        }
                        if self.filtered.is_empty() && self.mode != Mode::GotoLine {
                            ui.label(egui::RichText::new("No matches").color(theme.comment));
                        }
                    });
                });
            });
        self.scroll = false;

        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx.pointer_interact_pos().is_some_and(|p| !area.response.rect.contains(p));
        if esc || clicked_outside {
            self.open = false;
            return Outcome::Cancel(self.mode.clone());
        }
        if accept {
            self.open = false;
            let item = self.filtered.get(self.selected).map(|&i| self.items[i].clone());
            return Outcome::Accept(self.mode.clone(), item, self.query.clone());
        }
        if let Mode::Themes { .. } = self.mode
            && (self.selected != prev_selected || down || up)
                && let Some(&i) = self.filtered.get(self.selected) {
                    outcome = Outcome::Preview(self.items[i].id.clone());
                }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(n: usize) -> Vec<Item> {
        (0..n).map(|i| format!("src/module_{}/file_{i}.rs", i % 97)).map(|f| Item::new(f.clone(), f, "")).collect()
    }

    #[test]
    fn narrowing_the_query_matches_filtering_from_scratch() {
        let mut inc = Palette::default();
        inc.open(Mode::Files, files(5_000), 0);
        let mut typed = String::new();
        for ch in "mod3fil12".chars() {
            typed.push(ch);
            inc.query = typed.clone();
            inc.refilter();
            let mut fresh = Palette::default();
            fresh.open(Mode::Files, files(5_000), 0);
            fresh.query = typed.clone();
            fresh.refilter();
            assert_eq!(inc.filtered, fresh.filtered, "query {typed:?}");
        }
        // Deleting a character goes back to a full search.
        inc.query.pop();
        inc.refilter();
        let mut fresh = Palette::default();
        fresh.open(Mode::Files, files(5_000), 0);
        fresh.query = inc.query.clone();
        fresh.refilter();
        assert_eq!(inc.filtered, fresh.filtered);
    }

    #[test]
    fn typing_in_a_50k_file_list_is_fast() {
        let mut p = Palette::default();
        p.open(Mode::Files, files(50_000), 0);
        let start = std::time::Instant::now();
        for q in ["m", "mo", "mod", "modu", "modul"] {
            p.query = q.into();
            p.refilter();
        }
        // Generous bound so a debug build on a busy machine passes; release is ~10x faster.
        assert!(start.elapsed().as_millis() < 1500, "{:?}", start.elapsed());
    }
}

