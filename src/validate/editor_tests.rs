//! Validation tests for the editor widget (src/editor.rs), driven headlessly through
//! `egui::Context::run_ui` with synthetic input. Included from editor.rs as a child
//! module so private helpers (`apply_completion`, `find_matches`, ...) are reachable.

use super::*;
use crate::lsp::parse_completions;
use egui::{Event, RawInput};
use serde_json::json;
use std::time::Instant;

pub const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: true };

pub struct H {
    pub ctx: egui::Context,
    pub doc: Document,
    pub syn: Syntaxes,
    pub theme: Theme,
    pub shared: vim::Shared,
    pub vim_on: bool,
    pub completion: Option<Completion>,
    pub hover: Option<Hover>,
    pub events: Vec<EditorEvent>,
    pub diagnostics: Vec<Diagnostic>,
    pub matches: Vec<(usize, usize)>,
    pub triggers: Vec<String>,
    first: bool,
    time: f64,
    /// Seconds between frames (large by default so egui's undoer sees separate edits).
    pub dt: f64,
}

impl H {
    pub fn new(text: &str, vim_on: bool) -> Self {
        Self::with_path(text, None, vim_on)
    }

    pub fn with_path(text: &str, path: Option<&str>, vim_on: bool) -> Self {
        let theme = crate::theme::by_name("tokyonight");
        let syn = Syntaxes::new(&theme);
        let doc = Document::new(1, path.map(PathBuf::from), text.to_string(), &syn);
        let mut h = Self {
            ctx: egui::Context::default(),
            doc,
            syn,
            theme,
            shared: Default::default(),
            vim_on,
            completion: None,
            hover: None,
            events: vec![],
            diagnostics: vec![],
            matches: vec![],
            triggers: vec![],
            first: true,
            time: 0.0,
            dt: 2.0,
        };
        h.frame(vec![]);
        h
    }

    pub fn frame(&mut self, events: Vec<Event>) {
        let mods = events
            .iter()
            .find_map(|e| if let Event::Key { modifiers, .. } = e { Some(*modifiers) } else { None })
            .unwrap_or_default();
        // Advance the clock so egui's undoer sees distinct edits.
        self.time += self.dt;
        let mut events = events;
        events.insert(0, Event::ModifiersChanged(mods));
        let mut input = RawInput { events, time: Some(self.time), ..Default::default() };
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 700.0)));
        let request_focus = std::mem::take(&mut self.first);
        let mut out = self.ctx.run_ui(input, |ui| {
            let mut cx = EditorCtx {
                theme: &self.theme,
                syn: &self.syn,
                font: FontId::monospace(14.0),
                tab_size: 4,
                insert_spaces: true,
                diagnostics: &self.diagnostics,
                completion: &mut self.completion,
                hover: &mut self.hover,
                matches: &self.matches,
                current_match: None,
                request_focus,
                triggers: &self.triggers,
                vim: if self.vim_on { Some(&mut self.shared) } else { None },
            };
            self.events.extend(show(ui, &mut self.doc, &mut cx));
        });
        out.textures_delta.clear();
    }

    pub fn typing(&mut self, s: &str) {
        for c in s.chars() {
            self.frame(vec![Event::Text(c.to_string())]);
        }
    }

    pub fn key_m(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
    }

    pub fn key(&mut self, key: Key) {
        self.key_m(key, Modifiers::NONE);
    }

    pub fn select(&mut self, a: usize, b: usize) {
        self.doc.pending_sel = Some((a, b));
        self.frame(vec![]);
    }

    pub fn sel(&self) -> (usize, usize) {
        let st = TextEdit::load_state(&self.ctx, self.doc.editor_id()).unwrap();
        let r = st.cursor.char_range().unwrap();
        let [a, b] = r.sorted_cursors();
        (a.index.0, b.index.0)
    }

    /// Text with `|` at the cursor (or `[`..`]` around a selection).
    pub fn show(&self) -> String {
        let (a, b) = self.sel();
        let mut chars: Vec<char> = self.doc.text.chars().collect();
        if a == b {
            chars.insert(a, '|');
        } else {
            chars.insert(b, ']');
            chars.insert(a, '[');
        }
        chars.into_iter().collect()
    }
}

