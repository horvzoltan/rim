//! The Claude panel: conversation view, tool cards with approvals and inline
//! diffs, and the prompt box. State that talks to the CLI lives in `Chat`;
//! rendering is split into free functions that only emit `UiEvent`s so the
//! conversation can be borrowed immutably while drawing.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, channel};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use egui::text::LayoutJob;
use egui::{
    Align, Color32, CornerRadius, EventFilter, FontId, Frame, Id, Key, Label, Layout, Margin, Modifiers, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, vec2,
};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use crate::claude::{self, AuthStatus, Conversation, Effect, Item, Login, Options, PermState, Session, SessionInfo, Tool};
use crate::config::ClaudeConfig;
use crate::highlight::{HlCache, Syntaxes};
use crate::md::{self, Block, Span};
use crate::text::hash_str;
use crate::theme::Theme;

pub const MODES: [(&str, &str); 3] = [("Ask before edits", "manual"), ("Edit automatically", "acceptEdits"), ("Plan mode", "plan")];
pub const MODELS: [(&str, Option<&str>); 4] = [("Default model", None), ("Opus", Some("opus")), ("Sonnet", Some("sonnet")), ("Haiku", Some("haiku"))];

const NOT_FOUND: &str = "Claude Code was not found. Install it (`npm install -g @anthropic-ai/claude-code`, or the installer from claude.com/claude-code), then reopen this panel and use Sign in. You can also set `claude.path` in config.toml.";

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

    /// Pull CLI output into the conversation. Returns things the app should react to.
    pub fn poll(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.poll_auth();
        self.retiring.retain_mut(|(s, since)| {
            let done = s.poll().iter().any(|m| m["type"] == "result");
            !(done || s.exited || since.elapsed().as_secs() > 8)
        });
        let Some(s) = self.session.as_mut() else { return fx };
        let before = self.convo.items.len();
        for m in s.poll() {
            fx.extend(self.convo.handle(&m));
        }
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
        if self.session.as_ref().is_some_and(|s| !s.exited) {
            return true;
        }
        let Some(bin) = self.bin.clone() else {
            self.convo.notice(NOT_FOUND, true);
            return false;
        };
        let opts = Options { model: self.model.clone(), permission_mode: self.mode.clone(), resume: self.resume.take().or_else(|| self.convo.session_id.clone()) };
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

    /// Banner shown while signed out or signing in.
    fn auth_ui(&mut self, ui: &mut Ui, env: &Env) {
        let t = env.theme;
        let signing_in = self.auth.login.is_some();
        let signed_out = self.auth.status.as_ref().is_some_and(|s| !s.logged_in);
        if !signing_in && !signed_out && self.auth.error.is_none() {
            return;
        }
        let ctx = ui.ctx().clone();
        Frame::new().fill(t.bg_highlight).stroke(Stroke::new(1.0, t.yellow)).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(e) = self.auth.error.clone() {
                ui.label(RichText::new(e).color(t.red));
                ui.add_space(4.0);
            }
            if let Some(login) = &self.auth.login {
                let url = login.url();
                match &url {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Starting sign-in…");
                        });
                    }
                    Some(url) => {
                        ui.label(RichText::new("Finish signing in").strong().color(t.fg));
                        ui.label(RichText::new("1. Your browser should have opened. If not:").size(12.5).color(t.fg_dim));
                        ui.horizontal(|ui| {
                            if ui.button("Open sign-in page").clicked() {
                                crate::format::open_external(url);
                            }
                            if ui.button("Copy link").clicked() {
                                ctx.copy_text(url.clone());
                            }
                        });
                        ui.label(RichText::new("2. Sign in, then copy the code the page shows.").size(12.5).color(t.fg_dim));
                        ui.label(RichText::new("3. Paste it here:").size(12.5).color(t.fg_dim));
                    }
                }
                if url.is_some() {
                    let mut submit = false;
                    ui.horizontal(|ui| {
                        let r = ui.add(TextEdit::singleline(&mut self.auth.code).hint_text("Paste code…").desired_width(ui.available_width() - 150.0));
                        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        submit |= ui.add_enabled(!self.auth.code.trim().is_empty(), egui::Button::new("Submit").fill(t.green.gamma_multiply(0.3))).clicked();
                    });
                    if submit && !self.auth.code.trim().is_empty() {
                        let code = std::mem::take(&mut self.auth.code);
                        if let Some(l) = self.auth.login.as_mut() {
                            l.submit_code(&code);
                        }
                    }
                }
                if ui.button("Cancel").clicked() {
                    self.auth.login = None;
                }
            } else if signed_out {
                ui.label(RichText::new("Claude isn't signed in").strong().color(t.fg));
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.add(egui::Button::new("Sign in with Claude account").fill(t.blue.gamma_multiply(0.35))).clicked() {
                        self.start_login(&ctx, false);
                    }
                    if ui.button("Use API billing (Console)").clicked() {
                        self.start_login(&ctx, true);
                    }
                });
            }
        });
        ui.add_space(4.0);
    }

    pub fn submit(&mut self, ctx: &egui::Context, root: &Path, context: &ContextInfo) {
        let text = self.input.trim().to_string();
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
        if let Some(s) = &self.session {
            s.send_user(&full);
        }
        self.convo.items.push(Item::User { text, context: chip });
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
        actions
    }

    fn header(&mut self, ui: &mut Ui, env: &Env, actions: &mut Vec<ChatAction>) {
        let t = env.theme;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Claude").strong().size(15.0).color(t.orange));
            if let Some(m) = &self.convo.model {
                ui.label(RichText::new(m).size(11.5).color(t.comment));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new("×").frame(false)).on_hover_text("Close panel (Ctrl+L)").clicked() {
                    actions.push(ChatAction::Close);
                }
                let root = env.root.to_path_buf();
                ui.menu_button("History", |ui| {
                    if self.history.as_ref().is_none_or(|(at, _)| at.elapsed().as_secs() > 30) {
                        self.history = Some((Instant::now(), claude::list_sessions(&root)));
                    }
                    let list = self.history.as_ref().map(|h| &h.1[..]).unwrap_or(&[]);
                    if list.is_empty() {
                        ui.label(RichText::new("No previous conversations").color(t.comment));
                    }
                    let mut pick = None;
                    ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (i, s) in list.iter().enumerate() {
                            let title: String = s.title.chars().take(60).collect();
                            if ui.button(format!("{title}   {}", ago(s.modified))).clicked() {
                                pick = Some(i);
                            }
                        }
                    });
                    if let Some(i) = pick {
                        let info = &self.history.as_ref().unwrap().1[i];
                        let (id, path, title, modified) = (info.id.clone(), info.path.clone(), info.title.clone(), info.modified);
                        self.load_session(&SessionInfo { id, path, title, modified });
                        ui.close();
                    }
                });
                if let Some(st) = self.auth.status.clone().filter(|s| s.logged_in) {
                    let ctx = ui.ctx().clone();
                    ui.menu_button("Account", |ui| {
                        ui.label(RichText::new(st.email.as_deref().unwrap_or("Signed in")).strong());
                        if let Some(p) = &st.plan {
                            ui.label(RichText::new(format!("{p} plan")).size(12.0).color(t.comment));
                        }
                        ui.separator();
                        ui.label(RichText::new("Signing out also signs out Claude Code\nin VS Code and terminals.").size(11.5).color(t.comment));
                        if ui.button("Sign out").clicked() {
                            self.logout(&ctx);
                            ui.close();
                        }
                    });
                }
                if ui.button("New").on_hover_text("Start a new conversation").clicked() {
                    self.reset();
                    self.focus_input = true;
                }
                if let Some(u) = self.convo.usage_5h {
                    let color = if u > 0.9 { t.red } else if u > 0.7 { t.yellow } else { t.comment };
                    ui.label(RichText::new(format!("{:.0}% of 5h limit", u * 100.0)).size(11.5).color(color)).on_hover_text(match self.convo.usage_7d {
                        Some(w) => format!("Weekly usage: {:.0}%", w * 100.0),
                        None => "Usage of your plan's 5-hour window".into(),
                    });
                }
            });
        });
    }

    fn input_ui(&mut self, ui: &mut Ui, env: &Env, submit: &mut bool, actions: &mut Vec<ChatAction>) {
        let t = env.theme;
        let id = Self::input_id();
        let focused = ui.ctx().memory(|m| m.has_focus(id));
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
        Frame::new().fill(t.bg).stroke(Stroke::new(1.0, if focused { t.blue } else { t.gutter })).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(6)).show(ui, |ui| {
            ScrollArea::vertical().max_height(180.0).auto_shrink([false, true]).show(ui, |ui| {
                let hint = if self.convo.has_pending() { "Approve or deny the request above, or type a message…" } else { "Ask Claude…  (@ to mention a file, Enter to send)" };
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
            });
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
                let model_label = MODELS.iter().find(|m| m.1 == self.model.as_deref()).map(|m| m.0).unwrap_or("Custom model");
                egui::ComboBox::from_id_salt("chat-model").selected_text(model_label).width(110.0).show_ui(ui, |ui| {
                    for (label, model) in MODELS {
                        if ui.selectable_label(self.model.as_deref() == model, label).clicked() {
                            self.set_model(model);
                        }
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.convo.busy {
                        if ui.add(egui::Button::new("Stop").fill(t.red.gamma_multiply(0.35))).on_hover_text("Interrupt (Esc)").clicked() {
                            self.stop();
                        }
                    } else if ui.add_enabled(!self.input.trim().is_empty(), egui::Button::new("Send").fill(t.blue.gamma_multiply(0.35))).clicked() {
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

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn empty_state(ui: &mut Ui, env: &Env, has_bin: bool) {
    let t = env.theme;
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("Claude").size(28.0).color(t.orange));
        ui.add_space(6.0);
        if has_bin {
            ui.label(RichText::new("Ask about your code, or have Claude change it.").color(t.fg_dim));
            ui.add_space(10.0);
            for line in ["Your open file and selection are shared automatically", "@ mentions a file", "Edits ask for your approval first", "Ctrl+L switches between editor and chat"] {
                ui.label(RichText::new(line).size(12.5).color(t.comment));
            }
        } else {
            ui.label(RichText::new(NOT_FOUND).color(t.yellow));
        }
    });
}

// ---------- rendering ----------

fn rel(root: &Path, p: &str) -> String {
    Path::new(p).strip_prefix(root).map(|r| r.to_string_lossy().to_string()).unwrap_or_else(|_| p.to_string())
}

fn abs(root: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { root.join(path) }
}

fn items_ui(ui: &mut Ui, items: &[Item], busy: bool, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
    let t = env.theme;
    for (idx, item) in items.iter().enumerate() {
        ui.add_space(6.0);
        match item {
            Item::User { text, context } => {
                Frame::new().fill(t.bg_highlight).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(Label::new(RichText::new(text).color(t.fg)));
                    if let Some(c) = context {
                        ui.label(RichText::new(c).size(11.5).color(t.comment));
                    }
                });
            }
            Item::Text(text) => markdown_ui(ui, text, view, env, ev),
            Item::Thinking(text) => {
                if !text.trim().is_empty() {
                    egui::CollapsingHeader::new(RichText::new("Thinking").italics().color(t.comment)).id_salt(("think", idx)).default_open(false).show(ui, |ui| {
                        ui.label(RichText::new(text).italics().color(t.comment));
                    });
                }
            }
            Item::Tool(tool) => tool_ui(ui, idx, tool, busy, view, env, ev),
            Item::Notice { text, error } => {
                Frame::new().stroke(Stroke::new(1.0, if *error { t.red } else { t.gutter })).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(text).color(if *error { t.red } else { t.fg_dim }));
                });
            }
        }
    }
}

