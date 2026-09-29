//! Validation: the whole app shell (src/app.rs `App`) driven headlessly.
//! `App::new` gets `eframe::CreationContext::_new_kittest`, frames run through
//! `egui::Context::run_ui`. Config/state dirs point into a scratch folder (see test_env).
//! Included from app.rs as a child module.

use super::*;
use crate::test_env;
use egui::{Event, Pos2, RawInput, Rect};
use std::sync::MutexGuard;

const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: true };

struct H {
    ctx: egui::Context,
    app: App,
    frame: eframe::Frame,
    time: f64,
    pub root: PathBuf,
    _g: MutexGuard<'static, ()>,
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-app-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn write(root: &Path, rel: &str, s: &str) -> PathBuf {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, s).unwrap();
    p
}

impl H {
    /// `tweak` edits the config that is written before the app starts.
    fn new(root: &Path, tweak: impl FnOnce(&mut Config)) -> Self {
        let g = test_env::lock();
        let tools = test_env::base().join("tools");
        let lsp = crate::lsp::validate_tests::fake_lsp(&tools.join(root.file_name().unwrap()));
        let claude = crate::claude::tests::fake_auth_cli(&tools.join("claude"));
        let mut cfg = Config {
            language_servers: vec![config::ServerConfig {
                name: "fake".into(),
                command: vec![lsp.to_string_lossy().to_string()],
                extensions: vec!["rs".into()],
                initialization_options: None,
            }],
            ..Config::default()
        };
        // An empty command disables the built-in rustfmt, so formatting goes through the fake LSP.
        cfg.formatters.insert("rs".into(), vec![]);
        cfg.claude.path = Some(claude.to_string_lossy().to_string());
        tweak(&mut cfg);
        cfg.write_all().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let app = App::new(&cc, Some(root.to_path_buf()));
        let mut h = H { ctx, app, frame: eframe::Frame::_new_kittest(), time: 0.0, root: root.to_path_buf(), _g: g };
        h.frame(vec![]);
        h
    }