/// Build a harness from text with `|` marking the cursor.
fn at(text: &str, path: Option<&str>) -> H {
    let c = text.chars().position(|c| c == '|').unwrap();
    let t: String = text.chars().filter(|c| *c != '|').collect();
    let mut h = H::with_path(&t, path, false);
    h.select(c, c);
    h
}

// ---------------- typing / unicode ----------------

#[test]
fn typing_unicode_and_emoji() {
    let mut h = H::new("", false);
    h.typing("héllo 😀 世界");
    assert_eq!(h.doc.text, "héllo 😀 世界");
    assert_eq!(h.sel(), (10, 10));
    // Backspace removes whole chars (the CJK char, then the space+emoji...).
    h.key(Key::Backspace);
    h.key(Key::Backspace);
    h.key(Key::Backspace);
    assert_eq!(h.doc.text, "héllo 😀");
    h.key(Key::Backspace);
    assert_eq!(h.doc.text, "héllo ");
    // Line index / LSP positions stay consistent with the text after edits.
    assert_eq!(h.doc.index.total_chars, h.doc.text.chars().count());
}

#[test]
fn typing_in_the_middle_updates_the_index() {
    let mut h = at("ab|cd\nxyz", None);
    h.typing("é😀");
    assert_eq!(h.show(), "abé😀|cd\nxyz");
    assert_eq!(h.doc.index.line_count(), 2);
    assert_eq!(h.doc.line_col(), (0, 4));
}

// ---------------- undo ----------------

#[test]
fn ctrl_z_undoes_typing() {
    let mut h = H::new("", false);
    h.typing("hello");
    h.key_m(Key::Z, CTRL);
    assert_ne!(h.doc.text, "hello", "Ctrl+Z did nothing");
    // Redo brings it back.
    h.key_m(Key::Z, CTRL | Modifiers::SHIFT);
    assert_eq!(h.doc.text, "hello");
}

#[test]
fn ctrl_z_undoes_auto_indent_enter() {
    let mut h = H::new("", false);
    h.typing("fn x() {");
    h.frame(vec![]); // a pause (egui repaints for the cursor blink), so "{" becomes an undo point
    h.key(Key::Enter);
    let after_enter = h.doc.text.clone();
    assert_eq!(after_enter, "fn x() {\n    ");
    h.key_m(Key::Z, CTRL);
    assert_eq!(h.doc.text, "fn x() {", "undo after Enter should restore the text before Enter");
}

#[test]
fn ctrl_z_undoes_tab_indent_and_comment_toggle() {
    let mut h = H::with_path("a\nb", Some("/nonexistent/x.rs"), false);
    h.select(0, 3);
    h.key(Key::Tab);
    assert_eq!(h.doc.text, "    a\n    b");
    h.key_m(Key::Z, CTRL);
    assert_eq!(h.doc.text, "a\nb", "undo of Tab-indent of a selection");
    h.select(0, 0);
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.doc.text, "// a\nb");
    h.key_m(Key::Z, CTRL);
    assert_eq!(h.doc.text, "a\nb", "undo of Ctrl+/");
}

// ---------------- Enter auto-indent ----------------

#[test]
fn enter_keeps_indentation() {
    let mut h = at("    foo|", None);
    h.key(Key::Enter);
    assert_eq!(h.show(), "    foo\n    |");
    let mut h = at("\tfoo|", None);
    h.key(Key::Enter);
    assert_eq!(h.show(), "\tfoo\n\t|");
}

#[test]
fn enter_between_braces_opens_a_block() {
    for (open, close) in [('{', '}'), ('(', ')'), ('[', ']')] {
        let mut h = at(&format!("  f() {open}|{close}"), None);
        h.key(Key::Enter);
        assert_eq!(h.show(), format!("  f() {open}\n      |\n  {close}"));
    }
}

#[test]
fn enter_after_open_brace_indents() {
    let mut h = at("if x {|\nrest", None);
    h.key(Key::Enter);
    assert_eq!(h.show(), "if x {\n    |\nrest");
}

