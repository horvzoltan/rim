//! The Claude panel: conversation view, tool cards with approvals and inline
//! diffs, and the prompt box. State that talks to the CLI lives in `Chat`;
//! rendering is split into free functions that only emit `UiEvent`s so the
//! conversation can be borrowed immutably while drawing.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, channel};
use std::path::{Path, PathBuf};
use std::time::Instant;

use egui::text::LayoutJob;
use egui::{
    Align, Color32, CornerRadius, EventFilter, Frame, Id, Key, Label, Layout, Margin, Modifiers, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder,
};
use serde_json::{Value, json};
use similar::ChangeTag;

use crate::claude::{self, AccountInfo, AuthStatus, Conversation, Effect, ImageData, Item, Login, ModelInfo, Options, PermState, Reply, Session, SessionInfo, SessionSettings, SlashCommand, Usage, UserImage};
use crate::config::ClaudeConfig;
use crate::highlight::Syntaxes;
use crate::theme::Theme;

pub const MODES: [(&str, &str); 3] = [("Ask before edits", "manual"), ("Edit automatically", "acceptEdits"), ("Plan mode", "plan")];
/// Shown until the CLI reports its own catalog (which includes every model the account can use).
mod attach;
mod menus;
mod render;

pub use attach::{Attachment, clipboard_image};
use attach::is_image_file;
#[cfg(any(test, debug_assertions))]
pub use render::demo_conversation;
use render::{empty_state, items_ui};
#[cfg(test)]
use render::file_ref;

pub const MODELS: [(&str, Option<&str>); 4] = [("Default model", None), ("Opus", Some("opus")), ("Sonnet", Some("sonnet")), ("Haiku", Some("haiku"))];

const NOT_FOUND: &str = "Claude Code was not found. Install it with the button below (or `npm install -g @anthropic-ai/claude-code`), then use Sign in. You can also set `claude.path` in config.toml.";

#[derive(Default, Clone)]
pub struct Selection {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// What the editor currently shows; sent along with prompts so "this file" and "this code" just work.
#[derive(Default, Clone)]
pub struct ContextInfo {
    pub path: Option<PathBuf>,
    pub rel: String,
    pub selection: Option<Selection>,
}

impl ContextInfo {
    fn note(&self) -> Option<String> {
        let path = self.path.as_ref()?;
        let mut s = format!("<ide_opened_file>The user opened the file {} in the IDE. This may or may not be related to the current task.</ide_opened_file>", path.display());
        if let Some(sel) = &self.selection {
            let text: String = sel.text.chars().take(20_000).collect();
            s += &format!(
                "\n<ide_selection>The user selected the lines {} to {} from {}:\n{}\n\nThis may or may not be related to the current task.</ide_selection>",
                sel.start_line,
                sel.end_line,
                path.display(),
                text
            );
        }
        Some(s)
    }

    pub fn chip(&self) -> Option<String> {
        self.path.as_ref()?;
        Some(match &self.selection {
            Some(s) if s.start_line == s.end_line => format!("{} line {}", self.rel, s.start_line),
            Some(s) => format!("{} lines {}-{}", self.rel, s.start_line, s.end_line),
            None => self.rel.clone(),
        })
    }
}

pub enum ChatAction {
    OpenFile(PathBuf, Option<usize>),
    /// Effort, thinking, fast mode, output style… changed: persist them.
    SaveSettings(SessionSettings),
    /// The user typed `@`: offer a file picker.
    Mention,
    /// Escape in the prompt box with nothing to stop.
    FocusEditor,
    Close,
}

pub struct Env<'a> {
    pub theme: &'a Theme,
    pub syn: &'a Syntaxes,
    pub root: &'a Path,
    pub context: &'a ContextInfo,
}

enum UiEvent {
    Allow { request_id: String, input: Value, update: Option<Value> },
    Deny { request_id: String, message: String, interrupt: bool },
    Open(PathBuf, Option<usize>),
    Url(String),
}

#[derive(Default)]
struct AskState {
    picked: Vec<Vec<bool>>,
    other: Vec<String>,
}

#[derive(Default)]
struct View {
    /// Content signature, to repaint once more after layout so stick-to-bottom settles.
    sig: (usize, usize, bool),
    settle: u8,
    code_cache: HashMap<u64, LayoutJob>,
    /// Computed diff lines per tool card, so the diff isn't recomputed every frame.
    diff_cache: HashMap<u64, DiffLines>,
    asks: HashMap<String, AskState>,
}

#[derive(Clone)]
struct DiffLines {
    /// (group index, tag, line) for the lines shown.
    lines: Vec<(usize, ChangeTag, String)>,
    truncated: bool,
}

/// Who is signed in, and any sign-in in progress.
#[derive(Default)]
struct Auth {
    status: Option<AuthStatus>,
    checking: Option<Receiver<Option<AuthStatus>>>,
    checked_once: bool,
    login: Option<Login>,
    code: String,
    error: Option<String>,
}

pub struct Chat {
    auth: Auth,
    pub convo: Conversation,
    session: Option<Session>,
    pub bin: Option<PathBuf>,
    pub input: String,
    pub mode: String,
    pub model: Option<String>,
    pub include_context: bool,
    last_context: Option<String>,
    resume: Option<String>,
    /// Sessions being wound down after New: interrupted and given a moment to save.
    retiring: Vec<(Session, Instant)>,
    history: Option<(Instant, Vec<SessionInfo>)>,
    view: View,
    pub focus_input: bool,
    pub attachments: Vec<Attachment>,
    /// The CLI's model catalog, output styles and account, from the session handshake.
    pub models: Vec<ModelInfo>,
    pub output_styles: Vec<String>,
    pub account: Option<AccountInfo>,
    /// Slash commands the CLI accepts, from the handshake.
    pub commands: Vec<SlashCommand>,
    /// Highlighted row of the slash-command popup.
    slash_sel: usize,
    pub settings: SessionSettings,
    /// Settings changed while a turn was running: restart once it ends.
    restart_pending: bool,
    usage: Option<Usage>,
    status: Vec<(String, Vec<(String, String)>)>,
    info_open: bool,
    info_requested: bool,
    /// Where the session runs (for saving exports next to the project).
    session_root: PathBuf,
    /// The standalone installer running in the background.
    installing: Option<Receiver<Result<PathBuf, String>>>,
}

const INPUT_ID: &str = "rim-chat-input";