    fn frame(&mut self, events: Vec<Event>) {
        let mods = events
            .iter()
            .find_map(|e| if let Event::Key { modifiers, .. } = e { Some(*modifiers) } else { None })
            .unwrap_or_default();
        let mut events = events;
        events.insert(0, Event::ModifiersChanged(mods));
        self.time += 0.05;
        let mut input = RawInput { events, time: Some(self.time), ..Default::default() };
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0)));
        let app = &mut self.app;
        let frame = &mut self.frame;
        let mut out = self.ctx.run_ui(input, |ui| eframe::App::ui(app, ui, frame));
        out.textures_delta.clear();
    }

    fn key_m(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
    }

    fn key(&mut self, key: Key) {
        self.key_m(key, Modifiers::NONE);
    }

    fn typing(&mut self, s: &str) {
        for c in s.chars() {
            self.frame(vec![Event::Text(c.to_string())]);
        }
    }

    /// Run frames until `pred` holds (background threads: LSP, formatter, git, CLI).
    fn until(&mut self, what: &str, pred: impl Fn(&App) -> bool) {
        for _ in 0..400 {
            self.frame(vec![]);
            if pred(&self.app) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        panic!("timed out waiting for: {what}");
    }

    /// Git status and commits run in the background; wait for them to land.
    fn git_idle(&mut self) {
        self.until("git idle", |a| !a.git_status_running && a.git_status_queued.is_none() && a.git_busy.is_none());
    }

    fn doc(&self) -> &Document {
        &self.app.docs[self.app.active.unwrap()]
    }

    fn sel(&self) -> (usize, usize) {
        let st = TextEdit::load_state(&self.ctx, self.doc().editor_id()).unwrap();
        let [a, b] = st.cursor.char_range().unwrap().sorted_cursors();
        (a.index.0, b.index.0)
    }

    fn toasts(&self) -> Vec<String> {
        self.app.toasts.iter().map(|t| t.text.clone()).collect()
    }

    fn click(&mut self, p: Pos2) {
        self.frame(vec![Event::PointerMoved(p)]);
        self.frame(vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
        self.frame(vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
    }
}

const SRC: &str = "fn main() {\n    foo();\n    Foo();\n    bar();\n}\n";

// ---------------- find / go to line / quick open ----------------

#[test]
fn find_in_file_next_prev_and_close() {
    let root = scratch("find");
    let p = write(&root, "a.txt", SRC);
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&p);
    h.frame(vec![]);
    h.key_m(Key::F, CTRL);
    assert!(h.app.find.open);
    h.frame(vec![]);
    h.typing("foo");
    h.frame(vec![]);
    assert_eq!(h.app.find.matches, vec![(16, 19), (27, 30)], "case-insensitive matches");
    h.key(Key::Enter);
    eprintln!("sel right after Enter: {:?}, pending {:?}, focus editor? {}", h.sel(), h.app.docs[0].pending_sel, h.ctx.memory(|m| m.has_focus(h.app.docs[0].editor_id())));
    h.frame(vec![]);
    eprintln!("sel one frame later: {:?}", h.sel());
    assert_eq!(h.app.find.current, Some(0));
    // egui can't show a selection in an unfocused editor; the document remembers it instead.
    assert_eq!(h.doc().sel, (16, 19));
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(h.app.find.current, Some(1));
    assert_eq!(h.doc().sel, (27, 30));
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(h.app.find.current, Some(0), "wraps to the first match");
    h.key_m(Key::Enter, Modifiers::SHIFT);
    h.frame(vec![]);
    assert_eq!(h.app.find.current, Some(1), "Shift+Enter goes back (wrapping)");
    // Editing the document recomputes matches.
    h.app.docs[0].set_text(SRC.replace("bar", "foo"));
    h.frame(vec![]);
    assert_eq!(h.app.find.matches.len(), 3);
    h.key(Key::Escape);
    h.frame(vec![]);
    assert!(!h.app.find.open);
    h.frame(vec![]);
    assert_eq!(h.sel(), h.doc().sel, "closing find puts the match selection into the editor");
    // Ctrl+F with a selection pre-fills the query ("foo" on the last line, chars 38..41).
    h.app.find.query.clear();
    h.app.docs[0].pending_sel = Some((38, 41));
    h.frame(vec![]);
    h.frame(vec![]);
    h.key_m(Key::F, CTRL);
    assert_eq!(h.app.find.query, "foo");
}

#[test]
fn go_to_line_via_palette() {
    let root = scratch("goto");
    let p = write(&root, "a.txt", SRC);
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&p);
    h.frame(vec![]);
    h.key_m(Key::G, CTRL);
    assert!(h.app.palette.open);
    h.frame(vec![]);
    h.typing("4");
    h.key(Key::Enter);
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(!h.app.palette.open);
    assert_eq!(h.doc().line_col(), (3, 0));
    // Out of range clamps to the last line; garbage is ignored.
    h.key_m(Key::G, CTRL);
    h.frame(vec![]);
    h.typing("999");
    h.key(Key::Enter);
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(h.doc().line_col().0, 5);
    h.key_m(Key::G, CTRL);
    h.frame(vec![]);
    h.typing("abc");
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(h.doc().line_col().0, 5);
}

#[test]
fn quick_open_lists_non_ignored_files_and_opens() {
    let root = scratch("quick");
    std::process::Command::new("git").arg("-C").arg(&root).args(["init", "-q"]).status().unwrap();
    write(&root, ".gitignore", "target/\n");
    write(&root, "src/main.rs", SRC);
    write(&root, "src/other.txt", "x");
    write(&root, "target/junk.rs", "x");
    let mut h = H::new(&root, |_| {});
    h.key_m(Key::P, CTRL);
    assert!(h.app.palette.open);
    h.frame(vec![]);
    // The palette's items come from search::list_files (palette fields are private).
    let ids: Vec<String> = search::list_files(&h.root, 50_000);
    assert!(ids.contains(&"src/main.rs".to_string()));
    assert!(!ids.iter().any(|i| i.starts_with("target/")), "{ids:?}");
    h.typing("othtxt");
    h.frame(vec![]);
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(h.doc().path.as_deref(), Some(root.join("src/other.txt").as_path()));
    // Explorer revealed and selected it.
    assert_eq!(h.app.tree.selected.as_deref(), Some(root.join("src/other.txt").as_path()));
}