#[test]
fn enter_after_colon_in_python() {
    let mut h = at("def f():|", Some("/nonexistent/a.py"));
    h.key(Key::Enter);
    assert_eq!(h.show(), "def f():\n    |");
    // Not in Rust.
    let mut h = at("a:|", Some("/nonexistent/a.rs"));
    h.key(Key::Enter);
    assert_eq!(h.show(), "a:\n|");
}

#[test]
fn enter_replaces_a_selection() {
    let mut h = H::new("  abcdef", false);
    h.select(4, 6);
    h.key(Key::Enter);
    assert_eq!(h.show(), "  ab\n  |ef");
}

#[test]
fn enter_in_the_middle_of_indentation() {
    // Cursor inside the leading whitespace: indent is only what's before the cursor.
    let mut h = at("  |  foo", None);
    h.key(Key::Enter);
    assert_eq!(h.show(), "  \n  |  foo");
}

// ---------------- Tab / Shift+Tab ----------------

#[test]
fn tab_inserts_spaces_to_next_stop() {
    let mut h = at("ab|c", None);
    h.key(Key::Tab);
    assert_eq!(h.show(), "ab  |c");
    h.key(Key::Tab);
    assert_eq!(h.show(), "ab      |c");
}

#[test]
fn tab_and_shift_tab_on_a_multi_line_selection() {
    let mut h = H::new("a\n\n  b\nc", false);
    h.select(0, 7); // a .. b (not c)
    h.key(Key::Tab);
    assert_eq!(h.doc.text, "    a\n\n      b\nc", "empty lines stay empty, c untouched");
    let (s, e) = h.sel();
    assert_eq!((s, e), (0, 14), "selection covers the indented lines");
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.doc.text, "a\n\n  b\nc");
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.doc.text, "a\n\nb\nc");
}

#[test]
fn selection_ending_at_line_start_excludes_that_line() {
    let mut h = H::new("a\nb\nc", false);
    h.select(0, 4); // up to the start of "c"
    h.key(Key::Tab);
    assert_eq!(h.doc.text, "    a\n    b\nc");
}

#[test]
fn shift_tab_single_cursor() {
    let mut h = at("      fo|o", None);
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.show(), "  fo|o");
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.show(), "fo|o");
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.show(), "fo|o");
    let mut h = at("\t\tx|", None);
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.show(), "\tx|");
    // Cursor inside the indentation.
    let mut h = at("  |  x", None);
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.show(), "|x");
}

#[test]
fn shift_tab_on_unicode_line_does_not_panic() {
    // dedent slices by byte count of leading spaces; make sure mixed content is fine.
    let mut h = at(" é|x", None);
    h.key_m(Key::Tab, Modifiers::SHIFT);
    assert_eq!(h.show(), "é|x");
}

// ---------------- Ctrl+/ ----------------

#[test]
fn toggle_comment_single_line_rust() {
    let mut h = at("    let x = 1;|", Some("/nonexistent/a.rs"));
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.show(), "    // let x = 1;|");
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.show(), "    let x = 1;|");
}

#[test]
fn toggle_comment_multi_line_uses_min_indent_and_skips_blank() {
    let mut h = H::with_path("fn a() {\n    x();\n\n  y();\n}\n", Some("/nonexistent/a.rs"), false);
    h.select(9, 25); // lines 1..=3
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.doc.text, "fn a() {\n  //   x();\n\n  // y();\n}\n");
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.doc.text, "fn a() {\n    x();\n\n  y();\n}\n");
}

#[test]
fn toggle_comment_python_hash_and_mixed() {
    let mut h = H::with_path("# a\nb\n", Some("/nonexistent/a.py"), false);
    h.select(0, 5);
    h.key_m(Key::Slash, CTRL);
    // Not all commented -> comment all.
    assert_eq!(h.doc.text, "# # a\n# b\n");
}

#[test]
fn toggle_comment_unknown_language_is_a_noop() {
    let mut h = at("hello|", Some("/nonexistent/a.unknownext"));
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.doc.text, "hello");
}

#[test]
fn toggle_comment_cursor_position_follows() {
    let mut h = at("a|b", Some("/nonexistent/a.lua"));
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.show(), "-- a|b");
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.show(), "a|b");
}

