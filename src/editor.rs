//! Documents and the code editor widget (gutter, git markers, diagnostics,
//! completion and hover popups) built on top of egui's `TextEdit`.

use std::path::PathBuf;
use std::time::SystemTime;

use egui::text::{CCursor, CCursorRange};
use egui::text_edit::TextEditState;
use egui::{
    Align, Align2, Color32, FontId, Id, Key, Modifiers, Order, Pos2, Rect, ScrollArea, Sense, Shape, Stroke,
    TextBuffer, TextEdit, Ui, pos2, vec2,
};

use crate::config;
use crate::git::{self, LineMark};
use crate::highlight::{HlCache, Syntaxes};
use crate::lsp::{self, CompletionItem, Diagnostic};
use crate::text::{LineIndex, char_to_byte, expand_snippet, fuzzy_score, hash_str, is_word_char};
use crate::theme::Theme;
use crate::vim;

pub struct Document {
    pub id: u64,
    pub path: Option<PathBuf>,
    /// Title for virtual documents (diffs, logs).
    pub title: String,
    /// Identifies a virtual document so it can be reused.
    pub virtual_key: Option<String>,
    pub text: String,
    pub hash: u64,
    pub saved_hash: u64,
    pub read_only: bool,
    pub syntax: String,
    pub ext: String,
    pub comment: Option<&'static str>,
    pub hl: HlCache,
    pub index: LineIndex,
    pub lsp_version: i32,
    pub lsp_hash: u64,
    /// `App::lsp_gen` when this document was last synced with language servers.
    pub lsp_gen: u64,
    pub git_base: Option<String>,
    pub marks: Vec<(usize, usize, LineMark)>,
    marks_key: (u64, u64),
    base_hash: u64,
    marks_due: Option<std::time::Instant>,
    pub pending_sel: Option<(usize, usize)>,
    pub mtime: Option<SystemTime>,
    pub cursor_char: usize,
    pub vim: vim::Vim,
    /// Last selection (sorted char range) while the editor had focus. egui drops the selection of an
    /// unfocused text field, so this is what Claude context and find rely on.
    pub sel: (usize, usize),
    /// Re-apply `sel` to the editor when it next has focus (e.g. after find-next from the find bar).
    pub reselect: bool,
    /// Visible (first, last) line in the previous frame; highlighting colours around it.
    pub vis: Option<(usize, usize)>,
    /// The file uses CRLF line endings; new lines follow suit and saving keeps them consistent.
    pub crlf: bool,
    /// egui's own undo history was sized for this document.
    undo_configured: bool,
}

impl Document {
    pub fn new(id: u64, path: Option<PathBuf>, text: String, syn: &Syntaxes) -> Self {
        let syntax = syn.detect(path.as_deref(), &text);
        let ext = path
            .as_ref()
            .and_then(|p| p.extension())
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let file_name = path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string());
        let crlf = text.contains("\r\n");
        let title = file_name.clone().unwrap_or_else(|| "untitled".into());
        let mtime = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok());
        let hash = hash_str(&text);
        Self {
            id,
            comment: config::comment_token(&ext, file_name.as_deref().unwrap_or("")),
            path,
            title,
            virtual_key: None,
            index: LineIndex::new(&text),
            text,
            hash,
            saved_hash: hash,
            read_only: false,
            syntax,
            ext,
            hl: HlCache::default(),
            lsp_version: 0,
            lsp_hash: 0,
            lsp_gen: 0,
            git_base: None,
            marks: Vec::new(),
            marks_key: (0, 0),
            base_hash: 0,
            marks_due: None,
            pending_sel: Some((0, 0)),
            mtime,
            cursor_char: 0,
            vim: vim::Vim::default(),
            crlf,
            undo_configured: false,
            sel: (0, 0),
            vis: None,
            reselect: false,
        }
    }

    /// Point the document at a new path (after a rename), updating everything derived from it.
    pub fn set_path(&mut self, path: PathBuf, syn: &Syntaxes) {
        self.ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        self.comment = config::comment_token(&self.ext, &name);
        self.syntax = syn.detect(Some(&path), &self.text);
        self.title = name;
        self.path = Some(path);
        self.lsp_version = 0;
        self.lsp_gen = 0;
    }

    /// The text as written to disk: line endings made consistent for CRLF files.
    pub fn disk_text(&self) -> std::borrow::Cow<'_, str> {
        if self.crlf && self.text.split("\r\n").any(|part| part.contains('\n')) {
            std::borrow::Cow::Owned(self.text.replace("\r\n", "\n").replace('\n', "\r\n"))
        } else {
            std::borrow::Cow::Borrowed(&self.text)
        }
    }

    /// The selected text, if any (from the remembered selection).
    pub fn selected_text(&self) -> Option<&str> {
        let (a, b) = self.sel;
        if a >= b || b > self.index.total_chars {
            return None;
        }
        let (ba, bb) = (self.index.char_to_byte(&self.text, a), self.index.char_to_byte(&self.text, b));
        Some(&self.text[ba..bb])
    }

    pub fn is_dirty(&self) -> bool {
        !self.read_only && self.hash != self.saved_hash
    }

    pub fn editor_id(&self) -> Id {
        Id::new(("rim-editor", self.id))
    }

    /// Recompute derived data after the text may have changed.
    pub fn refresh(&mut self) {
        let h = hash_str(&self.text);
        if h != self.hash || self.index.hash != h {
            self.hash = h;
            self.index = LineIndex::new(&self.text);
        }
        if self.git_base.is_none() {
            self.marks.clear();
            self.marks_due = None;
        } else if (self.hash, self.base_hash) != self.marks_key {
            self.marks_key = (self.hash, self.base_hash);
            // Diffing a big file on every keystroke is wasteful: wait for a pause in typing.
            let delay = if self.marks.is_empty() { 0 } else { 250 };
            self.marks_due = Some(std::time::Instant::now() + std::time::Duration::from_millis(delay));
        }
        self.update_marks();
    }

    /// Recompute git change markers if they are due. Returns true while an update is pending.
    pub fn update_marks(&mut self) -> bool {
        match (self.marks_due, &self.git_base) {
            (Some(due), Some(base)) if std::time::Instant::now() >= due => {
                self.marks = git::line_marks(base, &self.text);
                self.marks_due = None;
                false
            }
            (Some(_), Some(_)) => true,
            _ => false,
        }
    }

    /// Set the HEAD version of this file, used for the gutter change markers.
    pub fn set_git_base(&mut self, base: Option<String>) {
        self.base_hash = base.as_deref().map(hash_str).unwrap_or(0);
        self.git_base = base;
        self.marks.clear();
        self.refresh();
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.refresh();
    }

    /// Place the cursor at (line, utf16 col) as reported by an LSP server.
    pub fn goto_lsp(&mut self, line: usize, col16: usize) {
        let c = self.index.lsp_to_char(&self.text, line, col16);
        self.pending_sel = Some((c, c));
    }

    pub fn goto_line(&mut self, line: usize) {
        let l = line.min(self.index.line_count().saturating_sub(1));
        let c = self.index.char_starts[l];
        self.pending_sel = Some((c, c));
    }

    pub fn line_col(&self) -> (usize, usize) {
        self.index.line_col(self.cursor_char.min(self.index.total_chars))
    }
}