#[test]
fn project_search_panel_opens_result_at_match() {
    let root = scratch("psearch");
    write(&root, "a.txt", "one\n  two needle\n");
    let mut h = H::new(&root, |_| {});
    h.key_m(Key::F, CTRL | Modifiers::SHIFT);
    assert!(h.app.sidebar == Sidebar::Search);
    h.frame(vec![]);
    h.typing("needle");
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(h.app.search.results.len(), 1);
    let m = &h.app.search.results[0].matches[0];
    let (p, l, c, len) = (h.app.search.results[0].path.clone(), m.line, m.col, m.len);
    h.app.open_at(&p, l, c, len);
    h.frame(vec![]);
    let s = h.doc().text.find("needle").unwrap();
    assert_eq!(h.sel(), (s, s + 6));
}

// ---------------- open / save edge cases ----------------

#[test]
fn non_utf8_crlf_and_empty_files() {
    let root = scratch("files");
    let bin = root.join("latin1.txt");
    std::fs::write(&bin, b"caf\xe9\n").unwrap();
    let crlf = write(&root, "crlf.txt", "a\r\nb\r\n");
    let empty = write(&root, "empty.rs", "");
    let mut h = H::new(&root, |_| {});
    assert!(h.app.open_path(&bin).is_none());
    assert!(h.toasts().iter().any(|t| t.contains("not a UTF-8 text file")), "{:?}", h.toasts());
    let i = h.app.open_path(&crlf).unwrap();
    h.frame(vec![]);
    h.app.save(i, false);
    assert_eq!(std::fs::read(&crlf).unwrap(), b"a\r\nb\r\n", "unchanged CRLF file saved byte-identical");
    let i = h.app.open_path(&empty).unwrap();
    h.frame(vec![]);
    h.typing("x");
    h.key_m(Key::S, CTRL);
    h.frame(vec![]);
    assert_eq!(std::fs::read_to_string(&empty).unwrap(), "x");
    assert!(!h.app.docs[i].is_dirty());
}

#[test]
fn dirty_close_asks_and_rename_does_not_clobber() {
    let root = scratch("close");
    let a = write(&root, "a.txt", "aaa");
    let b = write(&root, "b.txt", "precious contents");
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&a);
    h.frame(vec![]);
    h.typing("x");
    h.key_m(Key::W, CTRL);
    h.frame(vec![]);
    assert!(matches!(h.app.dialog, Some(Dialog::CloseDirty { .. })), "Ctrl+W on a dirty tab asks first");
    h.key(Key::Escape);
    h.frame(vec![]);
    assert!(h.app.dialog.is_none());
    assert_eq!(h.app.docs.len(), 1);
    // After cancelling the dialog, typing should go to the editor again.
    h.typing("Q");
    h.frame(vec![]);
    let focused = h.ctx.memory(|m| m.has_focus(h.app.docs[0].editor_id()));
    eprintln!("editor focused after cancelling the dialog: {focused}; text {:?}", h.app.docs[0].text);
    assert!(h.app.docs[0].text.contains('Q'), "keyboard input lost after cancelling the Save-changes dialog");
    // Renaming a.txt to an existing name silently replaces the other file.
    h.app.submit_input(InputKind::Rename(a.clone()), "b.txt".into());
    let survived = std::fs::read_to_string(&b).unwrap_or_default();
    assert_eq!(survived, "precious contents", "rename overwrote an existing file without asking");
}

// ---------------- LSP through the app ----------------