#[test]
fn toggle_comment_with_unicode_indent_does_not_panic() {
    // min_indent is a byte count; with a non-ASCII whitespace char (U+3000) it could split a char.
    let mut h = H::with_path("\u{3000}a\n b\n", Some("/nonexistent/a.rs"), false);
    h.select(0, 5);
    h.key_m(Key::Slash, CTRL);
    assert!(h.doc.text.contains("//"));
}

// ---------------- find ----------------

#[test]
fn find_matches_basic_and_unicode_offsets() {
    assert_eq!(find_matches("Foo foo FOO", "foo"), vec![(0, 3), (4, 7), (8, 11)]);
    assert_eq!(find_matches("😀ab😀AB", "ab"), vec![(1, 3), (4, 6)]);
    assert_eq!(find_matches("aaa", "aa"), vec![(0, 2)]); // non-overlapping
    assert_eq!(find_matches("x", ""), vec![]);
    assert_eq!(find_matches("ÉCOLE école", "école"), vec![(0, 5), (6, 11)]);
}

#[test]
fn find_matches_when_lowercase_changes_length() {
    // 'İ' (2 bytes) lowercases to 3 bytes; the code falls back to exact matching for the whole doc,
    // so case-insensitive search silently stops working for all other text.
    let m = find_matches("İstanbul FOO", "foo");
    assert_eq!(m, vec![(9, 12)], "case-insensitive find should still match FOO");
}

#[test]
fn find_matches_are_highlighted_without_panic() {
    let mut h = H::new("abc abc\nabc", false);
    h.matches = find_matches(&h.doc.text, "abc");
    h.frame(vec![]);
    assert_eq!(h.matches.len(), 3);
}

// ---------------- go to line ----------------

#[test]
fn goto_line_clamps() {
    let mut h = H::new("a\nb\nc", false);
    h.doc.goto_line(1);
    h.frame(vec![]);
    assert_eq!(h.sel(), (2, 2));
    h.doc.goto_line(99);
    h.frame(vec![]);
    assert_eq!(h.sel(), (4, 4));
}

// ---------------- read-only ----------------

#[test]
fn read_only_documents_ignore_edits() {
    let mut h = H::new("diff\n+a\n-b", false);
    h.doc.read_only = true;
    h.frame(vec![]);
    h.typing("xyz");
    h.key(Key::Enter);
    h.key(Key::Tab);
    h.key(Key::Backspace);
    h.key_m(Key::Slash, CTRL);
    h.frame(vec![Event::Paste("pasted".into())]);
    assert_eq!(h.doc.text, "diff\n+a\n-b");
    assert!(!h.doc.is_dirty());
    // But selection / copy works.
    h.select(0, 4);
    h.key_m(Key::C, CTRL);
    assert_eq!(h.sel(), (0, 4));
}

#[test]
fn read_only_vim_edits_are_blocked() {
    let mut h = H::new("abc", true);
    h.doc.read_only = true;
    h.typing("xddiQ");
    assert_eq!(h.doc.text, "abc");
    assert_eq!(h.doc.vim.mode, vim::Mode::Normal);
}

// ---------------- empty / CRLF ----------------

#[test]
fn empty_document_every_key() {
    let mut h = H::with_path("", Some("/nonexistent/e.rs"), false);
    for k in [Key::Enter, Key::Tab, Key::Backspace, Key::Delete, Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End] {
        h.doc.set_text(String::new());
        h.select(0, 0);
        h.key(k);
    }
    h.doc.set_text(String::new());
    h.select(0, 0);
    h.key_m(Key::Tab, Modifiers::SHIFT);
    h.key_m(Key::Slash, CTRL);
    h.key_m(Key::Z, CTRL);
    h.key_m(Key::A, CTRL);
    assert_eq!(h.doc.text, "");
}

#[test]
fn empty_document_vim_every_key() {
    let mut h = H::new("", true);
    h.typing("xXdddwDCccyypPJr~uGgg$0^wbe%*#nN{}>><<v");
    h.key(Key::Escape);
    h.typing("Vd");
    h.key_m(Key::R, CTRL);
    h.key_m(Key::D, CTRL);
    h.key_m(Key::U, CTRL);
    assert_eq!(h.doc.text, "");
}