pub struct Completion {
    pub doc_id: u64,
    pub items: Vec<CompletionItem>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub anchor: usize,
    pub incomplete: bool,
    scroll_to_selected: bool,
}

impl Completion {
    pub fn new(doc_id: u64, items: Vec<CompletionItem>, anchor: usize, incomplete: bool) -> Self {
        Self { doc_id, items, filtered: vec![], selected: 0, anchor, incomplete, scroll_to_selected: true }
    }

    pub fn refilter(&mut self, prefix: &str) {
        let p = prefix.to_lowercase();
        let mut scored: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let f = it.filter.to_lowercase();
                if p.is_empty() {
                    Some((0, i))
                } else if f.starts_with(&p) {
                    Some((10_000, i))
                } else {
                    fuzzy_score(&p, &f).map(|s| (s, i))
                }
            })
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        self.filtered = scored.into_iter().map(|(_, i)| i).take(300).collect();
        self.selected = 0;
        self.scroll_to_selected = true;
    }
}

pub struct Hover {
    pub doc_id: u64,
    pub char_idx: usize,
    pub pos: Pos2,
    pub diags: Vec<(u8, String)>,
    pub text: Option<String>,
    rect: Option<Rect>,
    /// Mouse hovers close when the pointer moves away; Ctrl+K hovers stay.
    mouse: bool,
}

pub enum EditorEvent {
    Complete { anchor: usize },
    /// A completion without its extra edits was accepted; its text starts at char `at`. The
    /// server may add an import when asked for the item's details (`completionItem/resolve`).
    CompletionAccepted { item: CompletionItem, at: usize },
    Hover(usize),
    Definition(usize),
    Save,
    SaveClose,
    Close(bool),
    /// The gutter was clicked on this line (0-based).
    ToggleBreakpoint(usize),
}

pub struct EditorCtx<'a> {
    pub theme: &'a Theme,
    pub syn: &'a Syntaxes,
    pub font: FontId,
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub diagnostics: &'a [Diagnostic],
    pub completion: &'a mut Option<Completion>,
    pub hover: &'a mut Option<Hover>,
    pub matches: &'a [(usize, usize)],
    pub current_match: Option<(usize, usize)>,
    pub request_focus: bool,
    pub triggers: &'a [String],
    /// Present when vim motions are enabled.
    pub vim: Option<&'a mut vim::Shared>,
    /// Breakpoint lines (0-based) and, while stopped in the debugger, the line being executed.
    pub breakpoints: &'a [usize],
    pub exec_line: Option<usize>,
}

impl EditorCtx<'_> {
    fn indent_unit(&self) -> String {
        if self.insert_spaces { " ".repeat(self.tab_size) } else { "\t".into() }
    }
}

fn replace_chars(text: &mut String, a: usize, b: usize, with: &str) {
    let ba = char_to_byte(text, a);
    let bb = char_to_byte(text, b);
    text.replace_range(ba..bb, with);
}


/// `word_start` for a document, scanning only the cursor's line.
fn doc_word_start(doc: &Document, c: usize) -> usize {
    let c = c.min(doc.index.total_chars);
    let line = doc.index.line_of_char(c);
    let lb = doc.index.byte_starts[line];
    let b = doc.index.char_to_byte(&doc.text, c);
    c - doc.text[lb..b].chars().rev().take_while(|ch| is_word_char(*ch)).count()
}

/// Character ranges of all (case-insensitive) matches of `query`.
pub fn find_matches(text: &str, query: &str) -> Vec<(usize, usize)> {
    crate::text::find_ci(text, query, 10_000)
}

/// Byte length of a line's leading spaces and tabs (safe to slice with).
fn indent_len(l: &str) -> usize {
    l.len() - l.trim_start_matches([' ', '\t']).len()
}

/// Replace whole lines `first..=last` using `f`, returning the new char range of the region.
fn transform_lines(doc: &mut Document, first: usize, last: usize, f: impl Fn(&str) -> String) -> (usize, usize) {
    let idx = &doc.index;
    let start_b = idx.byte_starts[first];
    let end_b = idx.byte_starts.get(last + 1).map(|b| b - 1).unwrap_or(doc.text.len());
    let region = &doc.text[start_b..end_b];
    let new: Vec<String> = region.split('\n').map(&f).collect();
    let new = new.join("\n");
    let start_c = idx.char_starts[first];
    let new_len = new.chars().count();
    doc.text.replace_range(start_b..end_b, &new);
    doc.refresh();
    (start_c, start_c + new_len)
}

fn line_range(doc: &Document, a: usize, b: usize) -> (usize, usize) {
    let first = doc.index.line_of_char(a);
    let mut last = doc.index.line_of_char(b);
    if b > a && last > first && doc.index.char_starts[last] == b {
        last -= 1;
    }
    (first, last)
}

/// Apply language-server text edits ((line, utf-16 col) ranges, in the server's order) to a
/// document, keeping the cursor and selection on the same text. Returns false if nothing changed.
pub fn apply_text_edits(doc: &mut Document, edits: &[crate::lsp::AdditionalEdit]) -> bool {
    let mut chars: Vec<(usize, usize, usize, &str)> = edits
        .iter()
        .enumerate()
        .map(|(n, (s, e, t))| {
            let a = doc.index.lsp_to_char(&doc.text, s.0, s.1);
            let b = doc.index.lsp_to_char(&doc.text, e.0, e.1).max(a);
            (a, b, n, t.as_str())
        })
        .collect();
    if chars.is_empty() {
        return false;
    }
    // Where a char position ends up after all edits.
    let map = |p: usize| -> usize {
        let mut delta: isize = 0;
        for &(a, b, _, t) in &chars {
            let len = t.chars().count() as isize;
            if b <= p && !(a == b && a == p) {
                delta += len - (b - a) as isize;
            } else if a < p && p < b {
                return (a as isize + delta + len).max(0) as usize;
            }
        }
        (p as isize + delta).max(0) as usize
    };
    let (sa, sb) = doc.sel;
    let cur = doc.cursor_char;
    let (nsa, nsb, ncur) = (map(sa), map(sb), map(cur));
    // Back to front; edits at the same position keep their order (per the LSP spec).
    chars.sort_by_key(|e| std::cmp::Reverse((e.0, e.2)));
    let before = doc.hash;
    for &(a, b, _, t) in &chars {
        replace_chars(&mut doc.text, a, b, t);
    }
    doc.refresh();
    if doc.hash == before {
        return false;
    }
    doc.sel = (nsa, nsb);
    doc.cursor_char = ncur;
    doc.pending_sel = Some(if sa == sb { (ncur, ncur) } else if cur == sa { (nsb, nsa) } else { (nsa, nsb) });
    true
}