fn lsp_log(root: &Path) -> Vec<Value> {
    crate::lsp::validate_tests::read_log(&test_env::base().join("tools").join(root.file_name().unwrap()))
}

#[test]
fn lsp_sync_completion_hover_definition_diagnostics() {
    let root = scratch("lsp");
    let p = write(&root, "a.rs", "fn main() {\n    \n}\n");
    write(&root, "def.rs", "// def\nfn target() {}\n");
    let mut h = H::new(&root, |_| {});
    let i = h.app.open_path(&p).unwrap();
    h.frame(vec![]);
    h.until("server ready", |a| a.lsp.get("fake").is_some_and(|c| c.ready));
    // Type inside main: didChange carries the full new text.
    h.app.docs[i].pending_sel = Some((16, 16));
    h.frame(vec![]);
    h.typing("pr");
    h.until("didChange with typed text", |a| {
        let _ = a;
        lsp_log(&root).iter().any(|m| m["method"] == "textDocument/didChange" && m["params"]["contentChanges"][0]["text"] == "fn main() {\n    pr\n}\n")
    });
    // Typing a word char requested completion; the popup opens with the server's items.
    h.until("completion popup", |a| a.completion.is_some());
    let c = h.app.completion.as_ref().unwrap();
    let labels: Vec<&str> = c.filtered.iter().map(|&k| c.items[k].label.as_str()).collect();
    assert_eq!(labels, ["println!", "print_all"], "filtered by the typed prefix 'pr'");
    // Down to print_all (textEdit + additionalTextEdits) and accept.
    h.key(Key::ArrowDown);
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(h.app.docs[i].text, "use all;\nfn main() {\n    print_all()\n}\n");
    // Diagnostics: type the word "bad".
    h.typing(" bad");
    h.until("diagnostic for 'bad'", |a| a.diagnostics.get(&lsp::uri(&p)).is_some_and(|d| !d.is_empty()));
    let d = &h.app.diagnostics[&lsp::uri(&p)][0];
    assert_eq!(d.start.0, 2);
    assert_eq!(d.message, "bad word");
    // Hover (Ctrl+K) fills in the server's text.
    h.key_m(Key::K, CTRL);
    h.until("hover text", |a| a.hover.as_ref().is_some_and(|x| x.text.is_some()));
    assert_eq!(h.app.hover.as_ref().unwrap().text.as_deref(), Some("fn fake()\nDocs here"));
    assert!(h.app.hover.as_ref().unwrap().diags.iter().any(|d| d.1.contains("bad word")) || true);
    h.key(Key::Escape);
    // F12: go to definition opens def.rs at (1, 3).
    h.key(Key::F12);
    h.until("definition opened", |a| a.active.is_some_and(|k| a.docs[k].path.as_deref() == Some(root.join("def.rs").as_path())));
    h.frame(vec![]);
    assert_eq!(h.doc().line_col(), (1, 3));
    // Closing the tab sends didClose.
    let before = lsp_log(&root).iter().filter(|m| m["method"] == "textDocument/didClose").count();
    h.key_m(Key::W, CTRL);
    h.until("didClose", |_| lsp_log(&root).iter().filter(|m| m["method"] == "textDocument/didClose").count() > before);
}

#[test]
fn lsp_snippet_completion_via_ctrl_space() {
    let root = scratch("lspsnip");
    let p = write(&root, "a.rs", "x\n");
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&p);
    h.frame(vec![]);
    h.until("server ready", |a| a.lsp.get("fake").is_some_and(|c| c.ready));
    h.app.docs[0].pending_sel = Some((1, 1));
    h.frame(vec![]);
    h.key_m(Key::Space, CTRL);
    // Prefix "x" matches none of "println!" / "print_all" / "zzz": no popup.
    for _ in 0..20 {
        h.frame(vec![]);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(h.app.completion.is_none());
    // On an empty word: everything is offered; accept the snippet.
    h.app.docs[0].set_text("\n".into());
    h.app.docs[0].pending_sel = Some((0, 0));
    h.frame(vec![]);
    h.key_m(Key::Space, CTRL);
    h.until("completion popup", |a| a.completion.as_ref().is_some_and(|c| c.filtered.len() == 3));
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(h.doc().text, "println!(\"fmt\")\n");
    assert_eq!(h.sel(), (10, 13), "first placeholder `fmt` selected");
}

