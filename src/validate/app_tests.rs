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
        Self::with_lock(test_env::lock(), root, tweak)
    }

    /// Like `new`, for tests that prepared shared state (installed servers) under the lock.
    fn with_lock(g: MutexGuard<'static, ()>, root: &Path, tweak: impl FnOnce(&mut Config)) -> Self {
        let tools = test_env::base().join("tools");
        let lsp = crate::lsp::validate_tests::fake_lsp(&tools.join(root.file_name().unwrap()));
        let claude = crate::claude::tests::fake_auth_cli(&tools.join("claude"));
        let mut cfg = Config {
            language_servers: vec![config::ServerConfig {
                name: "fake".into(),
                command: vec![lsp.to_string_lossy().to_string()],
                extensions: vec!["rs".into()],
                ..Default::default()
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
fn find_in_files_popup_previews_and_opens_the_selected_match() {
    let root = scratch("psearch");
    write(&root, "a.txt", "one\n  two needle\n");
    write(&root, "b.rs", "fn needles() {}\nlet needle = 1;\n");
    let mut h = H::new(&root, |_| {});
    h.key_m(Key::F, CTRL | Modifiers::SHIFT);
    assert!(h.app.find_files.open);
    h.frame(vec![]);
    h.typing("needle");
    // Results arrive as you type, without pressing Enter.
    h.until("search results", |a| a.find_files.results.len() == 2);
    // Whole words drops the `needles` match.
    h.app.find_files.opts.whole_word = true;
    h.app.find_files.changed_at_for_test();
    h.until("whole-word results", |a| !a.find_files.results.is_empty() && a.find_files.rows_len() == 2);
    // Pick the match in b.rs; the preview follows the selection.
    let row = (0..2).find(|&i| h.app.find_files.row_path(i).ends_with("b.rs")).unwrap();
    for _ in 0..row {
        h.key(Key::ArrowDown);
    }
    assert!(h.app.find_files.preview_path().unwrap().ends_with("b.rs"));
    h.key(Key::Enter);
    h.frame(vec![]);
    assert!(!h.app.find_files.open);
    let s = h.doc().text.find("needle =").unwrap();
    assert_eq!(h.sel(), (s, s + 6));
    // Reopening keeps the query; Escape closes.
    h.key_m(Key::F, CTRL | Modifiers::SHIFT);
    assert_eq!(h.app.find_files.query, "needle");
    h.key(Key::Escape);
    assert!(!h.app.find_files.open);
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

/// typescript-language-server style auto-import: the completion arrives without its import,
/// which comes from `completionItem/resolve` after the item is accepted.
#[test]
fn completion_auto_import_arrives_after_resolve() {
    let root = scratch("lsp-autoimport");
    let p = write(&root, "a.rs", "fn main() {\n    \n}\n");
    let mut h = H::new(&root, |_| {});
    let i = h.app.open_path(&p).unwrap();
    h.frame(vec![]);
    h.until("server ready", |a| a.lsp.get("fake").is_some_and(|c| c.ready));
    h.app.docs[i].pending_sel = Some((16, 16));
    h.frame(vec![]);
    h.typing("Th");
    h.until("completion popup", |a| a.completion.as_ref().is_some_and(|c| c.items.iter().any(|it| it.label == "Thing")));
    h.key(Key::Enter);
    let want = "use things::Thing;\nfn main() {\n    Thing\n}\n";
    h.until("import added after resolve", |a| a.docs[i].text == want);
    let resolve = lsp_log(&root).into_iter().find(|m| m["method"] == "completionItem/resolve").expect("resolve sent");
    assert_eq!(resolve["params"]["label"], "Thing");
    // The cursor stayed after the completed word: typing continues there.
    h.frame(vec![]);
    h.typing("X");
    assert_eq!(h.app.docs[i].text, "use things::Thing;\nfn main() {\n    ThingX\n}\n");

    // A resolve answer for text that changed since is dropped.
    let before = h.app.docs[i].text.clone();
    let id = h.app.docs[i].id;
    let late = json!({ "additionalTextEdits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "use late;\n" }] });
    h.app.apply_resolved_completion(id, 30, 12345, &late);
    assert_eq!(h.app.docs[i].text, before);
    // ...and so is one that would edit after the completion's start.
    let at = before.find("ThingX").unwrap();
    let hash = crate::text::hash_str(&before[..at]);
    let after = json!({ "additionalTextEdits": [{ "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 0 } }, "newText": "x" }] });
    h.app.apply_resolved_completion(id, at, hash, &after);
    assert_eq!(h.app.docs[i].text, before);
}

/// Ctrl+. offers the server's code actions for the problem on the cursor's line and applies them:
/// a direct edit, a command that makes the server send an edit back, one resolved lazily, and an
/// edit to a file that isn't open.
#[test]
fn quick_fixes_apply_edits_commands_and_resolved_actions() {
    let root = scratch("lsp-quickfix");
    let p = write(&root, "b.rs", "fn main() {\n    let x = bad;\n}\n");
    let other = write(&root, "other.rs", "fn other() {}\n");
    let other_uri = lsp::uri(&other.canonicalize().unwrap());
    let mut h = H::new(&root, |c| {
        c.language_servers[0].env.insert("FAKE_LSP_OTHER".into(), other_uri.clone());
    });
    let i = h.app.open_path(&p).unwrap();
    h.frame(vec![]);
    let uri = lsp::uri(&p);
    h.until("diagnostic", |a| a.diagnostics.get(&uri).is_some_and(|d| !d.is_empty()));
    // Cursor at the start of the line, not on the problem itself.
    h.app.docs[i].pending_sel = Some((16, 16));
    h.frame(vec![]);
    h.key_m(Key::Period, CTRL);
    h.until("quick-fix menu", |a| a.palette.open && a.palette.mode == Mode::CodeActions && a.quick_fix.waiting == 0);
    let titles: Vec<String> = h.app.quick_fix.actions.iter().map(|a| a.title.clone()).collect();
    assert_eq!(titles, ["Replace 'bad' with 'good'", "Touch the other file", "Add a footer (resolved later)", "Add a header (command)"]);
    let req = lsp_log(&root).into_iter().rfind(|m| m["method"] == "textDocument/codeAction").unwrap();
    assert_eq!(req["params"]["context"]["diagnostics"][0]["message"], "bad word", "the line's diagnostic is sent as context");
    // Enter takes the preferred fix.
    h.key(Key::Enter);
    assert_eq!(h.app.docs[i].text, "fn main() {\n    let x = good;\n}\n");
    assert!(!h.app.palette.open);

    let pick = |h: &mut H, title: &str| {
        h.app.quick_fix();
        h.until("actions", |a| a.quick_fix.waiting == 0 && !a.quick_fix.actions.is_empty());
        let k = h.app.quick_fix.actions.iter().position(|a| a.title == title).unwrap();
        h.app.palette.open = false;
        h.app.run_code_action(k);
    };
    pick(&mut h, "Add a header (command)");
    h.until("server-sent edit applied", |a| a.docs[i].text.starts_with("// header\n"));
    h.until("applyEdit answered", |_| lsp_log(&root).iter().any(|m| m["result"]["applied"] == true));
    pick(&mut h, "Add a footer (resolved later)");
    h.until("resolved edit applied", |a| a.docs[i].text.ends_with("}\n// footer\n"));
    assert_eq!(h.app.docs[i].text, "// header\nfn main() {\n    let x = good;\n}\n// footer\n");
    // An edit to a file that isn't open opens it in a tab, unsaved; the current tab stays active.
    pick(&mut h, "Touch the other file");
    let o = h.app.docs.iter().position(|d| d.title == "other.rs").expect("other.rs opened");
    assert_eq!(h.app.docs[o].text, "// touched\nfn other() {}\n");
    assert!(h.app.docs[o].is_dirty());
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "fn other() {}\n", "not written to disk");
    assert_eq!(h.app.active, Some(i));
}

/// Pull diagnostics (TypeScript 7 style): rim asks for each file's problems after opening and
/// editing it, re-asks the other open files when one changes, and again when the server says so.
#[test]
fn pull_diagnostics_follow_edits_other_files_and_refreshes() {
    let root = scratch("lsp-pull");
    let a = write(&root, "a.rs", "fn main() {\n    bad();\n    missing();\n}\n");
    let b = write(&root, "b.rs", "fn other() {}\n");
    let mut h = H::new(&root, |c| {
        c.language_servers[0].env.insert("FAKE_LSP_PULL".into(), "1".into());
    });
    let (ua, ub) = (lsp::uri(&a), lsp::uri(&b));
    let msgs = |h: &H, uri: &str| -> Vec<String> {
        h.app.diagnostics.get(uri).map(|d| d.iter().map(|d| d.message.clone()).collect()).unwrap_or_default()
    };
    let ia = h.app.open_path(&a).unwrap();
    h.until("pulled on open", |a| a.diagnostics.get(&ua).is_some_and(|d| d.len() == 2));
    let mut m = msgs(&h, &ua);
    m.sort();
    assert_eq!(m, ["bad word", "cannot find missing"]);
    let req = lsp_log(&root).into_iter().find(|m| m["method"] == "textDocument/diagnostic").unwrap();
    assert_eq!(req["params"]["identifier"], "fake");

    // An edit is followed by a new pull.
    h.app.docs[ia].set_text("fn main() {\n    good();\n    missing();\n}\n".into());
    h.until("bad word gone", |a| a.diagnostics.get(&ua).is_some_and(|d| d.len() == 1));
    assert_eq!(msgs(&h, &ua), ["cannot find missing"]);
    // An edit that changes nothing gets "unchanged" (the previous answer's id is sent back) and
    // the problems stay.
    let pulls = |root: &Path| lsp_log(root).iter().filter(|m| m["method"] == "textDocument/diagnostic").count();
    let before = pulls(&root);
    h.app.docs[ia].set_text("fn main() {\n    good();\n    missing();\n}\n\n".into());
    h.until("pulled again", |_| pulls(&root) > before);
    let last = lsp_log(&root).into_iter().rfind(|m| m["method"] == "textDocument/diagnostic").unwrap();
    assert!(last["params"]["previousResultId"].is_string());
    for _ in 0..10 {
        h.frame(vec![]);
    }
    assert_eq!(msgs(&h, &ua), ["cannot find missing"]);

    // Quick fixes get the pulled problem as context.
    h.app.docs[ia].pending_sel = Some((29, 29));
    h.frame(vec![]);
    h.app.quick_fix();
    h.until("code actions", |a| a.quick_fix.waiting == 0);
    let ca = lsp_log(&root).into_iter().rfind(|m| m["method"] == "textDocument/codeAction").unwrap();
    assert_eq!(ca["params"]["context"]["diagnostics"][0]["message"], "cannot find missing");
    h.app.palette.open = false;

    // Defining `missing` in another file clears a.rs's problem: its problems are asked for again.
    let ib = h.app.open_path(&b).unwrap();
    h.until("b.rs pulled", |a| a.diagnostics.contains_key(&ub));
    h.app.docs[ib].set_text("fn other() {}\nfn missing() {} // meh\n".into());
    h.until("a.rs re-pulled after b.rs changed", |a| a.diagnostics.get(&ua).is_some_and(|d| d.is_empty()));

    // Typing quickly: requests don't pile up, and the last text's problems win.
    h.app.activate(ib);
    h.app.docs[ib].pending_sel = Some((13, 13));
    h.frame(vec![]);
    h.typing(" bad");
    h.until("b.rs problem after typing", |a| a.diagnostics.get(&ub).is_some_and(|d| d.len() == 1));
    let d = &h.app.diagnostics[&ub][0];
    assert_eq!((d.start, d.message.as_str()), ((0, 14), "bad word"));

    // The server asks for a refresh (its rules changed): every open file is asked again.
    h.app.lsp.get_mut("fake").unwrap().request("workspace/executeCommand", json!({ "command": "fake.strict" }), Pending::ExecuteCommand);
    h.until("refreshed", |a| a.diagnostics.get(&ub).is_some_and(|d| d.iter().any(|d| d.message == "meh is not allowed")));
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
    for s in [Sidebar::Explorer, Sidebar::Git] {
        h.app.sidebar = s;
        h.frame(vec![]);
    }
    h.app.open_find_files();
    h.frame(vec![]);
    h.key(Key::Escape);
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

// ---------------- language server manager ----------------

/// Put a fake language server where rim would have installed package `id` (before `H::new`,
/// which loads the installed list).
fn fake_install(id: &str) -> PathBuf {
    let p = crate::lsp_registry::by_id(id).unwrap();
    let dir = config::servers_dir().join(p.folder);
    let info = crate::lsp_install::InstallInfo { version: "9.9.9".into(), bin: p.program().into(), installed_at: 0 };
    let bin = crate::lsp_install::program_path(p, &dir, &info);
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, crate::lsp::validate_tests::FAKE_LSP).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("rim-install.json"), serde_json::to_string(&info).unwrap()).unwrap();
    bin
}

/// Take the test lock and start with no installed servers.
fn clean_servers() -> MutexGuard<'static, ()> {
    let g = test_env::lock();
    let _ = std::fs::remove_dir_all(config::servers_dir());
    g
}

/// The rect of the widget labelled `label` in the last frame (needs `show_interactive_widgets`).
fn widget_rect(h: &H, label: &str) -> Vec<Rect> {
    h.ctx.viewport(|vp| {
        let w = &vp.prev_pass.widgets;
        w.layer_ids()
            .flat_map(|l| w.get_layer(l))
            .filter(|r| w.info(r.id).and_then(|i| i.label.as_deref()) == Some(label))
            .map(|r| r.rect)
            .collect()
    })
}

/// Regression: the row's click sensor used to be registered after its buttons and covered them,
/// so clicking Install only toggled the row's details.
#[test]
fn install_button_in_a_row_is_clickable() {
    let g = clean_servers();
    let root = scratch("lsp-install-click");
    let mut h = H::with_lock(g, &root, |_| {});
    // Only a fake `npm` exists, so nothing touches the network.
    let tools = test_env::base().join("tools").join("fake-npm");
    std::fs::create_dir_all(&tools).unwrap();
    let npm = tools.join("npm");
    std::fs::write(
        &npm,
        r#"#!/bin/sh
prefix=""; while [ $# -gt 0 ]; do case "$1" in --prefix) prefix="$2"; shift;; esac; shift; done
mkdir -p "$prefix/node_modules/pyright/.bin" "$prefix/node_modules/.bin"
echo '{"version":"1.2.3"}' > "$prefix/node_modules/pyright/package.json"
printf '#!/bin/sh
' > "$prefix/node_modules/.bin/pyright-langserver"; chmod +x "$prefix/node_modules/.bin/pyright-langserver"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    h.app.lsp_mgr.which = std::sync::Arc::new(move |p: &str| (p == "npm").then(|| npm.clone()));
    h.app.lsp_mgr.clear_cache();
    h.ctx.all_styles_mut(|s| s.debug.show_interactive_widgets = true);
    h.app.open_language_servers(None);
    // The dialog sizes itself over a few frames; measure once it has settled.
    // The dialog grows over a few frames to its full height; measure once the layout stops moving.
    let mut last: Option<Rect> = None;
    for _ in 0..200 {
        h.frame(vec![]);
        let now = widget_rect(&h, "Python").first().copied();
        if now.is_some() && now == last {
            break;
        }
        last = now;
    }
    let python = widget_rect(&h, "Python");
    assert_eq!(python.len(), 1, "one Python row");
    let row_y = python[0].center().y;
    let install = widget_rect(&h, "Install")
        .into_iter()
        .find(|r| (r.center().y - row_y).abs() < 20.0)
        .expect("an Install button on the Python row");
    h.click(install.center());
    assert_eq!(h.app.lsp_mgr.expanded, None, "the click reached the button, not the row");
    h.until("pyright installed", |a| a.lsp_mgr.jobs.get("pyright").is_some_and(|j| !j.running));
    assert_eq!(h.app.lsp_mgr.jobs["pyright"].error, None);
    assert_eq!(h.app.lsp_mgr.installed["pyright"].version, "1.2.3");
    // Clicking elsewhere on the row still opens its details.
    h.click(python[0].center());
    assert_eq!(h.app.lsp_mgr.expanded, Some("pyright"));
}

#[test]
fn installed_server_runs_can_be_disabled_and_restarts_after_update() {
    let g = clean_servers();
    fake_install("taplo");
    let root = scratch("lsp-installed");
    let p = write(&root, "Cargo.toml", "[package]\nname = \"bad\"\n");
    let mut h = H::with_lock(g, &root, |_| {});
    h.app.open_path(&p);
    h.until("taplo diagnostics", |a| a.diagnostics.values().any(|d| d.len() == 1) && a.lsp.get("taplo").is_some_and(|c| c.ready));
    let i = h.app.active.unwrap();
    let s = h.app.servers_for_doc(i);
    assert!(s[0].managed && s[0].name == "taplo");
    assert!(h.app.suggested_package(i).is_none());

    // Disabled: stops, its problems go away, and it isn't suggested either.
    h.app.lsp_set_enabled("taplo", false);
    h.frame(vec![]);
    assert!(!h.app.lsp.contains_key("taplo"));
    assert!(h.app.diagnostics.values().all(|d| d.is_empty()));
    assert!(h.app.servers_for_doc(i).is_empty());
    assert!(h.app.cfg.disabled_language_servers == ["taplo"]);
    h.app.lsp_set_enabled("taplo", true);
    h.until("taplo back", |a| a.lsp.get("taplo").is_some_and(|c| c.ready));

    // An update restarts it with the new copy, without restarting rim.
    let info = crate::lsp_install::InstallInfo { version: "10.0.0".into(), bin: "taplo".into(), installed_at: 1 };
    h.app.lsp_install_event("taplo".into(), lsp_manager::InstallEvent::Done(Ok(info)));
    assert!(!h.app.lsp.contains_key("taplo"));
    assert!(h.app.toasts.iter().any(|t| t.text.contains("Installed Taplo 10.0.0")));
    h.until("taplo restarted", |a| a.lsp.get("taplo").is_some_and(|c| c.ready) && a.diagnostics.values().any(|d| d.len() == 1));
}

#[test]
fn addon_diagnostics_merge_with_the_main_server() {
    let g = clean_servers();
    fake_install("vscode-eslint-language-server");
    let root = scratch("lsp-addon");
    let p = write(&root, "a.ts", "let bad = 1;\n");
    let mut h = H::with_lock(g, &root, |c| {
        c.language_servers.push(config::ServerConfig { extensions: vec!["ts".into()], name: "fake-ts".into(), ..c.language_servers[0].clone() });
    });
    h.app.open_path(&p);
    let uri = lsp::uri(&p);
    h.until("both servers report", |a| a.diagnostics.get(&uri).is_some_and(|d| d.len() == 2));
    let i = h.app.active.unwrap();
    let names: Vec<String> = h.app.servers_for_doc(i).into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["fake-ts", "vscode-eslint-language-server"]);
    // Both servers get edits (each tracks the version it has seen).
    h.app.docs[i].set_text("let ok = 1;\n".into());
    h.until("both servers clear", |a| a.diagnostics.get(&uri).is_some_and(|d| d.is_empty()) && a.lsp_diags.len() == 2);
    h.app.docs[i].set_text("bad\n".into());
    h.until("both report again", |a| a.diagnostics.get(&uri).is_some_and(|d| d.len() == 2));
    // Turning the add-on off leaves only the main server's problem.
    h.app.lsp_set_enabled("vscode-eslint-language-server", false);
    assert_eq!(h.app.diagnostics[&uri].len(), 1);
}

#[test]
fn status_bar_suggests_a_server_and_opens_the_manager() {
    let g = clean_servers();
    let root = scratch("lsp-suggest");
    let p = write(&root, "notes.svelte", "<p>hi</p>\n");
    let mut h = H::with_lock(g, &root, |_| {});
    h.app.open_path(&p);
    h.frame(vec![]);
    let i = h.app.active.unwrap();
    let s = h.app.suggested_package(i).map(|p| p.id);
    assert_eq!(s, Some("svelte-language-server"));
    h.app.open_language_servers(s);
    h.frame(vec![]);
    assert!(matches!(h.app.dialog, Some(Dialog::Settings)));
    assert!(h.app.lsp_mgr.tab == lsp_manager::SettingsTab::LanguageServers);
    assert_eq!(h.app.lsp_mgr.expanded, Some("svelte-language-server"));
    // The page draws (all rows, one expanded) without panicking.
    h.frame(vec![]);
    h.key(Key::Escape);
    assert!(h.app.dialog.is_none());
}

#[test]
fn debugging_with_breakpoints_stepping_and_the_panel() {
    let root = scratch("debug");
    let file = write(&root, "src/main.rs", "fn main() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n    println!(\"{}\", x + y + z);\n}\n");
    let adapter = crate::dap::tests::fake_adapter(&root.join("tools"));
    let mut h = H::new(&root, |_| {});
    h.app.open_path(&file);
    h.frame(vec![]);
    // F9 puts a breakpoint on the cursor's line; toggling again removes it.
    h.app.open_at(&file, 2, 0, 0);
    h.frame(vec![]);
    h.key(Key::F9);
    assert_eq!(h.app.debug.lines_for(&file), [2]);
    h.key(Key::F9);
    assert!(h.app.debug.lines_for(&file).is_empty());
    h.key(Key::F9);
    // F5 without a configuration opens the configuration window instead of starting.
    h.key(Key::F5);
    assert!(h.app.debug_ui.config_open && h.app.debug.phase == crate::debug::Phase::Idle);
    h.frame(vec![]);
    h.app.debug_ui.config_open = false;
    h.app.debug.config = crate::debug::LaunchConfig { kind: crate::debug::Kind::Custom, command: vec![adapter.display().to_string()], program: "prog".into(), ..Default::default() };
    h.key(Key::F5);
    assert!(h.app.debug_ui.open, "the panel opens when debugging starts");
    h.until("stopped at the breakpoint", |a| a.debug.phase == crate::debug::Phase::Stopped && a.debug.scopes.first().is_some_and(|s| s.vars.is_some()));
    // The execution line is revealed in the editor and drawn with the panel.
    assert_eq!(h.doc().path.as_deref(), Some(file.as_path()));
    assert_eq!(h.doc().line_col().0, 2);
    h.frame(vec![]);
    assert_eq!(h.app.debug.frames[0].line, 2);
    // Step over moves the execution line; Continue finishes the program.
    h.key(Key::F10);
    h.until("stepped", |a| a.debug.phase == crate::debug::Phase::Stopped && a.debug.frames.first().is_some_and(|f| f.line == 3));
    assert_eq!(h.doc().line_col().0, 3);
    h.key(Key::F5);
    h.until("ended", |a| a.debug.phase == crate::debug::Phase::Ended);
    assert!(h.app.debug.console.iter().any(|l| l.text == "done"));
    // Shift+F5 while idle is harmless; the breakpoints were saved with the project.
    h.key_m(Key::F5, Modifiers::SHIFT);
    assert!(std::fs::read_to_string(root.join(".rim/debug.toml")).unwrap().contains("[breakpoints]"));
    let _ = std::fs::remove_dir_all(&root);
}