#[test]
fn crlf_enter_keeps_line_endings_consistent() {
    let mut h = at("a {|\r\n}\r\n", Some("/nonexistent/a.rs"));
    h.key(Key::Enter);
    // A CRLF file should stay CRLF; mixing in a bare "\n" corrupts line endings on save.
    assert!(!h.doc.text.replace("\r\n", "").contains('\n'), "mixed line endings: {:?}", h.doc.text);
}

#[test]
fn crlf_editing_and_comment_toggle() {
    let mut h = H::with_path("a\r\nb\r\n", Some("/nonexistent/a.rs"), false);
    assert_eq!(h.doc.index.line_count(), 3);
    h.select(0, 4);
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.doc.text, "// a\r\n// b\r\n");
    h.key_m(Key::Slash, CTRL);
    assert_eq!(h.doc.text, "a\r\nb\r\n");
    h.select(0, 4);
    h.key(Key::Tab);
    assert_eq!(h.doc.text, "    a\r\n    b\r\n");
}

#[test]
fn crlf_vim_dollar_lands_on_last_real_char() {
    let mut h = H::new("abc\r\nxyz\r\n", true);
    h.typing("$");
    assert_eq!(h.doc.cursor_char, 2, "$ should stop on 'c', not on the '\\r'");
    h.typing("x");
    assert_eq!(h.doc.text, "ab\r\nxyz\r\n");
}

// ---------------- large files ----------------

fn big_rust(lines: usize) -> String {
    let mut s = String::new();
    for i in 0..lines {
        match i % 10 {
            0 => s += &format!("/// Doc for item {i}\n"),
            1 => s += &format!("pub fn item_{i}(x: u32) -> u32 {{\n"),
            2 => s += "    let s = \"a string with { braces }\";\n",
            3 => s += "    /* block comment */ let y = x * 2;\n",
            4 => s += "    if y > 10 { return y; }\n",
            5 => s += "    // line comment 😀 unicode\n",
            6 => s += "    let v: Vec<u8> = vec![1, 2, 3];\n",
            7 => s += "    y + v.len() as u32\n",
            8 => s += "}\n",
            _ => s += "\n",
        }
    }
    s
}

#[test]
fn large_file_20k_lines_stays_usable() {
    let text = big_rust(20_000);
    let t0 = Instant::now();
    let mut h = H::with_path(&text, Some("/nonexistent/big.rs"), false);
    let first = t0.elapsed();
    // Jump to the middle and type.
    h.doc.goto_line(10_000);
    h.frame(vec![]);
    let t1 = Instant::now();
    h.typing("abc");
    h.key(Key::Enter);
    let per_key = t1.elapsed() / 4;
    // Jump to the end and back.
    h.doc.goto_line(19_999);
    h.frame(vec![]);
    h.doc.goto_line(0);
    h.frame(vec![]);
    let t2 = Instant::now();
    h.typing("/*");
    let open_comment = t2.elapsed() / 2;
    eprintln!("20k lines: first frame {first:?}, per keystroke mid-file {per_key:?}, per keystroke opening a block comment at top {open_comment:?}");
    assert!(h.doc.text.contains("abc\n"));
    // Debug build: generous budget. Report actual numbers in the log.
    assert!(per_key.as_millis() < 2000, "typing too slow: {per_key:?}");
}

#[test]
fn huge_file_over_2mb_is_plain_and_does_not_panic() {
    let text = big_rust(120_000); // > 2 MB
    assert!(text.len() > 2 * 1024 * 1024);
    let mut h = H::with_path(&text, Some("/nonexistent/huge.rs"), false);
    h.doc.goto_line(40_000);
    h.frame(vec![]);
    h.typing("x");
    assert!(h.doc.is_dirty());
}

#[test]
fn very_long_single_line() {
    let text = "x".repeat(200_000);
    let mut h = H::with_path(&text, Some("/nonexistent/long.rs"), false);
    h.select(100_000, 100_000);
    h.typing("y");
    assert_eq!(h.doc.text.len(), 200_001);
}

// ---------------- diagnostics / hover / definition plumbing ----------------