impl Chat {
    pub fn new(cfg: &ClaudeConfig) -> Self {
        Self {
            auth: Auth::default(),
            convo: Conversation::default(),
            session: None,
            bin: claude::find_binary(cfg.path.as_deref()),
            input: String::new(),
            mode: cfg.permission_mode.clone(),
            model: cfg.model.clone().filter(|m| !m.is_empty()),
            include_context: cfg.include_context,
            last_context: None,
            resume: None,
            retiring: Vec::new(),
            history: None,
            view: View::default(),
            focus_input: false,
            attachments: Vec::new(),
            models: Vec::new(),
            output_styles: Vec::new(),
            account: None,
            commands: Vec::new(),
            slash_sel: 0,
            settings: cfg.session_settings(),
            restart_pending: false,
            usage: None,
            status: Vec::new(),
            info_open: false,
            info_requested: false,
            session_root: PathBuf::new(),
            installing: None,
        }
    }

    /// Change a session setting; takes effect by restarting the CLI (resuming the conversation),
    /// after the current turn if one is running.
    pub fn set_settings(&mut self, s: SessionSettings) {
        if s == self.settings {
            return;
        }
        self.settings = s;
        if self.session.is_some() {
            if self.convo.busy {
                self.restart_pending = true;
            } else {
                self.restart_session();
            }
        }
    }

    fn restart_session(&mut self) {
        self.restart_pending = false;
        if self.session.take().is_some() {
            self.resume = self.convo.session_id.clone();
        }
    }