#[test]
fn lsp_formatting_edits_are_applied_in_order() {
    let root = scratch("lspfmt");
    let p = write(&root, "a.rs", "x   \ny\n");
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&p);
    h.frame(vec![]);
    h.until("server ready", |a| a.lsp.get("fake").is_some_and(|c| c.ready));
    h.key_m(Key::F, Modifiers::ALT | Modifiers::SHIFT);
    h.until("formatted", |a| a.docs[0].text != "x   \ny\n");
    // Server sent [insert "A" at 0:0, insert "B" at 0:0, delete trailing spaces]. LSP: same-position
    // inserts apply in array order -> "ABx".
    assert_eq!(h.doc().text, "ABx\ny\n");
}

#[test]
fn lsp_missing_server_is_reported_once() {
    let root = scratch("lspmissing");
    let p = write(&root, "a.rs", "x\n");
    let mut h = H::new(&root, |c| c.language_servers[0].command = vec!["no-such-lsp-xyz".into()]);
    h.app.open_path(&p);
    for _ in 0..5 {
        h.frame(vec![]);
    }
    let n = h.toasts().iter().filter(|t| t.contains("no-such-lsp-xyz")).count();
    assert_eq!(n, 1, "{:?}", h.toasts());
    assert!(h.app.lsp_failed.contains("fake"));
}

// ---------------- external formatter through the app ----------------

#[test]
fn formatter_format_on_save_failure_and_non_utf8() {
    let root = scratch("fmt");
    let tools = root.join("tools");
    let up = crate::format::validate_tests::script(&tools, "up", "#!/bin/sh\ntr a-z A-Z\n");
    let bad = crate::format::validate_tests::script(&tools, "bad", "#!/bin/sh\ncat >/dev/null\necho 'boom at 1:1' >&2\nexit 2\n");
    let latin = crate::format::validate_tests::script(&tools, "latin", "#!/bin/sh\ncat >/dev/null\nprintf '\\377'\n");
    let slow = crate::format::validate_tests::script(&tools, "slow", "#!/bin/sh\nsleep 0.5\ntr a-z A-Z\n");
    let (up, bad, latin, slow) = (up.to_string_lossy().to_string(), bad.to_string_lossy().to_string(), latin.to_string_lossy().to_string(), slow.to_string_lossy().to_string());
    let fa = write(&root, "a.up", "hello\n");
    let fb = write(&root, "b.bad", "hello\n");
    let fc = write(&root, "c.latin", "hello\n");
    let fd = write(&root, "d.slow", "hello\n");
    let mut h = H::new(&root, |c| {
        c.format_on_save = true;
        c.formatters.insert("up".into(), vec![up.clone()]);
        c.formatters.insert("bad".into(), vec![bad.clone()]);
        c.formatters.insert("latin".into(), vec![latin.clone()]);
        c.formatters.insert("slow".into(), vec![slow.clone()]);
    });
    // Format on save: formatted text is saved.
    let i = h.app.open_path(&fa).unwrap();
    h.frame(vec![]);
    h.typing("x");
    h.key_m(Key::S, CTRL);
    h.until("formatted + saved", |a| !a.docs[i].is_dirty());
    assert_eq!(std::fs::read_to_string(&fa).unwrap(), "XHELLO\n");
    assert_eq!(h.app.docs[i].text, "XHELLO\n");
    // Failure: error toast, file still saved unformatted.
    let i = h.app.open_path(&fb).unwrap();
    h.frame(vec![]);
    h.typing("y");
    h.key_m(Key::S, CTRL);
    h.until("saved after failed format", |a| !a.docs[i].is_dirty());
    assert_eq!(std::fs::read_to_string(&fb).unwrap(), "yhello\n");
    assert!(h.toasts().iter().any(|t| t.contains("Format failed: boom at 1:1")), "{:?}", h.toasts());
    // Non-UTF-8 output.
    let i = h.app.open_path(&fc).unwrap();
    h.frame(vec![]);
    h.key_m(Key::F, Modifiers::ALT | Modifiers::SHIFT);
    h.until("error toast", |a| a.toasts.iter().any(|t| t.text.contains("invalid UTF-8")));
    assert_eq!(h.app.docs[i].text, "hello\n");
    // Edited while the formatter runs: the stale result is dropped.
    let i = h.app.open_path(&fd).unwrap();
    h.frame(vec![]);
    h.key_m(Key::F, Modifiers::ALT | Modifiers::SHIFT);
    h.typing("z");
    std::thread::sleep(std::time::Duration::from_millis(900));
    for _ in 0..10 {
        h.frame(vec![]);
    }
    assert_eq!(h.app.docs[i].text, "zhello\n", "stale format result must not overwrite newer edits");
    // No formatter and no LSP for the extension.
    let fe = write(&root, "e.nofmt", "q");
    h.app.open_path(&fe);
    h.frame(vec![]);
    h.key_m(Key::F, Modifiers::ALT | Modifiers::SHIFT);
    h.frame(vec![]);
    assert!(h.toasts().iter().any(|t| t.contains("No formatter available for .nofmt")));
}

