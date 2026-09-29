//! Application state and layout: sidebar (explorer / git), tabs, editor, status bar,
//! palette, find in files, dialogs and background tasks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Key, Layout, Margin, Modifiers,
    RichText, Sense, Stroke, TextEdit, Ui, ViewportCommand, pos2, vec2,
};
use serde_json::{Value, json};

use crate::chat::{self, Chat, ChatAction, ContextInfo, Selection};
use crate::claude;
use crate::config::{self, Config};
use crate::editor::{self, Completion, Document, EditorCtx, EditorEvent, Hover};
use crate::format;
use crate::git::{self, FileStatus, Repo};
use crate::highlight::Syntaxes;
use crate::lsp::{self, Diagnostic, LspEvent, Pending};
use crate::palette::{Item, Mode, Outcome, Palette};
use crate::search::{self, FileResult};
use crate::theme::{self, Theme};
use crate::tree::{FileTree, TreeAction};
use crate::vim;

mod claude_glue;
mod code_actions;
mod commands;
mod dialogs;
mod find_files;
mod git_glue;
mod graph_ui;
mod lsp_glue;
mod lsp_manager;
mod panels;
mod tasks;
mod util;

use code_actions::QuickFix;
use commands::Cmd;
use find_files::FindFiles;
use graph_ui::GraphState;
use lsp_manager::LspManager;
use util::*;

#[derive(PartialEq, Clone, Copy)]
enum Sidebar {
    Explorer,
    Git,
}

enum Task {
    Git {
        op: String,
        result: Result<String, String>,
    },
    Format {
        doc_id: u64,
        base_hash: u64,
        then_save: bool,
        result: Result<String, String>,
    },
    /// `git status`, plus fresh HEAD contents for open files when HEAD moved or they were requested.
    GitStatus {
        root: PathBuf,
        result: Result<git::Status, String>,
        bases: Option<Vec<(PathBuf, Option<String>)>>,
    },
    Commit {
        result: Result<String, String>,
    },
    Search {
        seq: u64,
        results: Vec<FileResult>,
        truncated: bool,
    },
    /// Progress of a language server install (by install folder).
    LspInstall {
        folder: String,
        event: lsp_manager::InstallEvent,
    },
    Files {
        root: PathBuf,
        files: Vec<String>,
    },
    /// A finished project graph scan, with the description cache read from `.rim/graph.json`.
    Graph {
        root: PathBuf,
        graph: crate::graph::ProjectGraph,
        cache: crate::graph::describe::Cache,
    },
    /// Claude's summary of one graph node: `Ok((text, model))`.
    Describe {
        root: PathBuf,
        rel: String,
        hash: u64,
        result: Result<(String, Option<String>), String>,
    },
}

enum Dialog {
    CloseDirty {
        doc_id: u64,
    },
    QuitDirty,
    Trash(PathBuf),
    Discard(FileStatus),
    Input {
        kind: InputKind,
        value: String,
        focus: bool,
    },
    Settings,
    /// The file changed on disk since it was opened or last saved.
    SaveConflict {
        doc_id: u64,
    },
}

#[derive(Clone)]
enum InputKind {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    NewBranch,
}

#[derive(PartialEq)]
enum ToastKind {
    Info,
    Error,
}

struct Toast {
    text: String,
    kind: ToastKind,
    until: Instant,
}

#[derive(Default)]
struct FindState {
    open: bool,
    query: String,
    focus: bool,
    cache: (u64, u64, String),
    matches: Vec<(usize, usize)>,
    current: Option<usize>,
}

/// (document id, text hash, selection) the cached Claude context was built from.
type ContextKey = Option<(u64, u64, (usize, usize))>;