fn markdown_ui(ui: &mut Ui, text: &str, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
    let t = env.theme;
    let blocks = md::parse(text);
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            ui.add_space(4.0);
        }
        match b {
            Block::Heading(level, spans) => {
                let size = 20.0 - (*level as f32 - 1.0) * 1.6;
                ui.label(RichText::new(md::plain(spans)).size(size.max(14.0)).strong().color(t.blue));
            }
            Block::Para(spans) => inline_ui(ui, spans, env, ev, false),
            Block::Item { indent, marker, spans } => {
                ui.horizontal_top(|ui| {
                    ui.add_space(6.0 + *indent as f32 * 16.0);
                    ui.label(RichText::new(marker).color(t.comment));
                    ui.vertical(|ui| inline_ui(ui, spans, env, ev, false));
                });
            }
            Block::Quote(spans) => {
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(3.0, 18.0), Sense::hover());
                    ui.painter().rect_filled(r, 1.0, t.gutter);
                    ui.vertical(|ui| inline_ui(ui, spans, env, ev, true));
                });
            }
            Block::Rule => {
                ui.separator();
            }
            Block::Code { lang, text } => code_block(ui, lang, text, view, env),
            Block::Table { header, rows } => {
                egui::Grid::new(("md-table", hash_str(text) ^ i as u64)).striped(true).spacing(vec2(14.0, 4.0)).show(ui, |ui| {
                    for h in header {
                        ui.label(RichText::new(md::plain(h)).strong().color(t.fg));
                    }
                    ui.end_row();
                    for r in rows {
                        for c in r {
                            ui.label(RichText::new(md::plain(c)).color(t.fg_dim));
                        }
                        ui.end_row();
                    }
                });
            }
        }
    }
}

