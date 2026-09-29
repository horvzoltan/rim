//! Find in Files popup (Ctrl+Shift+F), modelled on IntelliJ's: results update as you type, and the
//! selected match is shown in a syntax-highlighted preview of its file. Enter opens it.

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat, TextWrapping};
use egui::{Order, ScrollArea};

use super::*;
use crate::highlight::HlCache;
use crate::search::{Match, Options};

/// Pause after typing (seconds) before a search starts.
const DEBOUNCE: f64 = 0.15;
const MAX_MATCHES: usize = 2000;
const ROW_H: f32 = 24.0;

#[derive(Default)]
pub(super) struct FindFiles {
    pub(super) open: bool,
    pub(super) query: String,
    pub(super) opts: Options,
    pub(super) results: Vec<FileResult>,
    truncated: bool,
    /// (file index, match index) for each row of the list.
    rows: Vec<(usize, usize)>,
    pub(super) selected: usize,
    focus: bool,
    running: bool,
    searched: bool,
    /// Bumped per search, so results of an older one are dropped.
    seq: u64,
    /// egui time of the last query or option change; the search starts `DEBOUNCE` after it.
    changed_at: Option<f64>,
    scroll_list: bool,
    preview: Option<Preview>,
    /// (seq, row) the preview was last scrolled to.
    previewed: Option<(u64, usize)>,
}

struct Preview {
    path: PathBuf,
    text: String,
    syntax: String,
    hl: HlCache,
    /// Char index at which each line starts.
    line_starts: Vec<usize>,
    scroll_to: Option<usize>,
    /// (first, last) line shown in the previous frame; only those get highlighted.
    visible: (usize, usize),
}

impl FindFiles {
    pub(super) fn set_results(&mut self, seq: u64, results: Vec<FileResult>, truncated: bool) {
        if seq != self.seq {
            return;
        }
        self.rows = results.iter().enumerate().flat_map(|(f, r)| (0..r.matches.len()).map(move |m| (f, m))).collect();
        self.results = results;
        self.truncated = truncated;
        self.searched = true;
        self.running = false;
        self.selected = 0;
        self.scroll_list = true;
    }

    fn row(&self, i: usize) -> Option<(&FileResult, &Match)> {
        let &(f, m) = self.rows.get(i)?;
        let r = &self.results[f];
        Some((r, &r.matches[m]))
    }
}

#[cfg(test)]
impl FindFiles {
    pub(super) fn changed_at_for_test(&mut self) {
        self.changed_at = Some(f64::NEG_INFINITY);
    }
    pub(super) fn rows_len(&self) -> usize {
        self.rows.len()
    }
    pub(super) fn row_path(&self, i: usize) -> PathBuf {
        self.row(i).map(|(r, _)| r.path.clone()).unwrap_or_default()
    }
    pub(super) fn preview_path(&self) -> Option<PathBuf> {
        self.preview.as_ref().map(|p| p.path.clone())
    }
}

impl App {
    pub(super) fn open_find_files(&mut self) {
        let sel = self.active.and_then(|i| self.docs[i].selected_text()).filter(|s| !s.is_empty() && !s.contains('\n')).map(String::from);
        let f = &mut self.find_files;
        if let Some(s) = sel {
            f.query = s;
        }
        f.open = true;
        f.focus = true;
        // Files may have changed since the last search, so run it again right away.
        if !f.query.is_empty() {
            f.changed_at = Some(f64::NEG_INFINITY);
        }
    }

    fn start_find_files_search(&mut self) {
        let f = &mut self.find_files;
        f.seq += 1;
        f.changed_at = None;
        if f.query.is_empty() {
            f.results.clear();
            f.rows.clear();
            f.running = false;
            f.searched = false;
            f.truncated = false;
            return;
        }
        f.running = true;
        let (tx, ctx, root) = (self.tasks_tx.clone(), self.ctx.clone(), self.root.clone());
        let (seq, query, opts) = (f.seq, f.query.clone(), f.opts);
        std::thread::spawn(move || {
            let (results, truncated) = search::search(&root, &query, opts, MAX_MATCHES);
            let _ = tx.send(Task::Search { seq, results, truncated });
            ctx.request_repaint();
        });
    }