/// Apply a completion item, returning the new selection and where the item's text starts.
fn apply_completion(doc: &mut Document, item: &CompletionItem, cursor: usize) -> ((usize, usize), usize) {
    let start = item
        .edit_start
        .map(|(l, c)| doc.index.lsp_to_char(&doc.text, l, c))
        .unwrap_or_else(|| doc_word_start(doc, cursor))
        .min(cursor);
    // Replace up to the server's range end, or the cursor if the user typed past it.
    let end = item.edit_end.map(|(l, c)| doc.index.lsp_to_char(&doc.text, l, c)).unwrap_or(cursor).max(cursor);
    let (ins, sel) = if item.snippet { expand_snippet(&item.insert) } else { (item.insert.clone(), None) };
    let mut edits: Vec<(usize, usize, String)> = item
        .additional
        .iter()
        .map(|(s, e, t)| (doc.index.lsp_to_char(&doc.text, s.0, s.1), doc.index.lsp_to_char(&doc.text, e.0, e.1), t.clone()))
        .filter(|(s, e, _)| *e <= start || *s >= end)
        .collect();
    let shift: isize = edits
        .iter()
        .filter(|(s, _, _)| *s < start)
        .map(|(s, e, t)| t.chars().count() as isize - (*e - *s) as isize)
        .sum();
    edits.push((start, end, ins.clone()));
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (s, e, t) in &edits {
        replace_chars(&mut doc.text, *s, *e, t);
    }
    doc.refresh();
    let base = (start as isize + shift) as usize;
    let sel = match sel {
        Some((a, b)) => (base + a, base + b),
        None => {
            let end = base + ins.chars().count();
            (end, end)
        }
    };
    (sel, base)
}

/// Editing keys we handle ourselves instead of egui. Returns a new selection if the text changed.
fn handle_edit_keys(ui: &Ui, doc: &mut Document, cx: &EditorCtx, a: usize, b: usize) -> Option<(usize, usize)> {
    let consume = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
    let unit = cx.indent_unit();

    if consume(Modifiers::SHIFT, Key::Tab) {
        let (first, last) = line_range(doc, a, b);
        let tab = cx.tab_size;
        let removed_first = {
            let l = doc.index.line_text(&doc.text, first);
            if l.starts_with('\t') { 1 } else { l.chars().take(tab).take_while(|c| *c == ' ').count() }
        };
        let dedent = |l: &str| {
            if let Some(r) = l.strip_prefix('\t') {
                return r.to_string();
            }
            let n = l.chars().take(tab).take_while(|c| *c == ' ').count();
            l[n..].to_string()
        };
        let region = transform_lines(doc, first, last, dedent);
        return Some(if a == b && first == last {
            let c = a.saturating_sub(removed_first).max(region.0);
            (c, c)
        } else {
            region
        });
    }

    if consume(Modifiers::NONE, Key::Tab) {
        let (first, last) = line_range(doc, a, b);
        if first != last {
            return Some(transform_lines(doc, first, last, |l| if l.is_empty() { String::new() } else { format!("{unit}{l}") }));
        }
        let ins = if cx.insert_spaces {
            let col = a - doc.index.char_starts[first];
            " ".repeat(cx.tab_size - col % cx.tab_size)
        } else {
            "\t".into()
        };
        replace_chars(&mut doc.text, a, b, &ins);
        let c = a + ins.chars().count();
        return Some((c, c));
    }

    if consume(Modifiers::NONE, Key::Enter) {
        let nl = if doc.crlf { "\r\n" } else { "\n" };
        let line = doc.index.line_of_char(a);
        let lt = doc.index.line_text(&doc.text, line);
        let col = a - doc.index.char_starts[line];
        let before: String = lt.chars().take(col).collect();
        let indent: String = before.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let prev = before.trim_end().chars().last();
        let next = doc.text[doc.index.char_to_byte(&doc.text, b)..].chars().next();
        let colon_lang = matches!(doc.ext.as_str(), "py" | "yaml" | "yml" | "nim");
        let opens = matches!(prev, Some('{' | '[' | '(')) || (colon_lang && prev == Some(':'));
        let (ins, cursor_off) = if opens && matches!(next, Some('}' | ']' | ')')) {
            let first = format!("{nl}{indent}{unit}");
            let n = first.chars().count();
            (format!("{first}{nl}{indent}"), n)
        } else if opens {
            let s = format!("{nl}{indent}{unit}");
            let n = s.chars().count();
            (s, n)
        } else {
            let s = format!("{nl}{indent}");
            let n = s.chars().count();
            (s, n)
        };
        replace_chars(&mut doc.text, a, b, &ins);
        return Some((a + cursor_off, a + cursor_off));
    }

    if consume(Modifiers::COMMAND, Key::Slash) {
        let token = doc.comment?;
        let (first, last) = line_range(doc, a, b);
        let lines: Vec<String> = (first..=last).map(|l| doc.index.line_text(&doc.text, l).to_string()).collect();
        let non_empty: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
        if non_empty.is_empty() {
            return None;
        }
        let all_commented = non_empty.iter().all(|l| l[indent_len(l)..].starts_with(token));
        let min_indent = non_empty.iter().map(|l| indent_len(l)).min().unwrap_or(0);
        let delta: isize;
        let region = if all_commented {
            let with_space = format!("{token} ");
            delta = -(if non_empty[0][indent_len(non_empty[0])..].starts_with(&with_space) { with_space.len() } else { token.len() } as isize);
            transform_lines(doc, first, last, |l| {
                let ind = indent_len(l);
                let rest = &l[ind..];
                let rest = rest.strip_prefix(&with_space).or_else(|| rest.strip_prefix(token)).unwrap_or(rest);
                format!("{}{}", &l[..ind], rest)
            })
        } else {
            delta = token.len() as isize + 1;
            transform_lines(doc, first, last, |l| {
                if l.trim().is_empty() { l.to_string() } else { format!("{}{token} {}", &l[..min_indent], &l[min_indent..]) }
            })
        };
        return Some(if a == b && first == last {
            let c = ((a as isize + delta).max(region.0 as isize)) as usize;
            (c, c)
        } else {
            region
        });
    }
    None
}

/// Pull vim-relevant input out of egui's event queue so `TextEdit` never sees it.
/// In insert mode only Esc is taken (everything after it in the same frame too),
/// leaving typing, IME and selection to the regular text widget.
fn take_vim_keys(ui: &Ui, mode: vim::Mode) -> Vec<vim::Key> {
    use egui::Event;
    ui.input_mut(|i| {
        let mut keys = Vec::new();
        let mut modal = mode != vim::Mode::Insert;
        i.events.retain(|e| match e {
            Event::Text(s) if modal => {
                keys.extend(s.chars().filter(|c| !c.is_control()).map(vim::Key::Char));
                false
            }
            Event::Paste(_) | Event::Cut if modal => false,
            // Input-method commits (CJK etc.) are keys like any other outside insert mode.
            Event::Ime(egui::ImeEvent::Commit(s)) if modal => {
                keys.extend(s.chars().filter(|c| !c.is_control()).map(vim::Key::Char));
                false
            }
            Event::Ime(_) if modal => false,
            Event::Key { key, pressed: true, modifiers, .. } => {
                let ctrl = modifiers.ctrl || modifiers.command;
                if !modal {
                    if *key == Key::Escape && !ctrl {
                        keys.push(vim::Key::Esc);
                        modal = true;
                        return false;
                    }
                    return true;
                }
                if modifiers.alt {
                    return true;
                }
                if ctrl {
                    let c = match key {
                        Key::R => 'r',
                        Key::D => 'd',
                        Key::U => 'u',
                        Key::F => 'f',
                        Key::B => 'b',
                        Key::Z => 'z',
                        Key::Y => 'y',
                        _ => return true,
                    };
                    keys.push(vim::Key::Ctrl(c));
                    return false;
                }
                let k = match key {
                    Key::Escape => vim::Key::Esc,
                    Key::Enter => vim::Key::Enter,
                    Key::Backspace => vim::Key::Backspace,
                    Key::Delete => vim::Key::Char('x'),
                    Key::ArrowLeft => vim::Key::Char('h'),
                    Key::ArrowRight => vim::Key::Char('l'),
                    Key::ArrowUp => vim::Key::Char('k'),
                    Key::ArrowDown => vim::Key::Char('j'),
                    Key::Home => vim::Key::Char('0'),
                    Key::End => vim::Key::Char('$'),
                    Key::PageUp => vim::Key::Ctrl('b'),
                    Key::PageDown => vim::Key::Ctrl('f'),
                    // Letters arrive as Event::Text; Tab and the rest are swallowed.
                    _ => return false,
                };
                keys.push(k);
                false
            }
            _ => true,
        });
        keys
    })
}