/// `path`, `path:12` or `path:12:3` that exists under the project root.
fn file_ref(root: &Path, text: &str) -> Option<(PathBuf, Option<usize>)> {
    if text.len() > 200 || text.contains(char::is_whitespace) || !(text.contains('/') || text.contains('.')) {
        return None;
    }
    type Cache = HashMap<(PathBuf, String), (Instant, Option<(PathBuf, Option<usize>)>)>;
    thread_local! {
        // The panel redraws often; don't stat() every inline code span each frame. Entries
        // expire so files created later (e.g. by Claude) become links.
        static CACHE: std::cell::RefCell<Cache> = Default::default();
    }
    let key = (root.to_path_buf(), text.to_string());
    if let Some(hit) = CACHE.with_borrow(|c| c.get(&key).filter(|(at, _)| at.elapsed().as_secs() < 5).map(|(_, v)| v.clone())) {
        return hit;
    }
    let v = file_ref_uncached(root, text);
    CACHE.with_borrow_mut(|c| {
        if c.len() > 2000 {
            c.clear();
        }
        c.insert(key, (Instant::now(), v.clone()));
    });
    v
}

fn file_ref_uncached(root: &Path, text: &str) -> Option<(PathBuf, Option<usize>)> {
    let mut parts = text.split(':');
    let path = parts.next()?;
    let line = parts.next().and_then(|l| l.parse::<usize>().ok());
    let p = abs(root, path);
    (p.is_file() && p.starts_with(root)).then_some((p, line))
}