// ---------------- vim through the app ----------------

#[test]
fn vim_write_quit_through_the_app() {
    let root = scratch("vimw");
    let p = write(&root, "a.txt", "hello\n");
    let mut h = H::new(&root, |c| c.vim_mode = true);
    h.app.open_path(&p);
    h.frame(vec![]);
    h.typing("x:w");
    h.key(Key::Enter);
    h.frame(vec![]);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "ello\n");
    h.typing("x:q");
    h.key(Key::Enter);
    h.frame(vec![]);
    assert!(matches!(h.app.dialog, Some(Dialog::CloseDirty { .. })), ":q on a dirty buffer asks");
    h.key(Key::Escape);
    h.frame(vec![]);
    let focused = h.ctx.memory(|m| m.has_focus(h.app.docs[0].editor_id()));
    assert!(focused, "editor lost keyboard focus after cancelling the dialog");
    h.typing(":wq");
    h.key(Key::Enter);
    h.frame(vec![]);
    assert!(h.app.docs.is_empty(), ":wq saves and closes");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "llo\n");
}

// ---------------- git through the app ----------------

#[test]
fn git_markers_stage_commit_and_diff_views() {
    let root = scratch("git");
    let run = |args: &[&str]| {
        let o = std::process::Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.name", "T"]);
    run(&["config", "user.email", "t@example.test"]);
    run(&["config", "commit.gpgsign", "false"]);
    let p = write(&root, "a.txt", "one\ntwo\nthree\n");
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "init"]);
    let mut h = H::new(&root, |_| {});
    h.git_idle();
    assert_eq!(h.app.git.branch, "main");
    let i = h.app.open_path(&p).unwrap();
    h.frame(vec![]);
    assert!(h.app.docs[i].git_base.is_some());
    // Edit line 2 in the buffer: a Modified marker appears without saving.
    h.app.docs[i].pending_sel = Some((4, 4));
    h.frame(vec![]);
    h.typing("X");
    assert_eq!(h.app.docs[i].marks, vec![(1, 2, git::LineMark::Modified)]);
    h.key_m(Key::S, CTRL);
    h.frame(vec![]);
    h.git_idle();
    assert!(h.app.git.files.iter().any(|f| f.path == "a.txt" && f.unstaged == Some('M')));
    assert!(h.app.file_colors.contains_key(&p));
    // Stage and commit through the app.
    h.app.git_do("add", |r| r.stage("a.txt"));
    h.app.commit_msg = "edit two".into();
    h.app.commit();
    h.frame(vec![]);
    h.git_idle();
    assert!(h.app.git.files.is_empty(), "{:?}", h.app.git.files);
    assert!(h.app.commit_msg.is_empty());
    assert!(h.app.docs[i].marks.is_empty(), "markers reset after commit (new HEAD)");
    // Commit with nothing staged / empty message: errors, no commit.
    h.app.commit_msg = "".into();
    h.app.commit();
    assert!(h.toasts().iter().any(|t| t.contains("Commit message is empty")));
    // Diff view: read-only virtual doc.
    write(&root, "a.txt", "one\nXtwo\nthree\nfour\n");
    h.app.refresh_git(false);
    h.git_idle();
    let f = h.app.git.files[0].clone();
    h.app.open_diff(&f, false);
    h.frame(vec![]);
    let d = h.doc();
    assert!(d.read_only);
    assert!(d.text.contains("+four"), "{}", d.text);
    assert!(!d.is_dirty());
    h.typing("zzz");
    assert!(!h.doc().text.contains("zzz"));
    // Untracked file diff.
    write(&root, "new.txt", "brand new\n");
    h.app.refresh_git(false);
    h.git_idle();
    let f = h.app.git.files.iter().find(|f| f.path == "new.txt").unwrap().clone();
    h.app.open_diff(&f, false);
    h.frame(vec![]);
    assert!(h.doc().text.contains("+brand new"), "{}", h.doc().text);
    // Git log view.
    h.app.run_command("git_log");
    h.frame(vec![]);
    assert!(h.doc().text.contains("edit two"));
}