pub struct App {
    cfg: Config,
    theme: Theme,
    syn: Syntaxes,
    root: PathBuf,
    docs: Vec<Document>,
    active: Option<usize>,
    next_id: u64,
    tree: FileTree,
    sidebar: Sidebar,
    show_sidebar: bool,
    repo: Option<Repo>,
    git: git::Status,
    git_error: Option<String>,
    commit_msg: String,
    git_busy: Option<String>,
    /// A `git status` is running; `Some(reload)` if another was requested meanwhile.
    git_status_running: bool,
    git_status_queued: Option<bool>,
    /// Workspace file list for quick open and @-mentions, refreshed in the background.
    file_list: Option<(Instant, Vec<String>)>,
    file_list_running: bool,
    file_colors: HashMap<PathBuf, Color32>,
    lsp: HashMap<String, lsp::Client>,
    lsp_failed: HashSet<String>,
    lsp_mgr: LspManager,
    /// Bumped whenever a language server starts or goes away, so documents get re-synced.
    lsp_gen: u64,
    vim_shared: vim::Shared,
    chat: Chat,
    show_claude: bool,
    ctx_cache: Option<(ContextKey, ContextInfo)>,
    /// Documents to close once their (possibly asynchronous, formatted) save lands: `:wq`, `ZZ`.
    close_after_save: HashSet<u64>,
    /// Settings changed in the dialog but not written yet.
    cfg_dirty: bool,
    /// egui still treats a just-closed modal as on top for a frame, so focus is requested twice.
    refocus_frames: u8,
    /// Diagnostics per document URI, merged from all its servers (see `lsp_diags`).
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// (server, URI) → that server's latest diagnostics.
    lsp_diags: HashMap<(String, String), Vec<Diagnostic>>,
    completion: Option<Completion>,
    hover: Option<Hover>,
    quick_fix: QuickFix,
    palette: Palette,
    find: FindState,
    find_files: FindFiles,
    /// The project graph replaces the editor area while shown.
    show_graph: bool,
    graph: GraphState,
    dialog: Option<Dialog>,
    toasts: Vec<Toast>,
    tasks_tx: Sender<Task>,
    tasks_rx: Receiver<Task>,
    last_tick: Instant,
    quit_confirmed: bool,
    focus_editor: bool,
    title: String,
    ctx: egui::Context,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, arg: Option<PathBuf>) -> Self {
        let (cfg, cfg_err) = match Config::load() {
            Ok(c) => (c, None),
            Err(e) => (
                Config {
                    read_only: true,
                    ..Config::default()
                },
                Some(format!(
                    "{e}\nSettings won't be saved until config.toml is fixed."
                )),
            ),
        };
        let theme = theme::by_name(&cfg.theme);
        theme.apply(&cc.egui_ctx);
        load_font(&cc.egui_ctx, cfg.font.as_deref());
        cc.egui_ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = vec2(6.0, 4.0);
            s.visuals.window_corner_radius = CornerRadius::same(6);
        });

        let arg = arg.map(|p| p.canonicalize().unwrap_or(p));
        let (root, file) = match &arg {
            Some(p) if p.is_file() => {
                let dir = p.parent().unwrap_or(Path::new("/")).to_path_buf();
                let root = Repo::discover(&dir).map(|r| r.root).unwrap_or(dir);
                (root, Some(p.clone()))
            }
            Some(p) if p.is_dir() => (p.clone(), None),
            _ => {
                let last = std::fs::read_to_string(config::state_file())
                    .ok()
                    .map(|s| PathBuf::from(s.trim()))
                    .map(|p| p.canonicalize().unwrap_or(p));
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let home = dirs::home_dir();
                // Launched from a desktop entry (cwd = home): reopen the last folder instead.
                match last.filter(|l| l.is_dir() && home.as_ref() == Some(&cwd)) {
                    Some(l) => (l, None),
                    None => (cwd, None),
                }
            }
        };

        let (tasks_tx, tasks_rx) = channel();
        let syn = Syntaxes::new(&theme);
        let chat = Chat::new(&cfg.claude);
        let mut app = Self {
            tree: FileTree::new(root.clone()),
            cfg,
            theme,
            syn,
            root: root.clone(),
            docs: vec![],
            active: None,
            next_id: 1,
            sidebar: Sidebar::Explorer,
            show_sidebar: true,
            repo: None,
            git: Default::default(),
            git_error: None,
            commit_msg: String::new(),
            git_busy: None,
            git_status_running: false,
            git_status_queued: None,
            file_list: None,
            file_list_running: false,
            file_colors: HashMap::new(),
            lsp: HashMap::new(),
            lsp_failed: HashSet::new(),
            lsp_mgr: LspManager::new(),
            lsp_diags: HashMap::new(),
            lsp_gen: 1,
            vim_shared: vim::Shared::default(),
            chat,
            show_claude: false,
            ctx_cache: None,
            close_after_save: HashSet::new(),
            cfg_dirty: false,
            refocus_frames: 0,
            diagnostics: HashMap::new(),
            completion: None,
            hover: None,
            quick_fix: QuickFix::default(),
            palette: Palette::default(),
            find: FindState::default(),
            find_files: FindFiles::default(),
            show_graph: false,
            graph: GraphState::default(),
            dialog: None,
            toasts: vec![],
            tasks_tx,
            tasks_rx,
            last_tick: Instant::now(),
            quit_confirmed: false,
            focus_editor: false,
            title: String::new(),
            ctx: cc.egui_ctx.clone(),
        };
        app.set_root(root);
        #[cfg(debug_assertions)]
        if std::env::var_os("RIM_CLAUDE_DEMO").is_some() {
            app.chat.load_demo();
            app.show_claude = true;
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("RIM_GRAPH_DEMO").is_some() {
            app.toggle_graph();
        }
        if let Some(f) = file {
            app.open_path(&f);
        }
        if let Some(e) = cfg_err {
            app.error(e);
        }
        if let Some(p) = arg.filter(|p| !p.exists()) {
            app.error(format!(
                "{} does not exist; opened {} instead",
                p.display(),
                app.root.display()
            ));
        }
        app
    }

    // ---------- notifications ----------

    fn info(&mut self, text: impl Into<String>) {
        self.toast(text.into(), ToastKind::Info, Duration::from_secs(4));
    }

    fn error(&mut self, text: impl Into<String>) {
        self.toast(text.into(), ToastKind::Error, Duration::from_secs(8));
    }

    /// Show a notification; a repeat of one still on screen just stays up longer.
    fn toast(&mut self, text: String, kind: ToastKind, dur: Duration) {
        let until = Instant::now() + dur;
        match self
            .toasts
            .iter_mut()
            .find(|t| t.text == text && t.kind == kind)
        {
            Some(t) => t.until = until,
            None => self.toasts.push(Toast { text, kind, until }),
        }
    }

    // ---------- workspace ----------

    fn set_root(&mut self, root: PathBuf) {
        self.lsp.clear();
        self.lsp_failed.clear();
        self.diagnostics.clear();
        self.lsp_diags.clear();
        self.lsp_mgr.clear_cache();
        self.root = root.clone();
        self.tree = FileTree::new(root.clone());
        self.repo = Repo::discover(&root);
        self.find_files = FindFiles::default();
        self.graph = GraphState::default();
        self.file_list = None;
        self.chat.reset();
        if let Some(dir) = config::state_file().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(config::state_file(), root.to_string_lossy().as_bytes());
        self.refresh_git(true);
    }

    fn open_folder_dialog(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_directory(&self.root)
            .pick_folder()
        {
            if self.docs.iter().any(|d| d.is_dirty()) {
                self.error("Save or close modified files before switching folders");
                return;
            }
            self.docs.clear();
            self.active = None;
            self.set_root(dir);
        }
    }

    // ---------- documents ----------

    fn active_doc(&mut self) -> Option<&mut Document> {
        self.active.and_then(|i| self.docs.get_mut(i))
    }

    fn doc_index(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    fn activate(&mut self, i: usize) {
        if self.active != Some(i) {
            self.completion = None;
            self.hover = None;
        }
        self.active = Some(i);
        self.focus_editor = true;
        if let Some(p) = self.docs[i].path.clone() {
            self.tree.reveal(&p);
        }
    }

    fn open_path(&mut self, path: &Path) -> Option<usize> {
        let path = path.canonicalize().unwrap_or(path.to_path_buf());
        if let Some(i) = self
            .docs
            .iter()
            .position(|d| d.path.as_ref() == Some(&path))
        {
            self.activate(i);
            return Some(i);
        }
        match editor::open_file(self.next_id, path.clone(), &self.syn) {
            Ok(mut doc) => {
                self.next_id += 1;
                doc.set_git_base(self.git_base(&path));
                doc.refresh();
                self.docs.push(doc);
                let i = self.docs.len() - 1;
                self.activate(i);
                Some(i)
            }
            Err(e) => {
                self.error(format!("{}: {e}", path.display()));
                None
            }
        }
    }

    fn open_at(&mut self, path: &Path, line: usize, col: usize, len: usize) {
        if let Some(i) = self.open_path(path) {
            let d = &mut self.docs[i];
            let l = line.min(d.index.line_count() - 1);
            let s = d.index.char_starts[l] + col;
            d.pending_sel = Some((s, s + len));
        }
    }

    fn open_virtual(&mut self, key: String, title: String, text: String, ext: &str) {
        if let Some(i) = self
            .docs
            .iter()
            .position(|d| d.virtual_key.as_ref() == Some(&key))
        {
            self.docs[i].set_text(text);
            self.activate(i);
            return;
        }
        let mut doc = Document::new(self.next_id, None, text, &self.syn);
        self.next_id += 1;
        doc.syntax = self.syn.detect(Some(Path::new(&format!("x.{ext}"))), "");
        doc.virtual_key = Some(key);
        doc.title = title;
        doc.read_only = true;
        self.docs.push(doc);
        self.activate(self.docs.len() - 1);
    }

    fn new_file(&mut self) {
        let doc = Document::new(self.next_id, None, String::new(), &self.syn);
        self.next_id += 1;
        self.docs.push(doc);
        self.activate(self.docs.len() - 1);
    }

    fn request_close(&mut self, i: usize) {
        if self.docs[i].is_dirty() {
            self.dialog = Some(Dialog::CloseDirty {
                doc_id: self.docs[i].id,
            });
        } else {
            self.close_doc(i);
        }
    }

    fn close_doc(&mut self, i: usize) {
        let doc = self.docs.remove(i);
        if let Some(p) = &doc.path {
            let u = lsp::uri(p);
            for c in self.lsp.values_mut() {
                c.did_close(&u);
            }
        }
        self.active = match self.active {
            _ if self.docs.is_empty() => None,
            Some(a) if a > i => Some(a - 1),
            Some(a) if a == i => Some(i.min(self.docs.len() - 1)),
            a => a,
        };
        self.completion = None;
        self.hover = None;
        self.focus_editor = true;
    }

    /// Save a document, optionally formatting first.
    fn save(&mut self, i: usize, allow_format: bool) {
        if self.docs[i].read_only {
            return;
        }
        if self.docs[i].path.is_none() {
            let Some(path) = rfd::FileDialog::new().set_directory(&self.root).save_file() else {
                return;
            };
            let text = std::mem::take(&mut self.docs[i].text);
            let mut doc = Document::new(self.docs[i].id, Some(path.clone()), text, &self.syn);
            doc.saved_hash = 0;
            doc.set_git_base(self.git_base(&path));
            self.docs[i] = doc;
        }
        if allow_format && self.cfg.format_on_save {
            self.format(i, true);
            return;
        }
        self.write_doc(i);
    }

    fn write_doc(&mut self, i: usize) {
        self.write_doc_checked(i, false);
    }

    /// Write a document to disk. Unless `force`, first check that nobody changed the file
    /// on disk since we loaded or saved it, so their changes aren't silently lost.
    fn write_doc_checked(&mut self, i: usize, force: bool) {
        let doc = &self.docs[i];
        let Some(path) = doc.path.clone() else { return };
        let disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if !force && doc.mtime.is_some() && disk_mtime.is_some() && disk_mtime != doc.mtime {
            self.dialog = Some(Dialog::SaveConflict { doc_id: doc.id });
            return;
        }
        if let Err(e) = atomic_write(&path, doc.disk_text().as_bytes()) {
            self.close_after_save.remove(&doc.id);
            self.error(format!("Save failed: {e}"));
            return;
        }
        let doc = &mut self.docs[i];
        doc.saved_hash = doc.hash;
        doc.mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let (u, text) = (lsp::uri(&path), doc.text.clone());
        for s in self.servers_for_doc(i) {
            if let Some(c) = self.lsp.get_mut(&s.name) {
                c.did_save(&u, &text);
            }
        }
        if path == Config::path() {
            self.reload_config();
        }
        self.tree.invalidate();
        self.refresh_git(false);
        if self.close_after_save.remove(&self.docs[i].id) {
            self.close_doc(i);
        }
    }

    fn reload_from_disk(&mut self, i: usize) {
        let d = &mut self.docs[i];
        let Some(p) = d.path.clone() else { return };
        match std::fs::read_to_string(&p) {
            Ok(text) => {
                let c = d.cursor_char;
                d.set_text(text);
                d.saved_hash = d.hash;
                d.mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                let c = c.min(d.index.total_chars);
                d.pending_sel = Some((c, c));
            }
            Err(e) => self.error(format!("Reload failed: {e}")),
        }
    }

    fn reload_config(&mut self) {
        match Config::load() {
            Ok(c) => {
                let vim_changed = c.vim_mode != self.cfg.vim_mode;
                self.cfg = c;
                self.lsp_mgr.clear_cache();
                self.lsp_gen += 1;
                self.chat.refresh_binary(self.cfg.claude.path.as_deref());
                if vim_changed {
                    self.reset_vim();
                }
                self.set_theme(&self.cfg.theme.clone(), false);
                load_font(&self.ctx, self.cfg.font.as_deref());
                self.info("Settings reloaded");
            }
            Err(e) => {
                self.cfg.read_only = true;
                self.error(format!(
                    "{e}\nSettings won't be saved until config.toml is fixed."
                ));
            }
        }
    }

    fn save_cfg(&mut self) {
        self.cfg_dirty = false;
        if let Err(e) = self.cfg.save() {
            self.error(e);
        }
    }

    fn reset_vim(&mut self) {
        for d in &mut self.docs {
            d.vim = vim::Vim::default();
        }
        self.focus_editor = true;
    }

    fn set_vim(&mut self, on: bool) {
        self.cfg.vim_mode = on;
        self.save_cfg();
        self.reset_vim();
        self.info(format!("Vim motions: {}", if on { "on" } else { "off" }));
    }

    fn set_theme(&mut self, name: &str, persist: bool) {
        self.theme = theme::by_name(name);
        self.theme.apply(&self.ctx);
        self.syn.set_theme(&self.theme);
        if persist {
            self.cfg.theme = self.theme.name.to_string();
            self.save_cfg();
        }
    }
}