fn inline_ui(ui: &mut Ui, spans: &[Span], env: &Env, ev: &mut Vec<UiEvent>, dim: bool) {
    let t = env.theme;
    let base = if dim { t.comment } else { t.fg_dim };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for s in spans {
            if s.code {
                if let Some((p, line)) = file_ref(env.root, &s.text) {
                    if ui.link(RichText::new(&s.text).monospace().color(t.cyan)).on_hover_text("Open file").clicked() {
                        ev.push(UiEvent::Open(p, line));
                    }
                } else {
                    ui.label(RichText::new(&s.text).monospace().color(t.cyan).background_color(t.bg_highlight));
                }
                continue;
            }
            let mut rt = RichText::new(&s.text).color(if s.bold { t.fg } else { base });
            if s.bold {
                rt = rt.strong();
            }
            if s.italic {
                rt = rt.italics();
            }
            match &s.link {
                Some(url) => {
                    if ui.link(rt.color(t.blue)).on_hover_text(url).clicked() {
                        match file_ref(env.root, url.trim_start_matches("file://")) {
                            Some((p, line)) => ev.push(UiEvent::Open(p, line)),
                            None => ev.push(UiEvent::Url(url.clone())),
                        }
                    }
                }
                None => {
                    ui.label(rt);
                }
            }
        }
    });
}