#[test]
fn diagnostics_render_and_hover_collects_them() {
    let mut h = H::with_path("fn main() {\n    let 😀x = 1;\n}\n", Some("/nonexistent/a.rs"), false);
    h.diagnostics = vec![
        Diagnostic { start: (1, 8), end: (1, 11), severity: 1, message: "bad".into(), source: "rustc".into() },
        // Multi-line and out-of-range diagnostics must not panic.
        Diagnostic { start: (0, 0), end: (2, 1), severity: 2, message: "w".into(), source: String::new() },
        Diagnostic { start: (50, 3), end: (60, 0), severity: 3, message: "far".into(), source: String::new() },
    ];
    h.frame(vec![]);
    // UTF-16 col 8 on line 1 is after "    let " -> char 20; the emoji is 2 UTF-16 units.
    let c = h.doc.index.lsp_to_char(&h.doc.text, 1, 8);
    assert_eq!(c, 20);
    h.select(c, c);
    h.key_m(Key::K, CTRL);
    let hov = h.hover.as_ref().expect("Ctrl+K opens a hover");
    assert!(hov.diags.iter().any(|(s, m)| *s == 1 && m == "[rustc] bad"), "{:?}", hov.diags);
    assert!(h.events.iter().any(|e| matches!(e, EditorEvent::Hover(ci) if *ci == c)));
    // A hover response shows text.
    h.hover.as_mut().unwrap().text = Some("fn main()".into());
    h.frame(vec![]);
}

#[test]
fn f12_and_ctrl_space_emit_events() {
    let mut h = at("foo.ba|r", Some("/nonexistent/a.rs"));
    h.key(Key::F12);
    assert!(h.events.iter().any(|e| matches!(e, EditorEvent::Definition(6))));
    h.key_m(Key::Space, CTRL);
    assert!(h.events.iter().any(|e| matches!(e, EditorEvent::Complete { anchor: 4 })));
}

#[test]
fn typing_word_chars_and_trigger_chars_requests_completion() {
    let mut h = H::with_path("", Some("/nonexistent/a.rs"), false);
    h.triggers = vec![".".into(), "::".into()];
    h.typing("a");
    assert!(h.events.iter().any(|e| matches!(e, EditorEvent::Complete { anchor: 0 })));
    h.events.clear();
    h.typing(" ");
    assert!(h.events.is_empty());
    h.typing(".");
    assert!(h.events.iter().any(|e| matches!(e, EditorEvent::Complete { .. })));
    h.events.clear();
    h.typing(":");
    assert!(h.events.iter().any(|e| matches!(e, EditorEvent::Complete { .. })), "':' ends the '::' trigger");
}

// ---------------- completion popup & insert ----------------

fn completion_items(v: serde_json::Value) -> Vec<CompletionItem> {
    parse_completions(&v).0
}

#[test]
fn completion_popup_filter_navigate_and_accept_with_enter() {
    let mut h = at("let v = fo|", Some("/nonexistent/a.rs"));
    let items = completion_items(json!([
        { "label": "format!", "insertText": "format!($0)", "insertTextFormat": 2, "kind": 3 },
        { "label": "foo", "kind": 6 },
        { "label": "bar", "kind": 6 },
    ]));
    let mut c = Completion::new(h.doc.id, items, 8, false);
    c.refilter("fo");
    assert_eq!(c.filtered.len(), 2);
    h.completion = Some(c);
    h.frame(vec![]);
    // Sorted by sortText (label): foo, format!
    h.key(Key::ArrowDown);
    assert_eq!(h.completion.as_ref().unwrap().selected, 1);
    h.key(Key::ArrowDown);
    assert_eq!(h.completion.as_ref().unwrap().selected, 0, "wraps around");
    h.key(Key::ArrowUp);
    assert_eq!(h.completion.as_ref().unwrap().selected, 1);
    h.key(Key::Enter);
    assert!(h.completion.is_none());
    assert_eq!(h.show(), "let v = format!(|)", "snippet $0 places cursor inside parens");
}