    /// Load the selected match's file into the preview, and scroll it there when the selection moved.
    fn sync_find_preview(&mut self) {
        let f = &mut self.find_files;
        let Some((r, m)) = f.row(f.selected) else {
            f.preview = None;
            f.previewed = None;
            return;
        };
        let (path, line) = (r.path.clone(), m.line);
        if f.preview.as_ref().is_none_or(|p| p.path != path) {
            let text = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
            let syntax = self.syn.detect(Some(&path), &text);
            let mut line_starts = vec![0];
            for (i, c) in text.chars().enumerate() {
                if c == '\n' {
                    line_starts.push(i + 1);
                }
            }
            f.preview = Some(Preview { path, text, syntax, hl: HlCache::default(), line_starts, scroll_to: None, visible: (0, 100) });
            f.previewed = None;
        }
        if f.previewed != Some((f.seq, f.selected)) {
            f.previewed = Some((f.seq, f.selected));
            if let Some(p) = &mut f.preview {
                p.scroll_to = Some(line);
                p.visible = (line.saturating_sub(40), line + 40);
            }
        }
    }

    pub(super) fn find_files_ui(&mut self) {
        if !self.find_files.open {
            return;
        }
        let t = self.theme.clone();
        let ctx = self.ctx.clone();
        let now = ctx.input(|i| i.time);
        let (down, up, page_down, page_up, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::PageDown),
                i.consume_key(Modifiers::NONE, Key::PageUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        {
            let f = &mut self.find_files;
            let last = f.rows.len().saturating_sub(1);
            let before = f.selected;
            if down {
                f.selected = (f.selected + 1).min(last);
            }
            if up {
                f.selected = f.selected.saturating_sub(1);
            }
            if page_down {
                f.selected = (f.selected + 10).min(last);
            }
            if page_up {
                f.selected = f.selected.saturating_sub(10);
            }
            f.scroll_list |= f.selected != before;
        }
        self.sync_find_preview();

        let mut open_row = enter.then_some(self.find_files.selected);
        let screen = ctx.content_rect();
        let width = 960f32.min(screen.width() - 48.0);
        let height = 720f32.min(screen.height() - 96.0);
        let font = FontId::monospace(self.cfg.font_size);
        let area = egui::Area::new(Id::new("rim-find-files"))
            .order(Order::Foreground)
            .fixed_pos(pos2(screen.center().x - width / 2.0, screen.top() + 48.0))
            .show(&ctx, |ui| {
                Frame::popup(ui.style()).fill(t.bg_float).inner_margin(10.0).show(ui, |ui| {
                    ui.set_width(width);
                    ui.set_height(height);
                    self.find_files_header(ui, &t, now);
                    ui.add_space(6.0);
                    let list_h = (height - 80.0) * 0.42;
                    if let Some(i) = self.find_files_list(ui, &t, list_h) {
                        open_row = Some(i);
                    }
                    ui.separator();
                    self.find_files_preview(ui, &t, &font);
                });
            });

        let f = &mut self.find_files;
        if let Some(t0) = f.changed_at {
            if now - t0 >= DEBOUNCE {
                self.start_find_files_search();
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(DEBOUNCE - (now - t0)));
            }
        }
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx.pointer_interact_pos().is_some_and(|p| !area.response.rect.contains(p));
        if esc || clicked_outside {
            self.find_files.open = false;
            self.focus_editor = true;
        } else if let Some((r, m)) = open_row.and_then(|i| self.find_files.row(i)) {
            let (p, line, col, len) = (r.path.clone(), m.line, m.col, m.len);
            self.find_files.open = false;
            self.open_at(&p, line, col, len);
            self.focus_editor = true;
        }
    }