fn highlighted(view: &mut View, env: &Env, lang: &str, text: &str, font: &FontId) -> LayoutJob {
    let key = hash_str(&format!("{lang}\0{text}\0{}\0{}", env.syn.generation, font.size));
    if view.code_cache.len() > 400 {
        view.code_cache.clear();
    }
    view.code_cache
        .entry(key)
        .or_insert_with(|| {
            let syntax = env.syn.set.find_syntax_by_token(lang).map(|s| s.name.clone()).unwrap_or_else(|| "Plain Text".into());
            let mut job = HlCache::default().layout_job(text, env.syn, &syntax, env.theme, font);
            job.wrap.max_width = f32::INFINITY;
            job
        })
        .clone()
}

fn code_block(ui: &mut Ui, lang: &str, text: &str, view: &mut View, env: &Env) {
    let t = env.theme;
    let font = FontId::monospace(12.5);
    let job = highlighted(view, env, lang, text, &font);
    Frame::new().fill(t.bg_dark).stroke(Stroke::new(1.0, t.border)).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(if lang.is_empty() { "text" } else { lang }).size(11.0).color(t.comment));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("Copy").size(11.0)).frame(false)).clicked() {
                    ui.ctx().copy_text(text.to_string());
                }
            });
        });
        ScrollArea::horizontal().id_salt(hash_str(text)).auto_shrink([false, true]).show(ui, |ui| {
            ui.add(Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
        });
    });
}

// ---------- tool cards ----------

fn tool_title(t: &Tool, root: &Path) -> (String, String) {
    let s = |k: &str| t.input[k].as_str().unwrap_or("").to_string();
    let first = |v: String| v.lines().next().unwrap_or("").chars().take(90).collect::<String>();
    match t.name.as_str() {
        "Read" => ("Read".into(), rel(root, &s("file_path"))),
        "Edit" | "MultiEdit" => ("Edit".into(), rel(root, &s("file_path"))),
        "Write" => ("Write".into(), rel(root, &s("file_path"))),
        "NotebookEdit" => ("Edit notebook".into(), rel(root, &s("notebook_path"))),
        "Bash" | "PowerShell" => (t.name.clone(), first(if s("description").is_empty() { s("command") } else { s("description") })),
        "Grep" => ("Search".into(), s("pattern")),
        "Glob" => ("Find files".into(), s("pattern")),
        "WebFetch" => ("Fetch".into(), s("url")),
        "WebSearch" => ("Search the web".into(), s("query")),
        "Task" | "Agent" => ("Agent".into(), first(s("description"))),
        "TodoWrite" => ("Update tasks".into(), String::new()),
        "AskUserQuestion" => ("Question".into(), String::new()),
        "ExitPlanMode" => ("Plan".into(), String::new()),
        other => (other.to_string(), String::new()),
    }
}