/// Galley-relative rect of the position before char `ci`, like `Galley::pos_from_cursor` but
/// O(line length) instead of O(rows): lines map 1:1 to rows since the editor never wraps.
fn char_pos(galley: &egui::Galley, index: &LineIndex, ci: usize) -> Rect {
    let (line, col) = index.line_col(ci.min(index.total_chars));
    match galley.rows.get(line) {
        Some(row) => {
            let x = row.pos.x + row.row.x_offset(egui::text::CharIndex(col));
            let r = row.rect();
            Rect::from_min_max(pos2(x, r.min.y), pos2(x, r.max.y))
        }
        None => galley.pos_from_cursor(CCursor::new(ci)),
    }
}

pub fn show(ui: &mut Ui, doc: &mut Document, cx: &mut EditorCtx) -> Vec<EditorEvent> {
    doc.refresh();
    let ctx = ui.ctx().clone();
    let id = doc.editor_id();
    let font = cx.font.clone();
    let theme = cx.theme;
    let (row_h, char_w) = ctx.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let mut events = Vec::new();

    let mut state: TextEditState = TextEdit::load_state(&ctx, id).unwrap_or_default();
    let mut store = false;
    if !doc.undo_configured {
        // egui keeps a full copy of the text per undo step; keep that bounded for big files.
        doc.undo_configured = true;
        let max_undos = match doc.text.len() {
            n if n > 4 << 20 => 5,
            n if n > 1 << 20 => 15,
            n if n > 256 << 10 => 40,
            _ => 100,
        };
        let settings = egui::util::undoer::Settings { max_undos, ..Default::default() };
        state.set_undoer(egui::util::undoer::Undoer::with_settings(settings));
        store = true;
    }
    let mut scroll_to_cursor = false;
    let set_sel = |state: &mut TextEditState, (a, b): (usize, usize)| {
        state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
    };
    if let Some(sel) = doc.pending_sel.take() {
        set_sel(&mut state, sel);
        doc.sel = (sel.0.min(sel.1), sel.0.max(sel.1));
        doc.reselect = true;
        store = true;
        scroll_to_cursor = true;
    }
    if cx.request_focus {
        ctx.memory_mut(|m| m.request_focus(id));
    }
    if doc.reselect && ctx.memory(|m| m.has_focus(id)) {
        set_sel(&mut state, doc.sel);
        doc.reselect = false;
        store = true;
    }
    let has_focus = ctx.memory(|m| m.has_focus(id));
    let mut hover_at_cursor = false;
    let len_before = doc.text.len();

    let vim_on = cx.vim.is_some();
    let indent = cx.indent_unit();
    let mut scroll_minimal = false;
    if let Some(shared) = cx.vim.as_deref_mut()
        && has_focus
    {
        let keys = take_vim_keys(ui, doc.vim.mode);
        if !keys.is_empty() {
            let primary = state.cursor.char_range().map(|r| r.primary.index.0).unwrap_or(doc.cursor_char);
            let visual = matches!(doc.vim.mode, vim::Mode::Visual | vim::Mode::VisualLine);
            let mut t = if visual { vim::Text::with_buffer_cursor(&doc.text, doc.vim.visual_cursor()) } else { vim::Text::new(&doc.text, primary) };
            t.page = (ui.available_height() / row_h) as usize;
            t.read_only = doc.read_only;
            t.indent = indent;
            let mut actions = Vec::new();
            for k in keys {
                doc.vim.feed(shared, k, &mut t, &mut actions);
            }
            if t.edited() {
                doc.text = t.to_text();
                doc.refresh();
            }
            let (a, b) = doc.vim.selection(&t).unwrap_or((t.cursor, t.cursor));
            set_sel(&mut state, (t.orig(a), t.orig(b)));
            store = true;
            scroll_minimal = true;
            doc.cursor_char = t.orig(t.cursor);
            if doc.vim.mode != vim::Mode::Insert {
                *cx.completion = None;
            }
            *cx.hover = None;
            for a in actions {
                match a {
                    vim::Action::Copy(s) => ctx.copy_text(s),
                    vim::Action::Save => events.push(EditorEvent::Save),
                    vim::Action::SaveClose => events.push(EditorEvent::SaveClose),
                    vim::Action::Close { force } => events.push(EditorEvent::Close(force)),
                }
            }
        }
    }
    let block_cursor = vim_on && doc.vim.mode != vim::Mode::Insert;

    if has_focus {
        let consume = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        let range = state.cursor.char_range().unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));
        let [ca, cb] = range.sorted_cursors();
        let (a, b) = (ca.index.0, cb.index.0);

        // Completion popup navigation.
        let mut accept = None;
        if let Some(c) = cx.completion.as_mut().filter(|c| c.doc_id == doc.id && !c.filtered.is_empty()) {
            let n = c.filtered.len();
            if consume(Modifiers::NONE, Key::ArrowDown) {
                c.selected = (c.selected + 1) % n;
                c.scroll_to_selected = true;
            }
            if consume(Modifiers::NONE, Key::ArrowUp) {
                c.selected = (c.selected + n - 1) % n;
                c.scroll_to_selected = true;
            }
            if consume(Modifiers::NONE, Key::Enter) || consume(Modifiers::NONE, Key::Tab) {
                accept = Some(c.items[c.filtered[c.selected]].clone());
            }
        }
        if consume(Modifiers::NONE, Key::Escape) {
            *cx.completion = None;
            *cx.hover = None;
        }
        if let Some(item) = accept {
            *cx.completion = None;
            if !doc.read_only {
                let (sel, at) = apply_completion(doc, &item, b);
                set_sel(&mut state, sel);
                store = true;
                if item.additional.is_empty() {
                    events.push(EditorEvent::CompletionAccepted { item, at });
                }
            }
        }
        if consume(Modifiers::COMMAND, Key::Space) {
            events.push(EditorEvent::Complete { anchor: doc_word_start(doc, b) });
        }
        if consume(Modifiers::NONE, Key::F12) {
            events.push(EditorEvent::Definition(b));
        }
        if consume(Modifiers::COMMAND, Key::K) {
            hover_at_cursor = true;
        }
        if !doc.read_only && !store
            && let Some(sel) = handle_edit_keys(ui, doc, cx, a, b) {
                doc.refresh();
                set_sel(&mut state, sel);
                store = true;
                *cx.completion = None;
            }
    }
    if store {
        state.clone().store(&ctx, id);
    }

    let lines = doc.index.line_count();
    let digits = lines.to_string().len().max(3);
    let gutter_w = char_w * (digits as f32 + 3.0);
    let avail = ui.available_size();

    ScrollArea::both().id_salt(("scroll", doc.id)).auto_shrink([false, false]).show(ui, |ui| {
        let bg_idx = ui.painter().add(Shape::Noop);
        let syntax = doc.syntax.clone();
        let hl = &mut doc.hl;
        let vis = doc.vis.or(Some((0, 100)));
        let syn = cx.syn;
        let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, _w: f32| {
            let job = hl.layout_job_window(buf.as_str(), syn, &syntax, theme, &font, vis);
            ui.ctx().fonts_mut(|f| f.layout_job(job))
        };
        macro_rules! editor {
            ($te:expr) => {
                $te.id(id)
                    .font(font.clone())
                    .frame(egui::Frame::NONE)
                    .code_editor()
                    .lock_focus(true)
                    // Keep Escape (closes popups, leaves vim insert mode) instead of dropping focus.
                    .event_filter(egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, tab: true, escape: true })
                    .desired_width(f32::INFINITY)
                    .min_size(vec2((avail.x - gutter_w).max(50.0), avail.y))
                    .layouter(&mut layouter)
            };
        }
        let output = ui
            .horizontal_top(|ui| {
                ui.add_space(gutter_w);
                if block_cursor {
                    ui.visuals_mut().text_cursor.stroke = Stroke::NONE;
                }
                if doc.read_only {
                    let mut s = doc.text.as_str();
                    editor!(TextEdit::multiline(&mut s)).show(ui)
                } else {
                    editor!(TextEdit::multiline(&mut doc.text)).show(ui)
                }
            })
            .inner;
        let changed = output.response.response.changed();
        if changed {
            doc.refresh();
        }
        let galley = output.galley.clone();
        let gpos = output.galley_pos;
        let resp = output.response.response.clone();
        let clip = ui.clip_rect();
        let painter = ui.painter().clone();
        let cursor = output.cursor_range.map(|r| r.primary.index.0).unwrap_or(doc.cursor_char);
        doc.cursor_char = cursor;
        if resp.clicked() || resp.drag_started() {
            doc.reselect = false;
        }
        if ctx.memory(|m| m.has_focus(id))
            && let Some(r) = output.cursor_range
        {
            let [a, b] = r.sorted_cursors();
            doc.sel = (a.index.0, b.index.0);
        }
        let cursor_rect = char_pos(&galley, &doc.index, cursor).translate(gpos.to_vec2());
        let cur_line = doc.index.line_of_char(cursor.min(doc.index.total_chars));

        if scroll_to_cursor {
            ui.scroll_to_rect(cursor_rect.expand(row_h), Some(Align::Center));
        } else if scroll_minimal {
            ui.scroll_to_rect(cursor_rect.expand2(vec2(gutter_w + char_w * 2.0, row_h * 1.5)), None);
        }

        // Visible rows from the galley's real row positions: they are pixel-rounded and so drift
        // away from `row_h` (e.g. 16 vs 16.3125), which adds up to dozens of rows in big files.
        let first_vis = galley.rows.partition_point(|r| gpos.y + r.rect().max.y <= clip.top());
        let last_vis = galley.rows.partition_point(|r| gpos.y + r.pos.y < clip.bottom()).saturating_sub(1);
        // Colours follow the visible range with a frame's delay; repaint when it moved, and
        // while the rest of the file is still being parsed in the background.
        if doc.vis != Some((first_vis, last_vis)) {
            doc.vis = Some((first_vis, last_vis));
            ctx.request_repaint();
        }
        if doc.hl.needs_more() {
            ctx.request_repaint_after(std::time::Duration::from_millis(15));
        }
        let row_top = |line: usize| gpos.y + galley.rows.get(line).map(|r| r.pos.y).unwrap_or(line as f32 * row_h);

        // Background: current line and search matches (behind the text).
        let mut bg = vec![Shape::rect_filled(
            Rect::from_min_size(pos2(clip.left(), cursor_rect.top()), vec2(clip.width(), row_h)),
            0.0,
            theme.bg_highlight,
        )];
        if let Some(l) = cx.exec_line.filter(|l| (first_vis..=last_vis).contains(l)) {
            bg.push(Shape::rect_filled(Rect::from_min_size(pos2(clip.left(), row_top(l)), vec2(clip.width(), row_h)), 0.0, theme.yellow.gamma_multiply(0.18)));
        }
        for &(s, e) in cx.matches {
            let l = doc.index.line_of_char(s.min(doc.index.total_chars));
            if l < first_vis || l > last_vis {
                continue;
            }
            let r0 = char_pos(&galley, &doc.index, s);
            let r1 = char_pos(&galley, &doc.index, e);
            let rect = Rect::from_min_max(r0.min, pos2(r1.max.x.max(r0.min.x + 2.0), r0.max.y)).translate(gpos.to_vec2());
            let color = if cx.current_match == Some((s, e)) { theme.orange.gamma_multiply(0.55) } else { theme.yellow.gamma_multiply(0.25) };
            bg.push(Shape::rect_filled(rect, 2.0, color));
        }
        painter.set(bg_idx, Shape::Vec(bg));

        // Gutter: stays fixed at the left edge while scrolling horizontally.
        let gutter = Rect::from_min_max(pos2(clip.left(), clip.top()), pos2(clip.left() + gutter_w, clip.bottom()));
        painter.rect_filled(gutter, 0.0, theme.bg);
        let mut line_sev: std::collections::HashMap<usize, u8> = Default::default();
        for d in cx.diagnostics {
            let e = line_sev.entry(d.start.0).or_insert(d.severity);
            *e = (*e).min(d.severity);
        }
        let num_font = FontId::monospace(font.size * 0.9);
        for line in first_vis..=last_vis {
            let y = row_top(line);
            let color = match line_sev.get(&line) {
                Some(&s) if s <= 2 => theme.severity_color(s),
                _ if line == cur_line => theme.fg,
                _ => theme.gutter,
            };
            painter.text(
                pos2(gutter.right() - char_w * 1.6, y + row_h / 2.0),
                Align2::RIGHT_CENTER,
                (line + 1).to_string(),
                num_font.clone(),
                color,
            );
        }
        // Breakpoints and the execution line, at the left edge of the gutter.
        let bp_x = gutter.left() + char_w * 0.9;
        for &l in cx.breakpoints {
            if (first_vis..=last_vis).contains(&l) {
                painter.circle_filled(pos2(bp_x, row_top(l) + row_h / 2.0), row_h * 0.26, theme.red);
            }
        }
        if let Some(l) = cx.exec_line.filter(|l| (first_vis..=last_vis).contains(l)) {
            let (x, y) = (bp_x - row_h * 0.22, row_top(l) + row_h / 2.0);
            painter.add(Shape::convex_polygon(
                vec![pos2(x, y - row_h * 0.3), pos2(x + row_h * 0.5, y), pos2(x, y + row_h * 0.3)],
                theme.yellow,
                Stroke::new(1.0, theme.bg),
            ));
        }
        let gutter_resp = ui.interact(gutter, id.with("gutter"), Sense::click());
        if gutter_resp.clicked()
            && let Some(p) = gutter_resp.interact_pointer_pos()
        {
            let line = galley.rows.partition_point(|r| gpos.y + r.rect().max.y <= p.y);
            if line < lines {
                events.push(EditorEvent::ToggleBreakpoint(line));
            }
        }
        let mark_x = gutter.right() - char_w * 0.9;
        for &(s, e, kind) in &doc.marks {
            if e <= first_vis || s > last_vis {
                continue;
            }
            match kind {
                LineMark::Deleted => {
                    let y = row_top(s);
                    painter.add(Shape::convex_polygon(
                        vec![pos2(mark_x - 1.0, y - 4.0), pos2(mark_x + 5.0, y), pos2(mark_x - 1.0, y + 4.0)],
                        theme.red,
                        Stroke::NONE,
                    ));
                }
                _ => {
                    let color = if kind == LineMark::Added { theme.green } else { theme.blue };
                    let rect = Rect::from_min_max(pos2(mark_x, row_top(s)), pos2(mark_x + 3.0, row_top(e.min(lines)) .max(row_top(s) + row_h)));
                    painter.rect_filled(rect, 0.0, color);
                }
            }
        }

        // Diagnostic squiggles.
        let text_clip = Rect::from_min_max(pos2(gutter.right(), clip.top()), clip.max);
        let squiggle_painter = painter.with_clip_rect(text_clip);
        for d in cx.diagnostics {
            if d.start.0 < first_vis || d.start.0 > last_vis {
                continue;
            }
            let s = doc.index.lsp_to_char(&doc.text, d.start.0, d.start.1);
            let e = doc.index.lsp_to_char(&doc.text, d.end.0, d.end.1);
            let r0 = char_pos(&galley, &doc.index, s);
            let x1 = if d.end.0 == d.start.0 {
                char_pos(&galley, &doc.index, e).min.x
            } else {
                galley.rows.get(d.start.0).map(|r| r.rect().max.x).unwrap_or(r0.max.x)
            };
            let x0 = gpos.x + r0.min.x;
            let x1 = (gpos.x + x1).max(x0 + char_w);
            let y = gpos.y + r0.max.y - 1.5;
            let mut pts = Vec::new();
            let mut x = x0;
            let mut up = false;
            while x <= x1 {
                pts.push(pos2(x, if up { y - 1.5 } else { y + 1.0 }));
                x += 2.5;
                up = !up;
            }
            squiggle_painter.add(Shape::line(pts, Stroke::new(1.0, theme.severity_color(d.severity))));
        }

        if block_cursor {
            let r0 = char_pos(&galley, &doc.index, cursor).translate(gpos.to_vec2());
            let (l, c) = doc.index.line_col(cursor.min(doc.index.total_chars));
            let under = doc.index.line_text(&doc.text, l).chars().nth(c);
            let w = under.map(|ch| ctx.fonts_mut(|f| f.glyph_width(&font, ch))).unwrap_or(char_w).max(char_w * 0.5);
            let rect = Rect::from_min_size(r0.min, vec2(w, r0.height()));
            painter.rect_filled(rect, 1.0, theme.fg.gamma_multiply(0.85));
            if let Some(ch) = under {
                painter.text(rect.min, Align2::LEFT_TOP, ch.to_string(), font.clone(), theme.bg);
            }
        }
        if vim_on {
            let bar = Rect::from_min_max(pos2(clip.left(), clip.bottom() - row_h - 8.0), clip.max);
            if let Some((k, text)) = &doc.vim.cmdline {
                painter.rect_filled(bar, 0.0, theme.bg_float);
                let r = painter.text(pos2(bar.left() + 8.0, bar.center().y), Align2::LEFT_CENTER, format!("{k}{text}"), font.clone(), theme.fg);
                painter.rect_filled(Rect::from_min_size(pos2(r.right(), r.top()), vec2(char_w, r.height())), 1.0, theme.fg.gamma_multiply(0.6));
            } else if !doc.vim.message.is_empty() {
                painter.rect_filled(bar, 0.0, theme.bg_float);
                painter.text(pos2(bar.left() + 8.0, bar.center().y), Align2::LEFT_CENTER, &doc.vim.message, font.clone(), theme.yellow);
            }
        }

        // Ctrl+click → go to definition.
        if resp.clicked() && ui.input(|i| i.modifiers.command) {
            events.push(EditorEvent::Definition(cursor));
        }

        // Completion as you type.
        if changed && doc.text.len() > len_before && output.cursor_range.is_some_and(|r| r.is_empty()) && cursor > 0 {
            let b = doc.index.char_to_byte(&doc.text, cursor);
            let ch = doc.text[..b].chars().last().unwrap_or(' ');
            let is_trigger = cx.triggers.iter().any(|t| t.ends_with(ch));
            let open_here = cx.completion.as_ref().is_some_and(|c| c.doc_id == doc.id);
            let incomplete = cx.completion.as_ref().is_some_and(|c| c.incomplete);
            if is_trigger || (is_word_char(ch) && (!open_here || incomplete)) {
                events.push(EditorEvent::Complete { anchor: doc_word_start(doc, cursor) });
            }
        }
        if changed {
            *cx.hover = None;
        }

        // Keep the completion list in sync with what's typed.
        if let Some(c) = cx.completion.as_mut().filter(|c| c.doc_id == doc.id) {
            let single = output.cursor_range.is_none_or(|r| r.is_empty());
            let valid = single && cursor >= c.anchor && c.anchor <= doc.index.total_chars;
            let prefix: String = if valid {
                let (ba, bb) = (doc.index.char_to_byte(&doc.text, c.anchor), doc.index.char_to_byte(&doc.text, cursor));
                doc.text[ba..bb].to_string()
            } else {
                String::new()
            };
            if !valid || !prefix.chars().all(is_word_char) || !has_focus {
                *cx.completion = None;
            } else {
                if changed || c.filtered.is_empty() {
                    c.refilter(&prefix);
                }
                if c.filtered.is_empty() && !c.incomplete {
                    *cx.completion = None;
                }
            }
        }
        let accepted = show_completion(ui, cx, theme, row_h, pos2(gpos.x + char_pos(&galley, &doc.index, cx.completion.as_ref().map(|c| c.anchor).unwrap_or(0)).min.x, cursor_rect.bottom()));
        if let Some(item) = accepted {
            *cx.completion = None;
            if !doc.read_only {
                let (sel, at) = apply_completion(doc, &item, cursor);
                let mut st = TextEdit::load_state(&ctx, id).unwrap_or_default();
                set_sel(&mut st, sel);
                st.store(&ctx, id);
                ctx.memory_mut(|m| m.request_focus(id));
                if item.additional.is_empty() {
                    events.push(EditorEvent::CompletionAccepted { item, at });
                }
            }
        }

        // Hover: after the mouse rests on text, or Ctrl+K at the cursor.
        let pointer = resp.hover_pos();
        let any_pointer = ctx.pointer_hover_pos();
        let over_popup = cx.hover.as_ref().and_then(|h| h.rect).zip(any_pointer).is_some_and(|(r, p)| r.expand(8.0).contains(p));
        if let Some(h) = cx.hover.as_ref().filter(|h| h.mouse && !over_popup)
            && pointer.is_none_or(|p| h.pos.distance(p) > 24.0) {
                *cx.hover = None;
            }
        if resp.clicked() {
            *cx.hover = None;
        }
        if let Some(p) = pointer.filter(|p| p.x > gutter.right()) {
            let still = ui.input(|i| i.pointer.time_since_last_movement() > 0.45 && !i.pointer.any_down());
            if !still {
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            } else if cx.hover.is_none() {
                let cc = galley.cursor_from_pos(p - gpos);
                let r = galley.pos_from_cursor(cc).translate(gpos.to_vec2());
                // Only when actually over a character, not past the end of the line.
                if (r.center().y - p.y).abs() < row_h && p.x < r.max.x + char_w * 0.5 {
                    let ci = cc.index.0.saturating_sub(usize::from(p.x < r.min.x));
                    start_hover(doc, cx, ci, p, true, &mut events);
                }
            }
        }
        if hover_at_cursor {
            start_hover(doc, cx, cursor, cursor_rect.left_bottom() - vec2(0.0, row_h), false, &mut events);
        }
        show_hover(ui, cx, theme, row_h);
    });

    if doc.update_marks() {
        ctx.request_repaint_after(std::time::Duration::from_millis(260));
    }
    events
}