#[test]
fn completion_typing_narrows_and_escape_closes() {
    let mut h = at("x|", None);
    let items = completion_items(json!([{ "label": "xavier" }, { "label": "xylophone" }]));
    let mut c = Completion::new(h.doc.id, items, 0, false);
    c.refilter("x");
    h.completion = Some(c);
    h.frame(vec![]);
    h.typing("y");
    let c = h.completion.as_ref().unwrap();
    assert_eq!(c.filtered.len(), 1);
    assert_eq!(c.items[c.filtered[0]].label, "xylophone");
    h.typing("q");
    assert!(h.completion.is_none(), "no match closes the popup");
    let items = completion_items(json!([{ "label": "xyq1" }]));
    let mut c = Completion::new(h.doc.id, items, 0, false);
    c.refilter("xyq");
    h.completion = Some(c);
    h.frame(vec![]);
    h.key(Key::Escape);
    assert!(h.completion.is_none());
}

#[test]
fn completion_tab_accepts() {
    let mut h = at("pri|", None);
    let items = completion_items(json!([{ "label": "println!", "insertText": "println!(\"${1:fmt}\", ${2:args})", "insertTextFormat": 2 }]));
    let mut c = Completion::new(h.doc.id, items, 0, false);
    c.refilter("pri");
    h.completion = Some(c);
    h.frame(vec![]);
    h.key(Key::Tab);
    assert_eq!(h.show(), "println!(\"[fmt]\", args)", "first placeholder selected");
}

#[test]
fn apply_completion_text_edit_range_and_additional_edits() {
    let theme = crate::theme::by_name("tokyonight");
    let syn = Syntaxes::new(&theme);
    // textEdit replacing "st" (from col 8) in `let a = st|`, plus an import added at the top.
    let mut doc = Document::new(1, None, "use x;\nlet a = st\n".into(), &syn);
    let items = completion_items(json!([{
        "label": "String",
        "textEdit": { "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 10 } }, "newText": "String::new()" },
        "additionalTextEdits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "use std::string::String;\n" }]
    }]));
    let cursor = doc.index.lsp_to_char(&doc.text, 1, 10);
    let sel = apply_completion(&mut doc, &items[0], cursor);
    assert_eq!(doc.text, "use std::string::String;\nuse x;\nlet a = String::new()\n");
    let expected = doc.text.find("()").unwrap() + 2;
    assert_eq!(sel, (expected, expected));
}

#[test]
fn apply_completion_text_edit_replacing_past_the_cursor() {
    // InsertReplaceEdit / textEdit whose range extends past the cursor (e.g. completing in the
    // middle of an identifier `fo|obar`). LSP says the whole range is replaced.
    let theme = crate::theme::by_name("tokyonight");
    let syn = Syntaxes::new(&theme);
    let mut doc = Document::new(1, None, "fooobar".into(), &syn);
    let items = completion_items(json!([{
        "label": "foobaz",
        "textEdit": { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } }, "newText": "foobaz" }
    }]));
    apply_completion(&mut doc, &items[0], 2);
    assert_eq!(doc.text, "foobaz", "textEdit range end is ignored; text after the cursor is left behind");
}

#[test]
fn apply_completion_with_emoji_before() {
    let theme = crate::theme::by_name("tokyonight");
    let syn = Syntaxes::new(&theme);
    let mut doc = Document::new(1, None, "😀 ab".into(), &syn);
    // UTF-16: emoji = 2 units, so "ab" starts at character 3.
    let items = completion_items(json!([{
        "label": "abc",
        "textEdit": { "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 5 } }, "newText": "abc" }
    }]));
    apply_completion(&mut doc, &items[0], 4);
    assert_eq!(doc.text, "😀 abc");
}

#[test]
fn completion_click_accepts() {
    let mut h = at("x|", None);
    let items = completion_items(json!([{ "label": "xavier" }]));
    let mut c = Completion::new(h.doc.id, items, 0, false);
    c.refilter("x");
    h.completion = Some(c);
    h.frame(vec![]);
    h.dt = 0.05;
    // Find the popup area rect and click in it.
    let rect = h.ctx.memory(|m| m.area_rect(Id::new("rim-completion"))).expect("popup shown");
    let p = rect.left_top() + vec2(60.0, 14.0);
    h.frame(vec![Event::PointerMoved(p)]);
    h.frame(vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
    h.frame(vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
    assert_eq!(h.doc.text, "xavier");
}