fn tool_ui(ui: &mut Ui, idx: usize, tool: &Tool, busy: bool, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
    let t = env.theme;
    let pending = tool.is_pending();
    let denied = tool.perm.as_ref().is_some_and(|p| p.state == PermState::Denied);
    let (dot, status) = match (&tool.result, denied) {
        _ if pending => (t.yellow, "waiting for approval"),
        (Some(r), _) if r.is_error => (t.red, if denied { "denied" } else { "failed" }),
        (Some(_), _) => (t.green, ""),
        (None, true) => (t.red, "denied"),
        (None, false) if busy => (t.blue, "running"),
        (None, false) => (t.comment, ""),
    };
    let (verb, target) = tool_title(tool, env.root);
    let border = if pending { t.yellow } else { t.border };
    Frame::new().fill(t.bg_dark).stroke(Stroke::new(1.0, border)).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 3.5, dot);
            ui.label(RichText::new(&verb).strong().color(t.fg));
            if !target.is_empty() {
                match tool.path() {
                    Some(p) => {
                        if ui.link(RichText::new(&target).monospace().size(12.5).color(t.cyan)).clicked() {
                            ev.push(UiEvent::Open(abs(env.root, p), None));
                        }
                    }
                    None => {
                        ui.add(Label::new(RichText::new(&target).monospace().size(12.5).color(t.fg_dim)).truncate());
                    }
                }
            }
            if !status.is_empty() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(status).size(11.5).color(dot));
                });
            }
        });

        match tool.name.as_str() {
            "Edit" => {
                let (old, new) = (tool.input["old_string"].as_str().unwrap_or(""), tool.input["new_string"].as_str().unwrap_or(""));
                if !old.is_empty() || !new.is_empty() {
                    diff_ui(ui, view, (&tool.id, 0), old, new, env);
                }
            }
            "MultiEdit" => {
                for (n, e) in tool.input["edits"].as_array().into_iter().flatten().enumerate() {
                    diff_ui(ui, view, (&tool.id, n), e["old_string"].as_str().unwrap_or(""), e["new_string"].as_str().unwrap_or(""), env);
                }
            }
            "Write" => {
                let content = tool.input["content"].as_str().unwrap_or("");
                if !content.is_empty() {
                    diff_ui(ui, view, (&tool.id, 0), "", content, env);
                }
            }
            "Bash" | "PowerShell" => {
                if let Some(cmd) = tool.input["command"].as_str() {
                    Frame::new().fill(t.bg).corner_radius(CornerRadius::same(4)).inner_margin(Margin::same(6)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(Label::new(RichText::new(format!("$ {cmd}")).monospace().size(12.5).color(t.green)));
                    });
                }
            }
            "TodoWrite" => {
                for todo in tool.input["todos"].as_array().into_iter().flatten() {
                    let text = todo["content"].as_str().unwrap_or("");
                    let (mark, color) = match todo["status"].as_str() {
                        Some("completed") => ("[x]", t.comment),
                        Some("in_progress") => ("[~]", t.blue),
                        _ => ("[ ]", t.fg_dim),
                    };
                    let mut rt = RichText::new(format!("{mark} {text}")).monospace().size(12.5).color(color);
                    if todo["status"] == "completed" {
                        rt = rt.strikethrough();
                    }
                    ui.label(rt);
                }
            }
            "AskUserQuestion" if pending => ask_ui(ui, tool, view, env, ev),
            "ExitPlanMode" => {
                if let Some(plan) = tool.input["plan"].as_str() {
                    ui.add_space(4.0);
                    markdown_ui(ui, plan, view, env, ev);
                }
            }
            _ => {}
        }

        if pending && tool.name != "AskUserQuestion" {
            permission_buttons(ui, tool, env, ev);
        }

        // Details and output stay folded away.
        let generic = !matches!(tool.name.as_str(), "Edit" | "Write" | "MultiEdit" | "TodoWrite" | "AskUserQuestion" | "ExitPlanMode");
        if let Some(r) = &tool.result
            && !r.text.trim().is_empty()
            && (generic || r.is_error)
        {
            egui::CollapsingHeader::new(RichText::new(if r.is_error { "Error" } else { "Output" }).size(12.0).color(t.comment)).id_salt(("res", idx)).default_open(r.is_error && !denied).show(ui, |ui| {
                let text: String = r.text.lines().take(200).collect::<Vec<_>>().join("\n");
                ScrollArea::both().id_salt(("res-scroll", idx)).max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.add(Label::new(RichText::new(text).monospace().size(12.0).color(if r.is_error { t.red } else { t.fg_dim })).wrap_mode(egui::TextWrapMode::Extend));
                });
            });
        }
    });
}