fn start_hover(doc: &Document, cx: &mut EditorCtx, ci: usize, pos: Pos2, mouse: bool, events: &mut Vec<EditorEvent>) {
    let diags = cx
        .diagnostics
        .iter()
        .filter(|d| {
            let s = doc.index.lsp_to_char(&doc.text, d.start.0, d.start.1);
            let e = doc.index.lsp_to_char(&doc.text, d.end.0, d.end.1).max(s + 1);
            (s..e).contains(&ci)
        })
        .map(|d| {
            let src = if d.source.is_empty() { String::new() } else { format!("[{}] ", d.source) };
            (d.severity, format!("{src}{}", d.message))
        })
        .collect();
    *cx.hover = Some(Hover { doc_id: doc.id, char_idx: ci, pos, diags, text: None, rect: None, mouse });
    events.push(EditorEvent::Hover(ci));
}

fn show_completion(ui: &Ui, cx: &mut EditorCtx, theme: &Theme, row_h: f32, pos: Pos2) -> Option<CompletionItem> {
    let c = cx.completion.as_mut()?;
    if c.filtered.is_empty() {
        return None;
    }
    let mut accepted = None;
    let font = FontId::monospace(cx.font.size * 0.95);
    egui::Area::new(Id::new("rim-completion"))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).fill(theme.bg_float).show(ui, |ui| {
                ui.set_width(460.0);
                ScrollArea::vertical().max_height(row_h * 11.0).show(ui, |ui| {
                    for (i, &item_idx) in c.filtered.iter().enumerate() {
                        let item = &c.items[item_idx];
                        let selected = i == c.selected;
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h + 2.0), Sense::click());
                        if selected {
                            ui.painter().rect_filled(rect, 3.0, theme.selection);
                            if c.scroll_to_selected {
                                resp.scroll_to_me(None);
                            }
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 3.0, theme.bg_highlight);
                        }
                        let p = ui.painter();
                        let y = rect.center().y;
                        p.text(pos2(rect.left() + 4.0, y), Align2::LEFT_CENTER, lsp::kind_label(item.kind), FontId::monospace(font.size * 0.8), kind_color(theme, item.kind));
                        p.text(pos2(rect.left() + 58.0, y), Align2::LEFT_CENTER, &item.label, font.clone(), theme.fg);
                        if !item.detail.is_empty() {
                            let detail: String = item.detail.chars().take(40).collect();
                            p.text(pos2(rect.right() - 6.0, y), Align2::RIGHT_CENTER, detail, FontId::proportional(font.size * 0.8), theme.comment);
                        }
                        if resp.clicked() {
                            accepted = Some(item.clone());
                        }
                    }
                });
            });
        });
    c.scroll_to_selected = false;
    accepted
}