#[cfg(debug_assertions)]
impl App {
    /// `RIM_SHOT=<file.ppm>`: save the window 6 s after start (used to check the UI headlessly).
    fn debug_screenshot(&mut self) {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        static SENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let Some(path) = std::env::var_os("RIM_SHOT") else { return };
        let start = *START.get_or_init(Instant::now);
        self.ctx.request_repaint_after(Duration::from_millis(100));
        if start.elapsed() > Duration::from_secs(6) && !SENT.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = self.ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let [w, h] = img.size;
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in &img.pixels {
                out.extend_from_slice(&[px.r(), px.g(), px.b()]);
            }
            let _ = std::fs::write(&path, out);
            self.ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        #[cfg(debug_assertions)]
        self.debug_screenshot();
        self.poll_tasks();
        self.poll_lsp();
        self.poll_claude();
        self.periodic();
        if self.refocus_frames > 0 && self.dialog.is_none() {
            self.refocus_frames -= 1;
            self.focus_editor = true;
            self.ctx.request_repaint();
        }

        if self.ctx.input(|i| i.viewport().close_requested())
            && !self.quit_confirmed
            && self.docs.iter().any(|d| d.is_dirty())
        {
            self.ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::QuitDirty);
        }
        if self.dialog.is_none() {
            self.shortcuts();
        }