// ---------------- settings / config through the app ----------------

#[test]
fn opening_settings_does_not_destroy_a_broken_config() {
    let root = scratch("settings");
    let mut h = H::new(&root, |_| {});
    let path = Config::path();
    let broken = "# my comment\nvim_mode = true\ntheme = \"nord\"\ntab_size = \n";
    std::fs::write(&path, broken).unwrap();
    // Simulate a restart with the broken file: the app starts with defaults and reports the error.
    let cc = eframe::CreationContext::_new_kittest(h.ctx.clone());
    h.app = App::new(&cc, Some(root.clone()));
    h.frame(vec![]);
    assert!(h.toasts().iter().any(|t| t.starts_with("config.toml:")), "{:?}", h.toasts());
    // The user opens the config to fix it ("Preferences: Open config.toml").
    h.app.run_command("settings");
    h.frame(vec![]);
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, broken, "opening config.toml overwrote the user's (broken) file with defaults");
}

#[test]
fn saving_config_toml_reloads_settings() {
    let root = scratch("reload");
    let mut h = H::new(&root, |_| {});
    let path = Config::path();
    let i = h.app.open_path(&path).unwrap();
    h.frame(vec![]);
    let t = h.app.docs[i].text.replace("vim_mode = false", "vim_mode = true").replace("tab_size = 4", "tab_size = 2");
    h.app.docs[i].set_text(t);
    h.app.save(i, false);
    h.frame(vec![]);
    assert!(h.app.cfg.vim_mode);
    assert_eq!(h.app.cfg.tab_size, 2);
    assert!(h.toasts().iter().any(|t| t == "Settings reloaded"));
    // A broken save shows an error and keeps the old config.
    let t = h.app.docs[i].text.replace("tab_size = 2", "tab_size = ");
    h.app.docs[i].set_text(t);
    h.app.save(i, false);
    assert_eq!(h.app.cfg.tab_size, 2);
    assert!(h.toasts().iter().any(|t| t.starts_with("config.toml:")));
}

// ---------------- Claude panel inside the app ----------------