fn kind_color(t: &Theme, kind: u8) -> Color32 {
    match kind {
        2..=4 => t.blue,
        5 | 10 => t.cyan,
        7 | 8 | 13 | 22 | 25 => t.yellow,
        9 => t.cyan,
        14 => t.purple,
        15 => t.green,
        20 | 21 | 12 => t.orange,
        _ => t.fg_dim,
    }
}

fn show_hover(ui: &Ui, cx: &mut EditorCtx, theme: &Theme, row_h: f32) {
    let Some(h) = cx.hover.as_mut() else { return };
    let text = h.text.as_deref().unwrap_or("");
    if h.diags.is_empty() && text.is_empty() {
        return;
    }
    let font = FontId::monospace(cx.font.size * 0.9);
    let resp = egui::Area::new(Id::new("rim-hover"))
        .order(Order::Tooltip)
        .fixed_pos(h.pos + vec2(0.0, row_h))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).fill(theme.bg_float).show(ui, |ui| {
                ui.set_max_width(640.0);
                ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for (sev, msg) in &h.diags {
                        ui.label(egui::RichText::new(msg).color(theme.severity_color(*sev)).font(font.clone()));
                    }
                    if !h.diags.is_empty() {
                        let key = if cfg!(target_os = "macos") { "Cmd+." } else { "Ctrl+." };
                        ui.label(egui::RichText::new(format!("{key}: quick fixes")).color(theme.comment).size(font.size * 0.9));
                    }
                    if !h.diags.is_empty() && !text.is_empty() {
                        ui.separator();
                    }
                    if !text.is_empty() {
                        ui.label(egui::RichText::new(text).color(theme.fg).font(font.clone()));
                    }
                });
            });
        });
    h.rect = Some(resp.response.rect);
}