    fn find_files_header(&mut self, ui: &mut Ui, t: &Theme, now: f64) {
        let f = &mut self.find_files;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Find in Files").strong().color(t.fg));
            ui.add_space(8.0);
            if f.running {
                ui.spinner();
            } else if f.searched {
                let more = if f.truncated { "+" } else { "" };
                let n = f.rows.len();
                let text = format!("{n}{more} {} in {} files", if n == 1 { "match" } else { "matches" }, f.results.len());
                ui.label(RichText::new(text).size(12.0).color(t.comment));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut toggle = |ui: &mut Ui, label: &str, tip: &str, on: &mut bool| {
                    let text = RichText::new(label).size(12.5).color(if *on { t.blue } else { t.comment });
                    if ui.add(egui::Button::new(text).frame(*on)).on_hover_text(tip).clicked() {
                        *on = !*on;
                        f.changed_at = Some(f64::NEG_INFINITY);
                    }
                };
                toggle(ui, "W", "Whole words", &mut f.opts.whole_word);
                toggle(ui, "Aa", "Match case", &mut f.opts.case_sensitive);
            });
        });
        let id = Id::new("rim-find-files-input");
        let resp = ui.add(
            TextEdit::singleline(&mut f.query)
                .id(id)
                .hint_text("Text to find")
                .desired_width(f32::INFINITY)
                .font(FontId::proportional(15.0)),
        );
        if f.focus {
            f.focus = false;
            resp.request_focus();
            // Select the old query, so typing replaces it.
            if let Some(mut st) = TextEdit::load_state(ui.ctx(), id) {
                let end = CCursor::new(f.query.chars().count());
                st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), end)));
                st.store(ui.ctx(), id);
            }
        }
        if resp.changed() {
            f.changed_at = Some(now);
        }
    }

    /// Returns the row to open (double-click).
    fn find_files_list(&mut self, ui: &mut Ui, t: &Theme, height: f32) -> Option<usize> {
        let f = &mut self.find_files;
        let mut open = None;
        ScrollArea::vertical().id_salt("rim-find-files-list").max_height(height).min_scrolled_height(height).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            if f.rows.is_empty() {
                let msg = if f.query.is_empty() { "Type to search in all files" } else if f.searched && !f.running { "No matches" } else { "" };
                ui.label(RichText::new(msg).color(t.comment));
            }
            let font = FontId::monospace(12.5);
            for i in 0..f.rows.len() {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                if i == f.selected && f.scroll_list {
                    resp.scroll_to_me(None);
                }
                if resp.clicked() {
                    f.selected = i;
                }
                if resp.double_clicked() {
                    open = Some(i);
                }
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let Some((r, m)) = f.row(i) else { continue };
                let p = ui.painter();
                if i == f.selected {
                    p.rect_filled(rect, 4.0, t.selection);
                } else if resp.hovered() {
                    p.rect_filled(rect, 4.0, t.bg_highlight);
                }
                let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let right = p.text(
                    pos2(rect.right() - 8.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    format!("{name} {}", m.line + 1),
                    FontId::proportional(12.5),
                    t.comment,
                );
                let max_w = (right.left() - rect.left() - 24.0).max(40.0);
                let galley = ui.ctx().fonts_mut(|fo| fo.layout_job(match_job(m, t, &font, max_w)));
                p.galley(pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0), galley, t.fg_dim);
            }
        });
        f.scroll_list = false;
        open
    }

    fn find_files_preview(&mut self, ui: &mut Ui, t: &Theme, font: &FontId) {
        let f = &mut self.find_files;
        let (Some(p), Some((r, m))) = (f.preview.as_mut(), f.rows.get(f.selected).map(|&(fi, mi)| (&f.results[fi], &f.results[fi].matches[mi])))
        else {
            return;
        };
        ui.label(RichText::new(format!("{}:{}", r.rel, m.line + 1)).size(12.5).color(t.blue));
        let (line, col, len) = (m.line, m.col, m.len);
        let (row_h, char_w) = ui.ctx().fonts_mut(|fo| (fo.row_height(font), fo.glyph_width(font, '0')));
        let job = p.hl.layout_job_window(&p.text, &self.syn, &p.syntax, t, font, Some(p.visible));
        if p.hl.needs_more() {
            ui.ctx().request_repaint();
        }
        let galley = ui.ctx().fonts_mut(|fo| fo.layout_job(job));
        let lines = p.line_starts.len();
        let gutter = char_w * (lines.to_string().len() as f32 + 3.0);
        let view_h = ui.available_height();
        let mut area = ScrollArea::both().id_salt("rim-find-files-preview").auto_shrink([false, false]);
        if let Some(l) = p.scroll_to.take() {
            area = area.vertical_scroll_offset((l as f32 * row_h - view_h / 3.0).max(0.0));
        }
        let visible = &mut p.visible;
        let line_starts = &p.line_starts;
        area.show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(gutter + galley.size().x + 16.0, galley.size().y.max(row_h)), Sense::hover());
            let origin = rect.min + vec2(gutter, 0.0);
            let painter = ui.painter();
            let start = line_starts.get(line).copied().unwrap_or(0);
            let at = |c: usize| galley.pos_from_cursor(CCursor::new(c)).translate(origin.to_vec2());
            let line_rect = at(start);
            painter.rect_filled(
                egui::Rect::from_x_y_ranges(rect.left()..=rect.right().max(ui.clip_rect().right()), line_rect.y_range()),
                0.0,
                t.bg_highlight,
            );
            let hit = at(start + col).union(at(start + col + len));
            painter.rect_filled(hit.expand2(vec2(1.0, 0.0)), 2.0, match_bg(t));
            painter.galley(origin, galley.clone(), t.fg);
            let clip = ui.clip_rect();
            let first = ((clip.top() - rect.top()) / row_h).floor().max(0.0) as usize;
            let last = (((clip.bottom() - rect.top()) / row_h).ceil() as usize).min(lines.saturating_sub(1));
            for l in first..=last {
                let color = if l == line { t.fg_dim } else { t.comment };
                let y = at(line_starts[l]).center().y;
                painter.text(pos2(origin.x - char_w * 1.5, y), Align2::RIGHT_CENTER, (l + 1).to_string(), font.clone(), color);
            }
            *visible = (first, last);
        });
    }
}

fn match_bg(t: &Theme) -> Color32 {
    t.yellow.gamma_multiply(if t.dark { 0.3 } else { 0.45 })
}

/// One result line with the match highlighted, cut to `max_w` with an ellipsis.
fn match_job(m: &Match, t: &Theme, font: &FontId, max_w: f32) -> LayoutJob {
    let chars: Vec<char> = m.text.chars().collect();
    let a = m.text_col.min(chars.len());
    let b = (a + m.len).min(chars.len());
    // Keep the match visible when it sits far to the right on a long line.
    let skip = a.saturating_sub(60);
    let mut job = LayoutJob::default();
    let plain = TextFormat { font_id: font.clone(), color: t.fg_dim, ..Default::default() };
    let hit = TextFormat { font_id: font.clone(), color: t.fg, background: match_bg(t), ..Default::default() };
    let prefix = if skip > 0 { "…" } else { "" };
    job.append(&format!("{prefix}{}", chars[skip..a].iter().collect::<String>()), 0.0, plain.clone());
    job.append(&chars[a..b].iter().collect::<String>(), 0.0, hit);
    job.append(&chars[b..].iter().collect::<String>(), 0.0, plain);
    job.wrap = TextWrapping { max_width: max_w, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    job
}