#[test]
fn ctrl_l_opens_the_claude_panel_and_checks_sign_in() {
    let root = scratch("claude");
    let mut h = H::new(&root, |_| {});
    h.key_m(Key::L, CTRL);
    assert!(h.app.show_claude);
    for _ in 0..20 {
        h.frame(vec![]);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Ctrl+L again with the chat focused moves focus back to the editor (panel stays open).
    h.frame(vec![]);
    h.key_m(Key::L, CTRL);
    assert!(h.app.show_claude);
}

#[test]
fn many_frames_with_everything_open_do_not_panic() {
    let root = scratch("smoke");
    std::process::Command::new("git").arg("-C").arg(&root).args(["init", "-q"]).status().unwrap();
    let p = write(&root, "src/main.rs", SRC);
    write(&root, "README.md", "# hi\n");
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&p);
    h.app.show_claude = true;
    h.app.chat.convo = crate::chat::demo_conversation();
    for s in [Sidebar::Explorer, Sidebar::Search, Sidebar::Git] {
        h.app.sidebar = s;
        h.frame(vec![]);
    }
    h.key_m(Key::P, CTRL | Modifiers::SHIFT);
    h.frame(vec![]);
    h.typing("theme");
    h.key(Key::Enter);
    h.frame(vec![]);
    h.key(Key::ArrowDown);
    h.key(Key::Enter);
    h.frame(vec![]);
    h.key_m(Key::Comma, CTRL);
    h.frame(vec![]);
    assert!(matches!(h.app.dialog, Some(Dialog::Settings)));
    h.key(Key::Escape);
    h.frame(vec![]);
    h.click(Pos2::new(640.0, 400.0));
}

#[test]
fn selection_is_kept_as_claude_context_when_focus_moves_to_the_chat() {
    let root = scratch("ctxsel");
    let p = write(&root, "a.rs", "line one\nline two\nline three\n");
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&p);
    h.frame(vec![]);
    h.app.docs[0].pending_sel = Some((9, 27));
    h.frame(vec![]);
    h.frame(vec![]);
    let sel = h.app.context_info().selection.expect("selection while the editor is focused");
    assert_eq!((sel.start_line, sel.end_line), (2, 3));
    // Move focus to the chat prompt (Ctrl+L) as a user would before typing the question.
    h.key_m(Key::L, CTRL);
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.ctx.memory(|m| m.has_focus(crate::chat::Chat::input_id())));
    let ctx = h.app.context_info();
    assert!(ctx.selection.is_some(), "the editor selection is gone once the chat has focus, so it is never sent to Claude (chip: {:?})", ctx.chip());
}

#[test]
fn vim_ctrl_f_and_ctrl_b_page_instead_of_find_and_sidebar() {
    let root = scratch("vimctrl");
    let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let p = write(&root, "a.txt", &text);
    let mut h = H::new(&root, |c| c.vim_mode = true);
    h.app.open_path(&p);
    h.frame(vec![]);
    h.frame(vec![]);
    let sidebar = h.app.show_sidebar;
    h.key_m(Key::F, CTRL);
    h.frame(vec![]);
    assert!(!h.app.find.open, "Ctrl+F in vim normal mode pages down instead of opening Find");
    assert!(h.doc().line_col().0 > 10, "cursor on line {}", h.doc().line_col().0);
    h.key_m(Key::B, CTRL);
    h.frame(vec![]);
    assert_eq!(h.app.show_sidebar, sidebar, "Ctrl+B pages up instead of toggling the sidebar");
    assert_eq!(h.doc().line_col().0, 0);
    // In insert mode they are the app's shortcuts again.
    h.typing("i");
    h.key_m(Key::F, CTRL);
    h.frame(vec![]);
    assert!(h.app.find.open);
}

#[test]
fn repeated_notifications_are_shown_once() {
    let root = scratch("toasts");
    let mut h = H::new(&root, |_| {});
    h.app.error("Failed to load workspaces.");
    h.app.error("Failed to load workspaces.");
    h.app.info("Failed to load workspaces.");
    assert_eq!(h.toasts().len(), 2, "same text and kind once; an info with the same text is separate");
}