/// Load a file from disk as a new document.
pub fn open_file(id: u64, path: PathBuf, syn: &Syntaxes) -> Result<Document, String> {
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|_| "not a UTF-8 text file".to_string())?;
    Ok(Document::new(id, Some(path), text, syn))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, RawInput};

    /// Drives `show` with synthetic input, frame by frame, without a window.
    struct Harness {
        ctx: egui::Context,
        doc: Document,
        syn: Syntaxes,
        theme: Theme,
        shared: vim::Shared,
        vim_on: bool,
        completion: Option<Completion>,
        hover: Option<Hover>,
        events: Vec<EditorEvent>,
        first: bool,
        shapes: Vec<egui::epaint::ClippedShape>,
    }

    impl Harness {
        fn new(text: &str, vim_on: bool) -> Self {
            let theme = crate::theme::by_name("tokyonight");
            let syn = Syntaxes::new(&theme);
            let doc = Document::new(1, None, text.to_string(), &syn);
            Self { ctx: egui::Context::default(), doc, syn, theme, shared: Default::default(), vim_on, completion: None, hover: None, events: vec![], first: true, shapes: vec![] }
        }

        fn frame(&mut self, events: Vec<Event>) {
            let mut input = RawInput { events, ..Default::default() };
            input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)));
            let request_focus = std::mem::take(&mut self.first);
            let mut out = self.ctx.run_ui(input, |ui| {
                let mut cx = EditorCtx {
                    theme: &self.theme,
                    syn: &self.syn,
                    font: FontId::monospace(14.0),
                    tab_size: 4,
                    insert_spaces: true,
                    diagnostics: &[],
                    completion: &mut self.completion,
                    hover: &mut self.hover,
                    matches: &[],
                    current_match: None,
                    request_focus,
                    triggers: &[],
                    vim: if self.vim_on { Some(&mut self.shared) } else { None },
                    breakpoints: &[],
                    exec_line: None,
                };
                self.events.extend(show(ui, &mut self.doc, &mut cx));
            });
            out.textures_delta.clear();
            self.shapes = out.shapes;
        }

        /// Type text; one frame per char, like real key repeat.
        fn typing(&mut self, s: &str) {
            for c in s.chars() {
                self.frame(vec![Event::Text(c.to_string())]);
            }
        }

        fn key(&mut self, key: Key) {
            let ev = Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
            self.frame(vec![ev]);
        }
    }

    #[test]
    fn gutter_numbers_every_visible_row_of_a_big_file() {
        let text: String = (0..3000).map(|i| format!("let x{i} = {i};\n")).collect();
        let mut h = Harness::new(&text, true);
        h.frame(vec![]);
        h.typing("2500G");
        // Let the scroll animation settle on the cursor.
        for _ in 0..100 {
            h.frame(vec![]);
        }
        fn numbers(s: &Shape, out: &mut Vec<usize>) {
            match s {
                Shape::Vec(v) => v.iter().for_each(|s| numbers(s, out)),
                Shape::Text(t) if (0.0..600.0).contains(&t.pos.y) => out.extend(t.galley.text().parse::<usize>().ok()),
                _ => {}
            }
        }
        let mut shown = Vec::new();
        h.shapes.iter().for_each(|c| numbers(&c.shape, &mut shown));
        shown.sort();
        assert!(shown.contains(&2500), "cursor line has no number; on screen: {shown:?}");
        assert!(shown.len() >= 36 && shown.windows(2).all(|w| w[1] == w[0] + 1), "gap in gutter: {shown:?}");
    }

    /// Frame-time benchmark on big files (the reviewer's). Run in release:
    /// `cargo test --release bench_frames -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_frames() {
        let src: String = ["app.rs", "editor.rs", "vim.rs", "chat.rs"].iter().map(|f| std::fs::read_to_string(format!("src/{f}")).unwrap()).collect();
        let src_lines: Vec<&str> = src.lines().collect();
        let ms = |d: std::time::Duration, n: usize| d.as_secs_f64() * 1e3 / n as f64;
        for &lines in &[5_000usize, 20_000, 50_000] {
            let text: String = (0..lines).map(|i| format!("{}\n", src_lines[i % src_lines.len()])).collect();
            let mut h = Harness::new(&text, false);
            h.doc.set_git_base(Some(text.clone()));
            h.doc.syntax = "Rust".into();
            let s = std::time::Instant::now();
            h.frame(vec![]);
            let first = s.elapsed();
            h.frame(vec![]);
            let n = 20;
            let s = std::time::Instant::now();
            for _ in 0..n {
                h.frame(vec![]);
            }
            let idle = s.elapsed();
            let s = std::time::Instant::now();
            for _ in 0..n {
                h.typing("a");
            }
            let typing = s.elapsed();
            let mut hv = Harness::new(&text, true);
            hv.doc.syntax = "Rust".into();
            hv.frame(vec![]);
            hv.frame(vec![]);
            let s = std::time::Instant::now();
            for _ in 0..n {
                hv.typing("j");
            }
            let j = s.elapsed();
            let s = std::time::Instant::now();
            for _ in 0..10 {
                hv.typing("x");
            }
            let x = s.elapsed();
            println!(
                "{lines:>6} lines: first {:7.1} ms | idle {:6.2} ms | typing {:6.2} ms | vim j {:6.2} ms | vim x {:6.2} ms",
                ms(first, 1),
                ms(idle, n),
                ms(typing, n),
                ms(j, n),
                ms(x, 10)
            );
        }
    }

    #[test]
    fn regular_mode_types_text_directly() {
        let mut h = Harness::new("", false);
        h.frame(vec![]);
        h.typing("hello");
        assert_eq!(h.doc.text, "hello");
    }

    #[test]
    fn vim_normal_mode_does_not_type_text() {
        let mut h = Harness::new("hello", true);
        h.frame(vec![]);
        h.typing("jkl");
        assert_eq!(h.doc.text, "hello");
        assert_eq!(h.doc.vim.mode, vim::Mode::Normal);
        assert_eq!(h.doc.cursor_char, 1);
    }

    #[test]
    fn vim_insert_then_escape() {
        let mut h = Harness::new("hello", true);
        h.frame(vec![]);
        h.typing("iXY");
        assert_eq!(h.doc.vim.mode, vim::Mode::Insert);
        assert_eq!(h.doc.text, "XYhello");
        h.key(Key::Escape);
        assert_eq!(h.doc.vim.mode, vim::Mode::Normal);
        h.typing("x");
        assert_eq!(h.doc.text, "Xhello");
    }

    #[test]
    fn vim_operators_and_dot_repeat_through_the_widget() {
        let mut h = Harness::new("one two three four", true);
        h.frame(vec![]);
        h.typing("dw.");
        assert_eq!(h.doc.text, "three four");
        h.typing("cwX");
        h.key(Key::Escape);
        assert_eq!(h.doc.text, "X four");
        h.typing("u");
        assert_eq!(h.doc.text, "three four");
    }

    #[test]
    fn vim_ex_commands_reach_the_app() {
        let mut h = Harness::new("a", true);
        h.frame(vec![]);
        h.typing(":w");
        h.key(Key::Enter);
        assert!(matches!(h.events.as_slice(), [EditorEvent::Save]));
        assert_eq!(h.doc.text, "a");
    }

    #[test]
    fn vim_visual_selection_and_yank_paste() {
        let mut h = Harness::new("abc", true);
        h.frame(vec![]);
        h.typing("vly$p");
        assert_eq!(h.doc.text, "abcab");
    }

    #[test]
    fn ime_commits_are_keys_in_normal_mode_and_text_in_insert_mode() {
        let mut h = Harness::new("abc", true);
        h.frame(vec![]);
        h.frame(vec![Event::Ime(egui::ImeEvent::Commit("x".into()))]);
        assert_eq!(h.doc.text, "bc");
        h.frame(vec![Event::Text("i".into())]);
        h.frame(vec![Event::Ime(egui::ImeEvent::Commit("日本".into()))]);
        assert_eq!(h.doc.text, "日本bc");
    }

    #[test]
    fn several_keys_in_one_frame_all_apply() {
        let mut h = Harness::new("abc", true);
        h.frame(vec![]);
        h.frame(vec![Event::Text("i".into()), Event::Text("X".into())]);
        assert_eq!(h.doc.text, "Xabc");
        h.frame(vec![Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }, Event::Text("x".into())]);
        assert_eq!(h.doc.text, "abc");
    }
}

#[cfg(test)]
#[path = "validate/editor_tests.rs"]
pub(crate) mod validate_tests;
#[cfg(test)]
#[path = "validate/vim_widget_tests.rs"]
mod validate_vim_tests;