fn permission_buttons(ui: &mut Ui, tool: &Tool, env: &Env, ev: &mut Vec<UiEvent>) {
    let t = env.theme;
    let Some(p) = &tool.perm else { return };
    let plan = tool.name == "ExitPlanMode";
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        let allow_label = if plan { "Approve plan" } else { "Allow" };
        if ui.add(egui::Button::new(allow_label).fill(t.green.gamma_multiply(0.3))).clicked() {
            ev.push(UiEvent::Allow { request_id: p.request_id.clone(), input: tool.input.clone(), update: None });
        }
        for s in &p.suggestions {
            let label = match (s["type"].as_str(), s["mode"].as_str()) {
                (Some("setMode"), Some("acceptEdits")) => "Allow, and accept edits".to_string(),
                (Some("setMode"), Some(m)) => format!("Allow, switch to {m}"),
                (Some("addRules"), _) => "Allow always".to_string(),
                _ => continue,
            };
            if ui.button(label).clicked() {
                ev.push(UiEvent::Allow { request_id: p.request_id.clone(), input: tool.input.clone(), update: Some(s.clone()) });
            }
        }
        let deny_label = if plan { "Keep planning" } else { "Deny" };
        if ui.add(egui::Button::new(deny_label).fill(t.red.gamma_multiply(0.25))).clicked() {
            let message = if plan {
                "The user wants to keep planning. Revise the plan based on their next message.".to_string()
            } else {
                "The user doesn't want to proceed with this tool use. The tool use was rejected (if it was a file edit, nothing was written). Stop and wait for the user to say how to proceed.".to_string()
            };
            ev.push(UiEvent::Deny { request_id: p.request_id.clone(), message, interrupt: !plan });
        }
    });
    if !p.description.is_empty() && !matches!(tool.name.as_str(), "Edit" | "Write" | "Bash") {
        ui.label(RichText::new(&p.description).size(11.5).color(t.comment));
    }
}

fn ask_ui(ui: &mut Ui, tool: &Tool, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
    let t = env.theme;
    let Some(p) = &tool.perm else { return };
    let questions = tool.input["questions"].as_array().cloned().unwrap_or_default();
    let st = view.asks.entry(tool.id.clone()).or_default();
    st.picked.resize(questions.len(), Vec::new());
    st.other.resize(questions.len(), String::new());
    for (qi, q) in questions.iter().enumerate() {
        let options = q["options"].as_array().cloned().unwrap_or_default();
        st.picked[qi].resize(options.len(), false);
        let multi = q["multiSelect"].as_bool().unwrap_or(false);
        ui.add_space(6.0);
        ui.label(RichText::new(q["question"].as_str().unwrap_or("")).strong().color(t.fg));
        for (oi, o) in options.iter().enumerate() {
            let selected = st.picked[qi][oi];
            let label = o["label"].as_str().unwrap_or("");
            let resp = ui.selectable_label(selected, label);
            if let Some(d) = o["description"].as_str().filter(|d| !d.is_empty()) {
                resp.clone().on_hover_text(d);
                ui.label(RichText::new(d).size(11.5).color(t.comment));
            }
            if resp.clicked() {
                if multi {
                    st.picked[qi][oi] = !selected;
                } else {
                    st.picked[qi].iter_mut().for_each(|x| *x = false);
                    st.picked[qi][oi] = true;
                }
            }
        }
        ui.add(TextEdit::singleline(&mut st.other[qi]).hint_text("Other…").desired_width(f32::INFINITY));
    }
    let answered = |qi: usize| st.picked[qi].iter().any(|x| *x) || !st.other[qi].trim().is_empty();
    let ready = (0..questions.len()).all(answered);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.add_enabled(ready, egui::Button::new("Submit").fill(t.green.gamma_multiply(0.3))).clicked() {
            let mut answers = serde_json::Map::new();
            for (qi, q) in questions.iter().enumerate() {
                let options = q["options"].as_array().cloned().unwrap_or_default();
                let mut chosen: Vec<String> = options.iter().enumerate().filter(|(oi, _)| st.picked[qi][*oi]).map(|(_, o)| o["label"].as_str().unwrap_or("").to_string()).collect();
                if !st.other[qi].trim().is_empty() {
                    chosen = vec![st.other[qi].trim().to_string()];
                }
                answers.insert(q["question"].as_str().unwrap_or("").to_string(), Value::String(chosen.join(", ")));
            }
            let mut input = tool.input.clone();
            input["answers"] = Value::Object(answers);
            ev.push(UiEvent::Allow { request_id: p.request_id.clone(), input, update: None });
        }
        if ui.add(egui::Button::new("Skip").fill(t.red.gamma_multiply(0.25))).clicked() {
            ev.push(UiEvent::Deny { request_id: p.request_id.clone(), message: "The user skipped the question.".into(), interrupt: false });
        }
    });
}