        let t = self.theme.clone();
        egui::Panel::bottom("status")
            .exact_size(24.0)
            .frame(
                Frame::NONE
                    .fill(t.bg_dark)
                    .inner_margin(Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.status_bar(ui));
        if self.show_sidebar {
            egui::Panel::left("sidebar")
                .default_size(260.0)
                .size_range(160.0..=600.0)
                .frame(
                    Frame::NONE
                        .fill(t.bg_dark)
                        .inner_margin(Margin::symmetric(8, 4)),
                )
                .show(ui, |ui| self.sidebar_ui(ui));
        }
        if self.show_claude {
            self.cached_context();
            let context = &self.ctx_cache.as_ref().unwrap().1;
            let mut actions = Vec::new();
            egui::Panel::right("claude")
                .default_size(430.0)
                .size_range(300.0..=900.0)
                .frame(
                    Frame::NONE
                        .fill(t.bg_dark)
                        .inner_margin(Margin::symmetric(10, 6)),
                )
                .show(ui, |ui| {
                    let env = chat::Env {
                        theme: &self.theme,
                        syn: &self.syn,
                        root: &self.root,
                        context,
                    };
                    actions = self.chat.ui(ui, &env);
                });
            for a in actions {
                match a {
                    ChatAction::OpenFile(p, Some(line)) => {
                        self.open_at(&p, line.saturating_sub(1), 0, 0)
                    }
                    ChatAction::OpenFile(p, None) => {
                        self.open_path(&p);
                    }
                    ChatAction::Mention => self.open_mention_picker(),
                    ChatAction::FocusEditor => self.focus_editor = true,
                    ChatAction::SaveSettings(s) => {
                        self.cfg.claude.set_session_settings(&s);
                        self.save_cfg();
                    }
                    ChatAction::Close => {
                        self.show_claude = false;
                        self.focus_editor = true;
                    }
                }
            }
        }
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(t.bg))
            .show(ui, |ui| self.central(ui));

        self.palette_ui();
        self.find_files_ui();
        self.dialogs();
        self.toasts_ui();
        self.update_title();
    }
}

#[cfg(test)]
#[path = "validate/app_tests.rs"]
mod validate_tests;