    /// The catalog entry for the chosen model.
    fn current_model(&self) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.value == self.model)
    }

    pub fn attach(&mut self, a: Attachment) {
        self.attachments.push(a);
        self.focus_input = true;
    }

    /// Attach an image file (PNG, JPEG, GIF or WebP).
    pub fn attach_file(&mut self, ctx: &egui::Context, path: &Path) {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match std::fs::read(path).map_err(|e| e.to_string()).and_then(|b| Attachment::from_bytes(ctx, &name, b)) {
            Ok(a) => self.attach(a),
            Err(e) => self.convo.notice(format!("Couldn't attach {name}: {e}"), true),
        }
    }

    /// Open the account & usage window and ask the CLI for fresh numbers.
    pub fn show_info(&mut self, root: &Path, ctx: &egui::Context) {
        self.info_open = true;
        self.info_requested = false;
        if self.ensure_session(root, ctx)
            && let Some(s) = self.session.as_mut()
        {
            s.request_usage();
            s.request_status();
            self.info_requested = true;
        }
    }

    /// Ask the CLI for the conversation as text and offer to save it.
    pub fn export(&mut self, root: &Path, ctx: &egui::Context) {
        if self.convo.items.is_empty() {
            self.convo.notice("Nothing to export yet", false);
            return;
        }
        if self.ensure_session(root, ctx)
            && let Some(s) = self.session.as_mut()
        {
            s.request_export();
        }
    }

    fn handle_reply(&mut self, r: Reply, root: &Path) {
        match r {
            Reply::Init(init) => {
                if !init.models.is_empty() {
                    self.models = init.models;
                }
                if !init.output_styles.is_empty() {
                    self.output_styles = init.output_styles;
                }
                if !init.commands.is_empty() {
                    self.commands = init.commands;
                }
                self.account = Some(init.account);
            }
            Reply::Usage(u) => self.usage = Some(u),
            Reply::Status(st) => self.status = st,
            Reply::Export { text, filename } => {
                if let Some(path) = rfd::FileDialog::new().set_directory(root).set_file_name(&filename).save_file() {
                    match std::fs::write(&path, text) {
                        Ok(()) => self.convo.notice(format!("Saved {}", path.display()), false),
                        Err(e) => self.convo.notice(format!("Couldn't save {}: {e}", path.display()), true),
                    }
                }
            }
            Reply::Error(kind, e) => self.convo.notice(format!("{kind:?} failed: {e}"), true),
        }
    }

    #[cfg(debug_assertions)]
    pub fn load_demo(&mut self) {
        self.convo = demo_conversation();
    }

    pub fn input_id() -> Id {
        Id::new(INPUT_ID)
    }

    pub fn refresh_binary(&mut self, configured: Option<&str>) {
        self.bin = claude::find_binary(configured);
    }

    pub fn is_busy(&self) -> bool {
        self.convo.busy
    }

    /// Drop the conversation and the CLI process (e.g. when the folder changes).
    pub fn reset(&mut self) {
        // Killing a busy CLI in its first second loses the conversation (it isn't saved yet),
        // so stop the turn politely and let it finish writing in the background.
        if let Some(mut s) = self.session.take()
            && self.convo.busy
        {
            s.interrupt();
            self.retiring.push((s, Instant::now()));
        }
        let (u5, u7) = (self.convo.usage_5h, self.convo.usage_7d);
        self.convo = Conversation::default();
        self.convo.usage_5h = u5;
        self.convo.usage_7d = u7;
        self.last_context = None;
        self.resume = None;
        self.history = None;
    }

    pub fn load_session(&mut self, info: &SessionInfo) {
        self.reset();
        self.convo.load_transcript(&info.path);
        self.resume = Some(info.id.clone());
    }

    pub fn stop(&mut self) {
        if let Some(s) = self.session.as_mut()
            && self.convo.busy
        {
            s.interrupt();
        }
    }

    pub fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
        if let Some(s) = self.session.as_mut() {
            s.set_permission_mode(mode);
        }
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.model = model.map(String::from);
        match (self.session.as_mut(), model) {
            (Some(s), Some(m)) => s.set_model(m),
            // Going back to the default model: restart, resuming this conversation.
            (Some(_), None) if !self.convo.busy => {
                self.session = None;
                self.resume = self.convo.session_id.clone();
            }
            _ => {}
        }
    }

    pub fn insert_mention(&mut self, rel: &str) {
        if self.input.ends_with('@') {
            self.input.pop();
        }
        if !self.input.is_empty() && !self.input.ends_with(char::is_whitespace) {
            self.input.push(' ');
        }
        self.input.push_str(&format!("@{rel} "));
        self.focus_input = true;
    }

    /// Commands matching a message that is still just `/name`: by name first (in the CLI's
    /// order), then by alias. Commands whose UI needs a terminal are left out.
    pub fn slash_matches(&self) -> Vec<usize> {
        let Some(typed) = self.input.strip_prefix('/') else { return vec![] };
        if typed.contains(char::is_whitespace) {
            return vec![];
        }
        let typed = typed.to_ascii_lowercase();
        let mut m: Vec<(bool, usize)> = self
            .commands
            .iter()
            .enumerate()
            .filter(|(_, c)| !self.convo.terminal_only.contains(&c.name))
            .filter_map(|(i, c)| {
                let by_name = c.name.to_ascii_lowercase().starts_with(&typed);
                let by_alias = c.aliases.iter().any(|a| a.to_ascii_lowercase().starts_with(&typed));
                (by_name || by_alias).then_some((!by_name, i))
            })
            .collect();
        m.sort();
        m.into_iter().map(|(_, i)| i).collect()
    }

    /// The command a message starting with `/name ` (arguments follow) refers to.
    fn slash_active(&self) -> Option<&SlashCommand> {
        let typed = self.input.strip_prefix('/')?;
        let name = typed.split_whitespace().next()?;
        if !typed[name.len()..].starts_with(char::is_whitespace) {
            return None;
        }
        self.commands.iter().find(|c| c.name.eq_ignore_ascii_case(name) || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(name)))
    }

    /// Replace the typed prefix with the command's name, ready for its arguments.
    fn complete_slash(&mut self, ctx: &egui::Context, i: usize) {
        self.input = format!("/{} ", self.commands[i].name);
        let id = Self::input_id();
        let mut st = TextEdit::load_state(ctx, id).unwrap_or_default();
        let end = egui::text::CCursor::new(self.input.chars().count());
        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
        st.store(ctx, id);
        // Re-requesting focus would reset the input's key filter for a frame (arrow keys
        // would then move focus), so only ask for it when a mouse pick took it away.
        if !ctx.memory(|m| m.has_focus(id)) {
            self.focus_input = true;
        }
    }

    /// Pull CLI output into the conversation. Returns things the app should react to.
    pub fn poll(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.poll_auth();
        if let Some(rx) = &self.installing
            && let Ok(r) = rx.try_recv()
        {
            self.installing = None;
            match r {
                Ok(p) => {
                    self.convo.notice(format!("Installed Claude Code at {}. New conversations use it.", p.display()), false);
                    self.bin = Some(p);
                    self.auth.checked_once = false;
                }
                Err(e) => self.convo.notice(format!("Couldn't install Claude Code: {e}"), true),
            }
        }
        self.retiring.retain_mut(|(s, since)| {
            let done = s.poll().iter().any(|m| m["type"] == "result");
            !(done || s.exited || since.elapsed().as_secs() > 8)
        });
        let Some(s) = self.session.as_mut() else { return fx };
        let before = self.convo.items.len();
        let mut replies = Vec::new();
        for m in s.poll() {
            if let Some(r) = s.reply(&m) {
                replies.push(r);
                continue;
            }
            fx.extend(self.convo.handle(&m));
        }
        let root = self.session_root.clone();
        for r in replies {
            self.handle_reply(r, &root);
        }
        if self.restart_pending && !self.convo.busy {
            self.restart_session();
            return fx;
        }
        let Some(s) = self.session.as_mut() else { return fx };
        // A failed turn may mean the login expired; find out.
        if self.convo.items.len() > before && matches!(self.convo.items.last(), Some(Item::Notice { error: true, .. })) {
            self.auth.checked_once = false;
        }
        if s.exited {
            let tail = s.stderr_tail();
            self.session = None;
            if self.convo.busy {
                self.convo.busy = false;
                self.convo.status.clear();
                self.convo.deny_pending();
                let mut msg = String::from("Claude Code stopped unexpectedly.");
                if !tail.trim().is_empty() {
                    msg.push('\n');
                    msg.push_str(tail.trim());
                }
                msg.push_str("\nIf you are not signed in, use Sign in at the top of this panel.");
                self.convo.notice(msg, true);
            }
        }
        fx
    }

    fn ensure_session(&mut self, root: &Path, ctx: &egui::Context) -> bool {
        self.session_root = root.to_path_buf();
        if self.session.as_ref().is_some_and(|s| !s.exited) {
            return true;
        }
        let Some(bin) = self.bin.clone() else {
            self.convo.notice(NOT_FOUND, true);
            return false;
        };
        let opts = Options { model: self.model.clone(), permission_mode: self.mode.clone(), resume: self.resume.take().or_else(|| self.convo.session_id.clone()), settings: self.settings.clone() };
        match Session::start(&bin, root, &opts, ctx.clone()) {
            Ok(s) => {
                self.session = Some(s);
                true
            }
            Err(e) => {
                self.convo.notice(format!("Could not start Claude Code: {e}"), true);
                false
            }
        }
    }

    // ---------- account ----------

    pub fn check_auth(&mut self, ctx: &egui::Context) {
        let Some(bin) = self.bin.clone() else { return };
        if self.auth.checking.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(claude::auth_status(&bin));
            ctx.request_repaint();
        });
        self.auth.checking = Some(rx);
        self.auth.checked_once = true;
    }

    fn poll_auth(&mut self) {
        if let Some(rx) = &self.auth.checking
            && let Ok(st) = rx.try_recv()
        {
            self.auth.checking = None;
            if st.as_ref().is_some_and(|s| s.logged_in) {
                self.auth.error = None;
            }
            self.auth.status = st;
        }
        if let Some(login) = self.auth.login.as_mut()
            && let Some(ok) = login.poll()
        {
            let tail = login.output();
            self.auth.login = None;
            self.auth.code.clear();
            self.auth.error = (!ok).then(|| {
                // The prompt has no trailing newline, so an error can share its line.
                let cleaned = tail.replace("Paste code here if prompted >", "\n");
                let t = cleaned.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no details from the CLI").to_string();
                format!("Sign-in failed: {t}")
            });
            self.auth.checked_once = false;
        }
    }

    /// Run the official installer in the background; the panel switches to the new copy when done.
    pub fn install_standalone(&mut self, ctx: &egui::Context) {
        if self.installing.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(claude::install_standalone());
            ctx.request_repaint();
        });
        self.installing = Some(rx);
    }

    pub fn start_login(&mut self, ctx: &egui::Context, console: bool) {
        let Some(bin) = self.bin.clone() else { return };
        self.auth.error = None;
        self.auth.code.clear();
        match Login::start(&bin, console, ctx.clone()) {
            Ok(l) => self.auth.login = Some(l),
            Err(e) => self.auth.error = Some(e),
        }
    }

    pub fn logout(&mut self, ctx: &egui::Context) {
        let Some(bin) = self.bin.clone() else { return };
        self.session = None;
        let ctx = ctx.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = claude::auth_logout(&bin);
            let _ = tx.send(claude::auth_status(&bin));
            ctx.request_repaint();
        });
        self.auth.checking = Some(rx);
    }

    pub fn submit(&mut self, ctx: &egui::Context, root: &Path, context: &ContextInfo) {
        let mut text = self.input.trim().to_string();
        if text.is_empty() && !self.attachments.is_empty() {
            text = "Look at the attached image.".into();
        }
        if text.is_empty() || self.convo.busy {
            return;
        }
        if !self.ensure_session(root, ctx) {
            return;
        }
        let mut full = String::new();
        let mut chip = None;
        if self.include_context
            && let Some(note) = context.note()
            && self.last_context.as_ref() != Some(&note)
        {
            full.push_str(&note);
            full.push('\n');
            chip = context.chip();
            self.last_context = Some(note);
        }
        full.push_str(&text);
        let images: Vec<ImageData> = self.attachments.iter().map(|a| a.image.clone()).collect();
        if let Some(s) = &self.session {
            s.send_user_with(&full, &images);
        }
        let shown: Vec<UserImage> = self.attachments.drain(..).map(|a| a.user_image()).collect();
        self.convo.items.push(Item::User { text, context: chip, images: shown });
        self.convo.busy = true;
        self.convo.status = "starting".into();
        self.input.clear();
    }

    fn apply(&mut self, ev: UiEvent, actions: &mut Vec<ChatAction>) {
        match ev {
            UiEvent::Allow { request_id, input, update } => {
                if let Some(u) = &update
                    && u["type"] == "setMode"
                    && let Some(m) = u["mode"].as_str()
                {
                    self.mode = if m == "default" { "manual".into() } else { m.to_string() };
                }
                let mut resp = json!({ "behavior": "allow", "updatedInput": input });
                if let Some(u) = update {
                    resp["updatedPermissions"] = json!([u]);
                }
                if let Some(s) = &self.session {
                    s.respond(&request_id, resp);
                }
                if let Some(p) = self.convo.perm_tool_mut(&request_id).and_then(|t| t.perm.as_mut()) {
                    p.state = PermState::Allowed;
                }
            }
            UiEvent::Deny { request_id, message, interrupt } => {
                if let Some(s) = &self.session {
                    s.respond(&request_id, json!({ "behavior": "deny", "message": message, "interrupt": interrupt }));
                }
                if let Some(p) = self.convo.perm_tool_mut(&request_id).and_then(|t| t.perm.as_mut()) {
                    p.state = PermState::Denied;
                }
            }
            UiEvent::Open(p, line) => actions.push(ChatAction::OpenFile(p, line)),
            UiEvent::Url(u) => {
                crate::format::open_external(&u);
            }
        }
    }

    // ---------- UI ----------

    pub fn ui(&mut self, ui: &mut Ui, env: &Env) -> Vec<ChatAction> {
        let mut actions = Vec::new();
        let t = env.theme;
        if !self.auth.checked_once && self.bin.is_some() {
            self.check_auth(&ui.ctx().clone());
        }
        self.header(ui, env, &mut actions);
        ui.add_space(2.0);
        self.auth_ui(ui, env);

        let mut submit = false;
        egui::Panel::bottom("chat_input")
            .resizable(false)
            .frame(Frame::NONE.inner_margin(Margin { left: 0, right: 0, top: 6, bottom: 0 }))
            .show(ui, |ui| self.input_ui(ui, env, &mut submit, &mut actions));
        if submit {
            let context = env.context.clone();
            self.submit(&ui.ctx().clone(), env.root, &context);
        }

        let sig = (self.convo.items.len(), self.convo.items.iter().map(|i| match i {
            Item::Text(t) | Item::Thinking(t) => t.len(),
            // Cheap stand-ins for "the card changed": no JSON serialization per frame.
            Item::Tool(t) => t.result.as_ref().map(|r| r.text.len()).unwrap_or(0) + t.input_size() + usize::from(t.is_pending()),
            _ => 0,
        }).sum(), self.convo.busy);
        if sig != self.view.sig {
            self.view.sig = sig;
            self.view.settle = 3;
        }
        if self.view.settle > 0 {
            self.view.settle -= 1;
            ui.ctx().request_repaint();
        }
        let mut events = Vec::new();
        egui::CentralPanel::default().frame(Frame::NONE).show(ui, |ui| {
            ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if self.convo.items.is_empty() {
                    empty_state(ui, env, self.bin.is_some());
                    self.setup_note(ui, env);
                }
                items_ui(ui, &self.convo.items, self.convo.busy, &mut self.view, env, &mut events);
                if self.convo.busy && !self.convo.has_pending() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let s = if self.convo.status == "requesting" || self.convo.status.is_empty() { "Working…" } else { &self.convo.status };
                        ui.label(RichText::new(s).color(t.comment));
                    });
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                }
                ui.add_space(8.0);
            });
        });
        for e in events {
            self.apply(e, &mut actions);
        }
        self.info_window(ui.ctx(), env);
        actions
    }

    fn input_ui(&mut self, ui: &mut Ui, env: &Env, submit: &mut bool, actions: &mut Vec<ChatAction>) {
        let t = env.theme;
        let id = Self::input_id();
        let focused = ui.ctx().memory(|m| m.has_focus(id));
        // `/name` at the start of a message completes from the CLI's command catalog:
        // arrows pick, Tab (or Enter while the name is incomplete) completes.
        let slash = self.slash_matches();
        if focused && !slash.is_empty() {
            let n = slash.len();
            self.slash_sel = self.slash_sel.min(n - 1);
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                self.slash_sel = (self.slash_sel + 1) % n;
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                self.slash_sel = (self.slash_sel + n - 1) % n;
            }
            let exact = self.commands[slash[self.slash_sel]].name.eq_ignore_ascii_case(&self.input[1..]);
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) || (!exact && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))) {
                self.complete_slash(&ui.ctx().clone(), slash[self.slash_sel]);
            }
        }
        if focused {
            let shift = ui.input(|i| i.modifiers.shift);
            if !shift && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) || ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
                *submit = true;
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                if self.convo.busy {
                    self.stop();
                } else {
                    actions.push(ChatAction::FocusEditor);
                }
            }
        }
        // Ctrl+V with a picture on the clipboard attaches it (text pastes as usual).
        if focused
            && ui.input(|i| i.key_pressed(Key::V) && i.modifiers.command)
            && let Some(a) = clipboard_image(ui.ctx())
        {
            self.attach(a);
        }
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for p in dropped.into_iter().filter(|p| is_image_file(p)) {
            self.attach_file(&ui.ctx().clone(), &p);
        }
        Frame::new().fill(t.bg).stroke(Stroke::new(1.0, if focused { t.blue } else { t.gutter })).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(6)).show(ui, |ui| {
            if !slash.is_empty() {
                let mut pick = None;
                ScrollArea::vertical().id_salt("chat-slash").max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.style_mut().interaction.selectable_labels = false;
                    for (k, &ci) in slash.iter().enumerate() {
                        let c = &self.commands[ci];
                        let selected = k == self.slash_sel;
                        let resp = ui
                            .scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
                                Frame::new().fill(if selected { t.bg_highlight } else { Color32::TRANSPARENT }).corner_radius(CornerRadius::same(3)).inner_margin(Margin::symmetric(6, 3)).show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!("/{}", c.name)).color(t.blue).strong());
                                        if !c.hint.is_empty() {
                                            ui.label(RichText::new(&c.hint).color(t.comment).size(11.5));
                                        }
                                        ui.add(Label::new(RichText::new(c.summary()).color(t.fg_dim).size(11.5)).truncate());
                                    });
                                });
                            })
                            .response;
                        if selected {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            pick = Some(ci);
                        }
                    }
                });
                if let Some(ci) = pick {
                    self.complete_slash(&ui.ctx().clone(), ci);
                }
                ui.separator();
            } else if let Some(c) = self.slash_active() {
                let line = if c.hint.is_empty() { format!("/{}  ·  {}", c.name, c.summary()) } else { format!("/{} {}  ·  {}", c.name, c.hint, c.summary()) };
                ui.add(Label::new(RichText::new(line).color(t.comment).size(11.5)).truncate());
            }
            if !self.attachments.is_empty() {
                let mut remove = None;
                ui.horizontal_wrapped(|ui| {
                    for (k, a) in self.attachments.iter().enumerate() {
                        Frame::new().fill(t.bg_highlight).corner_radius(CornerRadius::same(4)).inner_margin(Margin::same(3)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                match &a.texture {
                                    Some(tex) => {
                                        let size = tex.size_vec2();
                                        let h = 44.0;
                                        ui.add(egui::Image::new((tex.id(), egui::vec2(size.x / size.y * h, h))).corner_radius(3.0));
                                    }
                                    None => {
                                        ui.label(RichText::new("🖼").size(20.0));
                                    }
                                }
                                let dims = if a.width > 0 { format!("{}×{}", a.width, a.height) } else { String::new() };
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&a.name).size(11.5).color(t.fg));
                                    ui.label(RichText::new(dims).size(10.5).color(t.comment));
                                });
                                if ui.add(egui::Button::new("×").frame(false)).on_hover_text("Remove").clicked() {
                                    remove = Some(k);
                                }
                            });
                        });
                    }
                });
                if let Some(k) = remove {
                    self.attachments.remove(k);
                }
            }
            let mut want_session = false;
            ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
                let hint = if self.convo.has_pending() { "Approve or deny the request above, or type a message…" } else { "Ask Claude…  (@ mentions a file, / runs a command, Ctrl+V pastes an image, Enter to send)" };
                let before = self.input.len();
                let resp = ui.add(
                    TextEdit::multiline(&mut self.input)
                        .id(id)
                        .hint_text(hint)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .frame(Frame::NONE)
                        .event_filter(EventFilter { horizontal_arrows: true, vertical_arrows: true, tab: false, escape: true }),
                );
                if self.focus_input {
                    resp.request_focus();
                    self.focus_input = false;
                }
                if resp.changed() && self.input.len() > before && self.input.ends_with('@') {
                    let prev = self.input[..self.input.len() - 1].chars().last();
                    if prev.is_none_or(char::is_whitespace) {
                        actions.push(ChatAction::Mention);
                    }
                }
                if resp.changed() {
                    self.slash_sel = 0;
                    // The command list comes from the CLI: start it (no API call until a message is sent).
                    want_session |= self.input.starts_with('/') && self.commands.is_empty() && self.bin.is_some();
                }
            });
            if want_session {
                self.ensure_session(env.root, &ui.ctx().clone());
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let mode_label = MODES.iter().find(|m| m.1 == self.mode).map(|m| m.0).unwrap_or("Ask before edits");
                egui::ComboBox::from_id_salt("chat-mode").selected_text(mode_label).width(130.0).show_ui(ui, |ui| {
                    for (label, mode) in MODES {
                        if ui.selectable_label(self.mode == mode, label).clicked() {
                            self.set_mode(mode);
                        }
                    }
                });
                // The CLI's catalog once the session reported it; the short built-in list before.
                let catalog: Vec<ModelInfo> = if self.models.is_empty() {
                    MODELS.iter().map(|(l, v)| ModelInfo { value: v.map(String::from), label: l.to_string(), description: String::new(), effort_levels: vec![], fast_mode: true, disabled: false }).collect()
                } else {
                    self.models.clone()
                };
                let model_label = catalog.iter().find(|m| m.value == self.model).map(|m| m.label.clone()).unwrap_or_else(|| self.model.clone().unwrap_or_else(|| "Default model".into()));
                let ctx = ui.ctx().clone();
                let root = env.root.to_path_buf();
                let r = egui::ComboBox::from_id_salt("chat-model").selected_text(model_label).width(130.0).show_ui(ui, |ui| {
                    if self.models.is_empty() && self.bin.is_some() {
                        // The full list comes from the CLI: start it (no API call until a message is sent).
                        self.ensure_session(&root, &ctx);
                        ui.label(RichText::new("Loading the model list…").size(11.5).color(t.comment));
                    }
                    for m in &catalog {
                        let text = if m.description.is_empty() { m.label.clone() } else { format!("{}  ", m.label) };
                        let resp = ui.add_enabled(!m.disabled, egui::Button::selectable(self.model == m.value, text));
                        let resp = if m.description.is_empty() { resp } else { resp.on_hover_text(&m.description) };
                        if resp.clicked() {
                            self.set_model(m.value.as_deref());
                        }
                    }
                });
                if let Some(desc) = catalog.iter().find(|m| m.value == self.model).map(|m| m.description.clone()).filter(|d| !d.is_empty()) {
                    r.response.on_hover_text(desc);
                }
                if ui.add(egui::Button::new("🖼").frame(false)).on_hover_text("Attach an image (or paste one with Ctrl+V, or drop a file here)").clicked() {
                    let picked = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"]).pick_files();
                    for p in picked.unwrap_or_default() {
                        self.attach_file(&ctx, &p);
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.convo.busy {
                        if ui.add(egui::Button::new("Stop").fill(t.red.gamma_multiply(0.35))).on_hover_text("Interrupt (Esc)").clicked() {
                            self.stop();
                        }
                    } else if ui.add_enabled(!self.input.trim().is_empty() || !self.attachments.is_empty(), egui::Button::new("Send").fill(t.blue.gamma_multiply(0.35))).clicked() {
                        *submit = true;
                    }
                });
            });
            if let Some(chip) = env.context.chip() {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.include_context, RichText::new(format!("Include {chip}")).size(12.0).color(t.comment));
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;

    fn frame(ctx: &egui::Context, chat: &mut Chat, theme: &Theme, syn: &Syntaxes, events: Vec<egui::Event>) -> Vec<ChatAction> {
        frame_at(ctx, chat, theme, syn, Path::new("/w"), events)
    }

    fn frame_at(ctx: &egui::Context, chat: &mut Chat, theme: &Theme, syn: &Syntaxes, root: &Path, events: Vec<egui::Event>) -> Vec<ChatAction> {
        let mut input = egui::RawInput { events, ..Default::default() };
        input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(420.0, 800.0)));
        let root = root.to_path_buf();
        let context = ContextInfo { path: Some(root.join("src/main.rs")), rel: "src/main.rs".into(), selection: None };
        let mut actions = Vec::new();
        let mut out = ctx.run_ui(input, |ui| {
            let env = Env { theme, syn, root: &root, context: &context };
            actions = chat.ui(ui, &env);
        });
        out.textures_delta.clear();
        actions
    }

    #[test]
    fn renders_every_kind_of_item_without_panicking() {
        let theme = crate::theme::by_name("tokyonight");
        let syn = Syntaxes::new(&theme);
        let ctx = egui::Context::default();
        let mut chat = Chat::new(&ClaudeConfig::default());
        chat.convo = demo_conversation();
        chat.convo.busy = true;
        for _ in 0..3 {
            frame(&ctx, &mut chat, &theme, &syn, vec![]);
        }
        // Empty state and the "not installed" state too.
        let mut empty = Chat::new(&ClaudeConfig::default());
        empty.bin = None;
        frame(&ctx, &mut empty, &theme, &syn, vec![]);
    }

    #[test]
    fn typing_at_asks_for_a_file_mention_and_insert_replaces_it() {
        let theme = crate::theme::by_name("tokyonight");
        let syn = Syntaxes::new(&theme);
        let ctx = egui::Context::default();
        let mut chat = Chat::new(&ClaudeConfig::default());
        chat.focus_input = true;
        frame(&ctx, &mut chat, &theme, &syn, vec![]);
        let actions = frame(&ctx, &mut chat, &theme, &syn, vec![egui::Event::Text("look at @".into())]);
        assert!(actions.iter().any(|a| matches!(a, ChatAction::Mention)));
        chat.insert_mention("src/a.rs");
        assert_eq!(chat.input, "look at @src/a.rs ");
    }

    #[test]
    fn slash_commands_complete_and_then_send() {
        let theme = crate::theme::by_name("tokyonight");
        let syn = Syntaxes::new(&theme);
        let ctx = egui::Context::default();
        let dir = std::env::temp_dir().join(format!("rim-chat-slash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut chat = Chat::new(&ClaudeConfig::default());
        chat.bin = Some(crate::claude::tests::fake_session_cli(&dir));
        chat.focus_input = true;
        frame_at(&ctx, &mut chat, &theme, &syn, &dir, vec![]);
        // Typing "/" starts the CLI so its command list can be shown.
        frame_at(&ctx, &mut chat, &theme, &syn, &dir, vec![egui::Event::Text("/co".into())]);
        assert!(chat.session.is_some());
        spin(&mut chat, |c| !c.commands.is_empty());
        assert_eq!(chat.slash_matches().len(), 3, "compact, code-review and color");
        // Enter completes the highlighted command instead of sending.
        let enter = || egui::Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        frame_at(&ctx, &mut chat, &theme, &syn, &dir, vec![enter()]);
        assert_eq!(chat.input, "/compact ");
        assert!(chat.convo.items.is_empty());
        assert_eq!(chat.slash_active().map(|c| c.name.as_str()), Some("compact"));
        // Aliases match.
        chat.input = "/rev".into();
        assert_eq!(chat.slash_matches(), [1]);
        // Down arrow moves the highlight; an exact name plus Enter sends the message as typed.
        chat.input = "/c".into();
        let down = egui::Event::Key { key: Key::ArrowDown, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        frame_at(&ctx, &mut chat, &theme, &syn, &dir, vec![down]);
        assert_eq!(chat.slash_sel, 1);
        chat.input = "/compact".into();
        frame_at(&ctx, &mut chat, &theme, &syn, &dir, vec![enter()]);
        assert!(matches!(chat.convo.items.first(), Some(Item::User { text, .. }) if text == "/compact"), "{:?}", chat.convo.items);
        // The session's init message names the commands whose UI needs a terminal: hidden from then on.
        spin(&mut chat, |c| !c.convo.terminal_only.is_empty());
        chat.input = "/col".into();
        assert!(chat.slash_matches().is_empty());
        chat.stop();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allow_and_deny_answer_the_cli_and_update_the_card() {
        let mut chat = Chat::new(&ClaudeConfig::default());
        chat.convo = demo_conversation();
        let mut actions = Vec::new();
        chat.apply(UiEvent::Allow { request_id: "r1".into(), input: json!({}), update: Some(json!({"type":"setMode","mode":"acceptEdits"})) }, &mut actions);
        assert_eq!(chat.mode, "acceptEdits");
        let Some(Item::Tool(t)) = chat.convo.items.iter().find(|i| matches!(i, Item::Tool(t) if t.id == "t1")) else { panic!() };
        assert_eq!(t.perm.as_ref().unwrap().state, PermState::Allowed);
        chat.apply(UiEvent::Deny { request_id: "r2".into(), message: "no".into(), interrupt: false }, &mut actions);
        assert!(!chat.convo.has_pending());
    }

    #[test]
    fn attachments_are_scaled_encoded_and_thumbnailed() {
        let ctx = egui::Context::default();
        // A 3000×1000 red picture is scaled to the API's 2000 px limit and PNG-encoded.
        let rgba: Vec<u8> = [255u8, 0, 0, 255].repeat(3000 * 1000);
        let a = Attachment::from_rgba(&ctx, "shot.png", 3000, 1000, &rgba).unwrap();
        assert_eq!((a.width, a.height), (2000, 666));
        assert_eq!(a.image.media_type, "image/png");
        let back = image::load_from_memory(&a.image.bytes).unwrap().to_rgba8();
        assert_eq!((back.width(), back.height()), (2000, 666));
        assert_eq!(back.get_pixel(10, 10).0, [255, 0, 0, 255]);
        let thumb = a.texture.as_ref().unwrap().size();
        assert!(thumb[0] <= 96 && thumb[1] <= 96 && thumb[0] > thumb[1], "{thumb:?}");
        // A small PNG file is sent as it is (bytes unchanged); a JPEG is decoded for its thumbnail.
        let small = Attachment::from_bytes(&ctx, "tiny.png", a.image.bytes.clone()).unwrap();
        assert_eq!(small.image.bytes, a.image.bytes);
        let mut jpg = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(40, 20, image::Rgb([0, 0, 255])).write_to(&mut jpg, image::ImageFormat::Jpeg).unwrap();
        let j = Attachment::from_bytes(&ctx, "blue.jpg", jpg.into_inner()).unwrap();
        assert_eq!((j.width, j.height), (40, 20));
        assert_eq!(j.image.media_type, "image/png", "re-encoded after decoding");
        // GIF and WebP go through untouched, without a thumbnail; other files are refused.
        let g = Attachment::from_bytes(&ctx, "anim.gif", b"GIF89a...".to_vec()).unwrap();
        assert_eq!((g.image.media_type.as_str(), g.texture.is_none()), ("image/gif", true));
        assert!(Attachment::from_bytes(&ctx, "notes.txt", b"hi".to_vec()).is_err());
        assert!(is_image_file(Path::new("/a/b.JPG")) && !is_image_file(Path::new("/a/b.rs")));
    }

    #[test]
    fn submit_sends_attachments_and_keeps_thumbnails() {
        let ctx = egui::Context::default();
        let dir = std::env::temp_dir().join(format!("rim-chat-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut chat = Chat::new(&ClaudeConfig::default());
        chat.bin = Some(crate::claude::tests::fake_session_cli(&dir));
        chat.attach(Attachment::from_rgba(&ctx, "one.png", 2, 2, &[0u8; 16]).unwrap());
        chat.input = "look".into();
        chat.submit(&ctx, &dir, &ContextInfo::default());
        assert!(chat.attachments.is_empty(), "sent");
        let Some(Item::User { images, .. }) = chat.convo.items.first() else { panic!() };
        assert_eq!(images.len(), 1);
        assert!(images[0].texture.is_some());
        let start = Instant::now();
        while chat.is_busy() && start.elapsed().as_secs() < 10 {
            chat.poll();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The fake CLI echoes what it received: one PNG.
        let result = chat.convo.items.iter().find_map(|i| match i { Item::Notice { text, .. } | Item::Text(text) => Some(text.clone()), _ => None });
        assert!(result.as_deref().is_some_and(|t| t.contains("images=1 media=image/png")), "{:?}", chat.convo.items);
        // The handshake filled the model list, with Fable in it, and the account.
        assert!(chat.models.iter().any(|m| m.value.as_deref() == Some("claude-fable-5-1")), "{:?}", chat.models);
        assert_eq!(chat.account.as_ref().and_then(|a| a.email.clone()).as_deref(), Some("me@example.test"));
        assert!(chat.commands.iter().any(|c| c.name == "compact"));
        // Changing a session setting while idle restarts the CLI, resuming the conversation.
        let sid = chat.convo.session_id.clone();
        assert!(sid.is_some());
        chat.set_settings(SessionSettings { fast_mode: true, ..chat.settings.clone() });
        assert!(chat.session.is_none());
        assert_eq!(chat.resume, sid);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reads a picture from this desktop's clipboard (put one there first, e.g. `wl-copy < x.png`).
    /// Run with `cargo test clipboard_image_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn clipboard_image_smoke() {
        let ctx = egui::Context::default();
        match clipboard_image(&ctx) {
            Some(a) => eprintln!("clipboard image: {}x{} ({} bytes as {})", a.width, a.height, a.image.bytes.len(), a.image.media_type),
            None => panic!("no image on the clipboard (or clipboard access failed)"),
        }
    }

    /// Talks to the real `claude` CLI (a couple of cents on the cheapest model).
    /// Run with `cargo test real_cli -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_cli_roundtrip_with_approval() {
        let dir = std::env::temp_dir().join(format!("rim-real-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut chat = Chat::new(&ClaudeConfig::default());
        assert!(chat.bin.is_some(), "no claude binary found");
        chat.model = Some("haiku".into());
        chat.input = "Create a file named hello.txt containing exactly the word hi. Then reply with the single word: done".into();
        chat.submit(&ctx, &dir, &ContextInfo::default());
        let start = Instant::now();
        let mut allowed = false;
        let mut touched = false;
        while chat.is_busy() && start.elapsed().as_secs() < 120 {
            for fx in chat.poll() {
                if matches!(fx, Effect::FileTouched(_)) {
                    touched = true;
                }
            }
            let pending = chat.convo.items.iter().find_map(|i| match i {
                Item::Tool(t) if t.is_pending() => Some((t.perm.as_ref().unwrap().request_id.clone(), t.input.clone())),
                _ => None,
            });
            if let Some((request_id, input)) = pending {
                chat.apply(UiEvent::Allow { request_id, input, update: None }, &mut Vec::new());
                allowed = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(allowed, "never asked for approval: {:?}", chat.convo.items.len());
        assert!(touched, "no FileTouched effect");
        assert_eq!(std::fs::read_to_string(dir.join("hello.txt")).unwrap().trim(), "hi");
        assert!(chat.convo.session_id.is_some());
        assert!(chat.convo.items.iter().any(|i| matches!(i, Item::Text(t) if t.to_lowercase().contains("done"))));
        std::fs::remove_dir_all(&dir).ok();
        if let Some(p) = claude::project_dir(&dir) {
            std::fs::remove_dir_all(p).ok();
        }
    }

    /// The real CLI sees a pasted picture and lists the account's models (including Fable).
    /// Run with `cargo test real_cli_sees -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_cli_sees_an_image_and_lists_models() {
        let dir = std::env::temp_dir().join(format!("rim-real-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut chat = Chat::new(&ClaudeConfig::default());
        assert!(chat.bin.is_some(), "no claude binary found");
        chat.model = Some("haiku".into());
        chat.attach(Attachment::from_rgba(&ctx, "red.png", 8, 8, &[255u8, 0, 0, 255].repeat(64)).unwrap());
        chat.input = "What single color is this image? Answer with one word only.".into();
        chat.submit(&ctx, &dir, &ContextInfo::default());
        let start = Instant::now();
        while chat.is_busy() && start.elapsed().as_secs() < 90 {
            chat.poll();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let answer = chat.convo.items.iter().find_map(|i| match i { Item::Text(t) => Some(t.to_lowercase()), _ => None }).unwrap_or_default();
        eprintln!("answer: {answer}; models: {:?}", chat.models.iter().map(|m| &m.label).collect::<Vec<_>>());
        assert!(answer.contains("red"), "{answer}");
        assert!(chat.models.iter().any(|m| m.label.starts_with("Fable")), "{:?}", chat.models);
        assert!(chat.account.as_ref().is_some_and(|a| a.email.is_some()));
        // The plan's usage meters come back as rows with readable names.
        chat.show_info(&dir, &ctx);
        let start = Instant::now();
        while chat.usage.is_none() && start.elapsed().as_secs() < 30 {
            chat.poll();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let usage = chat.usage.clone().expect("usage reply");
        eprintln!("usage: {:?}", usage.rows.iter().map(|r| format!("{} {:.0}%", r.label, r.percent)).collect::<Vec<_>>());
        assert!(usage.rows.iter().any(|r| r.label.starts_with("Current session")), "{usage:?}");
        std::fs::remove_dir_all(&dir).ok();
        if let Some(p) = claude::project_dir(&dir) {
            std::fs::remove_dir_all(p).ok();
        }
    }

    fn run_turn(chat: &mut Chat) {
        let start = Instant::now();
        while chat.is_busy() && start.elapsed().as_secs() < 90 {
            chat.poll();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!chat.is_busy(), "turn did not finish");
    }

    /// History: a finished conversation shows up in the list, reloads, and continues with its memory intact.
    #[test]
    #[ignore]
    fn real_cli_history_and_resume() {
        let dir = std::env::temp_dir().join(format!("rim-real-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut a = Chat::new(&ClaudeConfig::default());
        a.model = Some("haiku".into());
        a.input = "Remember the secret word tangerine. Reply only: ok".into();
        a.submit(&ctx, &dir, &ContextInfo::default());
        run_turn(&mut a);
        drop(a);

        let sessions = claude::list_sessions(&dir);
        assert_eq!(sessions.len(), 1, "history should list the conversation");
        assert!(sessions[0].title.contains("tangerine"));

        let mut b = Chat::new(&ClaudeConfig::default());
        b.model = Some("haiku".into());
        b.load_session(&sessions[0]);
        assert!(b.convo.items.iter().any(|i| matches!(i, Item::User { text, .. } if text.contains("tangerine"))));
        b.input = "What was the secret word? Reply with just the word.".into();
        b.submit(&ctx, &dir, &ContextInfo::default());
        run_turn(&mut b);
        assert!(b.convo.items.iter().any(|i| matches!(i, Item::Text(t) if t.to_lowercase().contains("tangerine"))), "resumed session lost its memory");
        std::fs::remove_dir_all(&dir).ok();
        if let Some(p) = claude::project_dir(&dir) {
            std::fs::remove_dir_all(p).ok();
        }
    }

    fn spin(chat: &mut Chat, until: impl Fn(&Chat) -> bool) {
        let ctx = egui::Context::default();
        for _ in 0..300 {
            chat.poll();
            if until(chat) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = ctx;
        panic!("timed out");
    }

    #[test]
    fn sign_in_and_out_through_the_panel_with_a_fake_cli() {
        let theme = crate::theme::by_name("tokyonight");
        let syn = Syntaxes::new(&theme);
        let ui_ctx = egui::Context::default();
        let dir = std::env::temp_dir().join(format!("rim-chat-auth-{}", std::process::id()));
        let mut chat = Chat::new(&ClaudeConfig::default());
        chat.bin = Some(claude::tests::fake_auth_cli(&dir));

        // First frame checks the account; signed out shows the banner.
        frame(&ui_ctx, &mut chat, &theme, &syn, vec![]);
        spin(&mut chat, |c| c.auth.status.is_some());
        assert_eq!(chat.auth.status.as_ref().map(|s| s.logged_in), Some(false));
        frame(&ui_ctx, &mut chat, &theme, &syn, vec![]);

        // A wrong code fails with a message and stays signed out.
        chat.start_login(&ui_ctx, false);
        spin(&mut chat, |c| c.auth.login.as_ref().is_some_and(|l| l.url().is_some()));
        frame(&ui_ctx, &mut chat, &theme, &syn, vec![]);
        chat.auth.login.as_mut().unwrap().submit_code("nope");
        spin(&mut chat, |c| c.auth.login.is_none());
        assert!(chat.auth.error.as_deref().is_some_and(|e| e.contains("Invalid code")), "{:?}", chat.auth.error);
        frame(&ui_ctx, &mut chat, &theme, &syn, vec![]);

        // The right code signs in; the banner goes away and the account menu appears.
        chat.start_login(&ui_ctx, true);
        spin(&mut chat, |c| c.auth.login.as_ref().is_some_and(|l| l.url().is_some()));
        chat.auth.login.as_mut().unwrap().submit_code("good#code");
        spin(&mut chat, |c| c.auth.login.is_none());
        frame(&ui_ctx, &mut chat, &theme, &syn, vec![]);
        spin(&mut chat, |c| c.auth.status.as_ref().is_some_and(|s| s.logged_in));
        assert!(chat.auth.error.is_none());
        frame(&ui_ctx, &mut chat, &theme, &syn, vec![]);

        chat.logout(&ui_ctx);
        spin(&mut chat, |c| c.auth.status.as_ref().is_some_and(|s| !s.logged_in));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Pressing New at any moment must not lose the conversation from History.
    #[test]
    #[ignore]
    fn real_cli_new_at_any_moment_keeps_the_conversation() {
        for wait_ms in [0u64, 400, 1500] {
            let dir = std::env::temp_dir().join(format!("rim-newtiming-{}-{wait_ms}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let ctx = egui::Context::default();
            let mut chat = Chat::new(&ClaudeConfig::default());
            chat.model = Some("haiku".into());
            chat.input = "Write a 300 word story about a lighthouse.".into();
            chat.submit(&ctx, &dir, &ContextInfo::default());
            std::thread::sleep(std::time::Duration::from_millis(wait_ms));
            chat.reset();
            let start = Instant::now();
            while start.elapsed().as_secs() < 12 && claude::list_sessions(&dir).is_empty() {
                chat.poll();
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let listed = claude::list_sessions(&dir).len();
            println!("New after {wait_ms}ms: listed={listed}");
            assert_eq!(listed, 1, "conversation lost when New was pressed after {wait_ms}ms");
            std::fs::remove_dir_all(&dir).ok();
            if let Some(p) = claude::project_dir(&dir) {
                std::fs::remove_dir_all(p).ok();
            }
        }
    }

    #[test]
    fn context_note_and_chip() {
        let c = ContextInfo { path: Some(PathBuf::from("/w/a.rs")), rel: "a.rs".into(), selection: Some(Selection { start_line: 2, end_line: 4, text: "x".into() }) };
        let note = c.note().unwrap();
        assert!(note.contains("<ide_opened_file>") && note.contains("lines 2 to 4"));
        assert_eq!(c.chip().unwrap(), "a.rs lines 2-4");
        assert!(ContextInfo::default().note().is_none());
    }

    #[test]
    fn file_references_resolve_only_inside_the_project() {
        let root = std::env::current_dir().unwrap();
        assert!(file_ref(&root, "src/main.rs:5").is_some_and(|(_, l)| l == Some(5)));
        assert!(file_ref(&root, "src/nope.rs").is_none());
        assert!(file_ref(&root, "/etc/passwd").is_none());
        assert!(file_ref(&root, "not a path").is_none());
    }
}

#[cfg(test)]
#[path = "validate/chat_tests.rs"]
mod validate_tests;