fn compute_diff(old: &str, new: &str) -> DiffLines {
    const MAX_LINES: usize = 60;
    let diff = TextDiff::from_lines(old, new);
    let mut lines = Vec::new();
    for (gi, group) in diff.grouped_ops(2).iter().enumerate() {
        for op in group {
            for ch in diff.iter_changes(op) {
                if lines.len() >= MAX_LINES {
                    return DiffLines { lines, truncated: true };
                }
                lines.push((gi, ch.tag(), ch.value().trim_end_matches('\n').to_string()));
            }
        }
    }
    DiffLines { lines, truncated: false }
}

fn diff_ui(ui: &mut Ui, view: &mut View, key: (&str, usize), old: &str, new: &str, env: &Env) {
    let t = env.theme;
    // Keyed by card and the text lengths: a streaming edit keeps changing until it's complete.
    let k = hash_str(&format!("{}\0{}\0{}\0{}", key.0, key.1, old.len(), new.len()));
    if view.diff_cache.len() > 500 {
        view.diff_cache.clear();
    }
    let d = view.diff_cache.entry(k).or_insert_with(|| compute_diff(old, new)).clone();
    ui.add_space(4.0);
    let mut last_group = 0;
    for (gi, tag, line) in &d.lines {
        if *gi != last_group {
            ui.label(RichText::new("⋯").color(t.comment));
            last_group = *gi;
        }
        let (sign, color, bg) = match tag {
            ChangeTag::Delete => ("-", t.red, Some(t.red.gamma_multiply(0.16))),
            ChangeTag::Insert => ("+", t.green, Some(t.green.gamma_multiply(0.16))),
            ChangeTag::Equal => (" ", t.comment, None),
        };
        Frame::new().fill(bg.unwrap_or(Color32::TRANSPARENT)).inner_margin(Margin::symmetric(4, 0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(Label::new(RichText::new(format!("{sign} {line}")).monospace().size(12.0).color(color)));
        });
    }
    if d.truncated {
        ui.label(RichText::new("… diff truncated").size(11.5).color(t.comment));
    }
}

/// A conversation with every kind of item; used by tests and the debug-only demo mode.
#[cfg(any(test, debug_assertions))]
pub fn demo_conversation() -> Conversation {
    let mut c = Conversation::default();
    let lines = [
        r##"{"type":"assistant","message":{"id":"a","content":[{"type":"text","text":"# Plan\n\nI'll **fix** the `src/main.rs` bug:\n\n- first\n- second\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n"}]}}"##,
        r#"{"type":"assistant","message":{"id":"a","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/src/main.rs","old_string":"a\nb\n","new_string":"a\nc\n"}}]}}"#,
        r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"/w/src/main.rs","old_string":"a\nb\n","new_string":"a\nc\n"},"permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],"tool_use_id":"t1"}}"#,
        r#"{"type":"assistant","message":{"id":"b","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cargo test","description":"Run tests"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok\nall passed"}]}}"#,
        r#"{"type":"assistant","message":{"id":"c","content":[{"type":"tool_use","id":"t3","name":"TodoWrite","input":{"todos":[{"content":"one","status":"completed"},{"content":"two","status":"in_progress"}]}}]}}"#,
        r#"{"type":"assistant","message":{"id":"d","content":[{"type":"tool_use","id":"t4","name":"AskUserQuestion","input":{"questions":[{"question":"Which?","options":[{"label":"A","description":"first"},{"label":"B"}],"multiSelect":false}]}}]}}"#,
        r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which?","options":[{"label":"A","description":"first"},{"label":"B"}],"multiSelect":false}]},"tool_use_id":"t4"}}"#,
    ];
    for l in lines {
        c.handle(&serde_json::from_str(l).unwrap());
    }
    c.items.insert(0, Item::User { text: "please fix it".into(), context: Some("src/main.rs lines 3-5".into()) });
    c.items.push(Item::Notice { text: "Something failed".into(), error: true });
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, chat: &mut Chat, theme: &Theme, syn: &Syntaxes, events: Vec<egui::Event>) -> Vec<ChatAction> {
        let mut input = egui::RawInput { events, ..Default::default() };
        input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(420.0, 800.0)));
        let root = PathBuf::from("/w");
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
