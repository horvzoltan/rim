//! Claude Code integration. Drives the local `claude` CLI in headless
//! stream-json mode (the same engine the official editor extensions use), so
//! login, models, permissions, CLAUDE.md, MCP servers and skills all behave
//! exactly as they do in a terminal.
//!
//! `Session` owns the child process; `Conversation` turns its JSON-lines
//! output into displayable items and is independent of the UI.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::format::find_in_path;

/// Find the `claude` executable: the configured path, `$PATH`, the usual install
/// locations, and finally the binary bundled with the VS Code extension.
pub fn find_binary(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = configured.map(str::trim).filter(|p| !p.is_empty()) {
        let path = match (p.strip_prefix("~/"), dirs::home_dir()) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(p),
        };
        return if path.is_file() { Some(path) } else { find_in_path(p) };
    }
    if let Some(p) = find_in_path("claude") {
        return Some(p);
    }
    let home = dirs::home_dir()?;
    for rel in [".local/bin/claude", ".claude/local/claude", ".npm-global/bin/claude", ".bun/bin/claude"] {
        let p = home.join(rel);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in [".vscode/extensions", ".vscode-insiders/extensions", ".cursor/extensions", ".vscode-oss/extensions"] {
        let Ok(rd) = std::fs::read_dir(home.join(dir)) else { continue };
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with("anthropic.claude-code") {
                continue;
            }
            let p = e.path().join("resources/native-binary/claude");
            let Ok(m) = std::fs::metadata(&p).and_then(|m| m.modified()) else { continue };
            if best.as_ref().is_none_or(|(t, _)| m > *t) {
                best = Some((m, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

// ---------- account ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub method: Option<String>,
}

pub fn parse_auth_status(text: &str) -> Option<AuthStatus> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    Some(AuthStatus {
        logged_in: v["loggedIn"].as_bool().unwrap_or(false),
        email: v["email"].as_str().map(String::from),
        plan: v["subscriptionType"].as_str().map(String::from),
        method: v["authMethod"].as_str().map(String::from),
    })
}

/// Ask the CLI who is signed in. `None` if it could not be run or understood.
pub fn auth_status(bin: &Path) -> Option<AuthStatus> {
    let out = Command::new(bin).args(["auth", "status", "--json"]).stdin(Stdio::null()).output().ok()?;
    parse_auth_status(&String::from_utf8_lossy(&out.stdout))
}

pub fn auth_logout(bin: &Path) -> Result<(), String> {
    let out = Command::new(bin).args(["auth", "logout"]).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// Whether this CLI has `--tools` (newer versions), which lets a one-shot question run with
/// no tools at all. Checked once per process.
fn supports_tools_flag(bin: &Path) -> bool {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&v) = cache.lock().unwrap().get(bin) {
        return v;
    }
    let help = Command::new(bin).arg("--help").stdin(Stdio::null()).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let v = help.contains("--tools");
    cache.lock().unwrap().insert(bin.to_path_buf(), v);
    v
}

/// The arguments for a one-shot, tool-less question (`claude -p`, prompt on stdin).
pub fn ask_once_args(model: Option<&str>, tools_flag: bool) -> Vec<String> {
    let mut args: Vec<String> = ["-p", "--output-format", "text", "--max-turns", "1"].iter().map(|s| s.to_string()).collect();
    if tools_flag {
        args.extend(["--tools".to_string(), String::new()]);
    }
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        args.extend(["--model".to_string(), m.to_string()]);
    }
    args
}

/// Ask the CLI one question headlessly and return its plain-text answer. Blocks: call it
/// from a worker thread. The prompt goes in on stdin, so its length does not matter.
pub fn ask_once(bin: &Path, cwd: &Path, prompt: &str, model: Option<&str>, timeout: Duration) -> Result<String, String> {
    let args = ask_once_args(model, supports_tools_flag(bin));
    let mut child = Command::new(bin)
        .args(&args)
        .current_dir(cwd)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    let out = crate::format::wait_with_timeout(child, timeout)?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || text.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().lines().last().unwrap_or("").to_string();
        return Err(if err.is_empty() { format!("claude exited with {}", out.status) } else { err });
    }
    Ok(text)
}

fn pump(mut r: impl Read + Send + 'static, out: Arc<Mutex<String>>, ctx: egui::Context) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
            ctx.request_repaint();
        }
    })
}

/// A running `claude auth login`: it opens the browser, prints a link, and waits for the
/// code the sign-in page shows.
pub struct Login {
    child: Child,
    stdin: Option<ChildStdin>,
    output: Arc<Mutex<String>>,
    pumps: Vec<std::thread::JoinHandle<()>>,
    exited: Option<(bool, std::time::Instant)>,
}

impl Login {
    pub fn start(bin: &Path, console: bool, ctx: egui::Context) -> Result<Login, String> {
        let mut cmd = Command::new(bin);
        cmd.args(["auth", "login"]);
        if console {
            cmd.arg("--console");
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{}: {e}", bin.display()))?;
        let output = Arc::new(Mutex::new(String::new()));
        let pumps = vec![pump(child.stdout.take().unwrap(), output.clone(), ctx.clone()), pump(child.stderr.take().unwrap(), output.clone(), ctx)];
        Ok(Login { stdin: child.stdin.take(), child, output, pumps, exited: None })
    }

    pub fn output(&self) -> String {
        self.output.lock().map(|o| o.clone()).unwrap_or_default()
    }

    /// The sign-in link, once the CLI has printed it.
    pub fn url(&self) -> Option<String> {
        self.output().split_whitespace().find(|w| w.starts_with("https://")).map(String::from)
    }

    pub fn submit_code(&mut self, code: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{}", code.trim());
            let _ = stdin.flush();
        }
    }

    /// `Some(success)` once the process has ended.
    pub fn poll(&mut self) -> Option<bool> {
        let ok = match self.exited {
            Some((ok, _)) => ok,
            None => {
                let ok = self.child.try_wait().ok().flatten()?.success();
                self.exited = Some((ok, std::time::Instant::now()));
                ok
            }
        };
        // Let the readers collect the last output, but don't wait forever if a browser
        // inherited the pipes. Checked again on later frames instead of sleeping here.
        let (_, at) = self.exited.unwrap();
        (self.pumps.iter().all(|h| h.is_finished()) || at.elapsed() > std::time::Duration::from_millis(300)).then_some(ok)
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Options {
    pub model: Option<String>,
    pub permission_mode: String,
    pub resume: Option<String>,
    pub settings: SessionSettings,
}

/// Per-session choices the CLI takes at launch (`--effort`, `--settings`). Changing one means
/// restarting the session, resuming the conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSettings {
    /// `low`, `medium`, `high`, `xhigh` or `max`; the model's default when empty.
    pub effort: Option<String>,
    pub fast_mode: bool,
    /// Extended thinking; `false` disables it (`alwaysThinkingEnabled: false`).
    pub thinking: bool,
    /// An output style name from the CLI's list; `default` when empty.
    pub output_style: Option<String>,
    /// Continue on another model when a message is flagged, instead of asking each time.
    pub switch_models_on_flag: Option<bool>,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self { effort: None, fast_mode: false, thinking: true, output_style: None, switch_models_on_flag: None }
    }
}

impl SessionSettings {
    /// The `--settings` JSON for these choices; `None` when everything is at its default.
    pub fn settings_json(&self) -> Option<Value> {
        let mut o = serde_json::Map::new();
        if self.fast_mode {
            o.insert("fastMode".into(), json!(true));
        }
        if !self.thinking {
            o.insert("alwaysThinkingEnabled".into(), json!(false));
        }
        if let Some(st) = self.output_style.as_deref().filter(|s| !s.is_empty() && *s != "default") {
            o.insert("outputStyle".into(), json!(st));
        }
        if let Some(b) = self.switch_models_on_flag {
            o.insert("switchModelsOnFlag".into(), json!(b));
        }
        (!o.is_empty()).then_some(Value::Object(o))
    }

    /// Extra command-line arguments for `claude`.
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(e) = self.effort.as_deref().filter(|e| !e.is_empty()) {
            a.extend(["--effort".to_string(), e.to_string()]);
        }
        if let Some(j) = self.settings_json() {
            a.extend(["--settings".to_string(), j.to_string()]);
        }
        a
    }
}

pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// A selectable model, from the CLI's catalog (the `initialize` reply).
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// The value for `--model` / `set_model`; `None` is the CLI's default.
    pub value: Option<String>,
    pub label: String,
    pub description: String,
    pub effort_levels: Vec<String>,
    pub fast_mode: bool,
    pub disabled: bool,
}

pub fn parse_models(v: &Value) -> Vec<ModelInfo> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let value = m["value"].as_str()?;
                    let label = m["displayName"].as_str().filter(|s| !s.is_empty()).unwrap_or(value).to_string();
                    Some(ModelInfo {
                        value: (value != "default").then(|| value.to_string()),
                        label,
                        description: m["description"].as_str().unwrap_or("").to_string(),
                        effort_levels: m["supportedEffortLevels"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
                        fast_mode: m["supportsFastMode"].as_bool().unwrap_or(false),
                        disabled: m["disabled"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Who is signed in, from the `initialize` reply.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub organization: Option<String>,
    pub plan: Option<String>,
}

/// What `initialize` told us about this session.
#[derive(Clone, Debug, Default)]
pub struct SessionInit {
    pub models: Vec<ModelInfo>,
    pub output_styles: Vec<String>,
    pub account: AccountInfo,
    /// Slash commands (built-in, skills, plugins) the CLI accepts in a message.
    pub commands: Vec<SlashCommand>,
}

/// A slash command as the CLI lists it in the `initialize` reply.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    /// The argument placeholder, e.g. `[on|off]`.
    pub hint: String,
    pub aliases: Vec<String>,
}

impl SlashCommand {
    /// The command's first line of help, short enough for one row.
    pub fn summary(&self) -> String {
        let line = self.description.lines().next().unwrap_or("").trim();
        let mut out: String = line.chars().take(90).collect();
        if out.len() < line.len() {
            out.push('…');
        }
        out
    }
}

pub fn parse_commands(v: &Value) -> Vec<SlashCommand> {
    let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let name = c["name"].as_str()?.to_string();
                    // Internal hooks for server-launched sessions, not for typing.
                    if name.starts_with("__") {
                        return None;
                    }
                    Some(SlashCommand {
                        name,
                        description: c["description"].as_str().unwrap_or("").to_string(),
                        hint: c["argumentHint"].as_str().unwrap_or("").to_string(),
                        aliases: strings(&c["aliases"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_init(v: &Value) -> SessionInit {
    SessionInit {
        models: parse_models(&v["models"]),
        output_styles: v["available_output_styles"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
        account: AccountInfo {
            email: v["account"]["email"].as_str().map(String::from),
            organization: v["account"]["organization"].as_str().map(String::from),
            plan: v["account"]["subscriptionType"].as_str().map(String::from),
        },
        commands: parse_commands(&v["commands"]),
    }
}

/// One usage meter from `get_usage` (the server's rows, shown as sent).
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    pub label: String,
    pub percent: f32,
    pub resets_at: Option<String>,
    pub severity: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub rows: Vec<UsageRow>,
    pub session_cost_usd: Option<f64>,
    pub plan: Option<String>,
}

pub fn parse_usage(v: &Value) -> Usage {
    // The server's meter rows (`rate_limits.limits`), named by kind.
    let rows = v["rate_limits"]["limits"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let percent = r["percent"].as_f64()?;
                    let kind = r["kind"].as_str().unwrap_or("");
                    let scope = r["scope"]["model"]["display_name"].as_str().or_else(|| r["scope"]["surface"]["display_name"].as_str());
                    let label = match (kind, scope) {
                        ("session", _) => "Current session (5 hours)".to_string(),
                        ("weekly_all", _) => "This week (all models)".to_string(),
                        (_, Some(sc)) if kind.starts_with("weekly") => format!("This week ({sc})"),
                        (_, Some(sc)) => format!("{} ({sc})", r["label"].as_str().unwrap_or(kind)),
                        (_, None) => r["label"].as_str().unwrap_or(kind).to_string(),
                    };
                    Some(UsageRow {
                        label,
                        percent: percent as f32,
                        resets_at: r["resets_at"].as_str().map(String::from),
                        severity: r["severity"].as_str().unwrap_or("normal").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Usage { rows, session_cost_usd: v["session"]["total_cost_usd"].as_f64(), plan: v["subscription_type"].as_str().map(String::from) }
}

/// `get_status`: the terminal's /status screen as sections of (label, value) rows.
pub fn parse_status(v: &Value) -> Vec<(String, Vec<(String, String)>)> {
    v["sections"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|sec| {
                    let rows = sec["rows"]
                        .as_array()
                        .map(|r| r.iter().map(|x| (x["label"].as_str().unwrap_or("").to_string(), x["value"].as_str().unwrap_or("").to_string())).collect())
                        .unwrap_or_default();
                    (sec["title"].as_str().unwrap_or("").to_string(), rows)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A picture attached to a prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageData {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl ImageData {
    pub fn block(&self) -> Value {
        use base64::Engine;
        json!({ "type": "image", "source": { "type": "base64", "media_type": self.media_type, "data": base64::engine::general_purpose::STANDARD.encode(&self.bytes) } })
    }
}

/// What a control request was for, so its reply can be routed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Control {
    Initialize,
    Usage,
    Status,
    Export,
    Other,
}

/// A server's answer to one of our control requests.
pub enum Reply {
    Init(SessionInit),
    Usage(Usage),
    Status(Vec<(String, Vec<(String, String)>)>),
    /// The conversation as text, and the file name /export would offer.
    Export { text: String, filename: String },
    Error(Control, String),
}

pub struct Session {
    child: Child,
    tx: Sender<String>,
    rx: Receiver<Option<Value>>,
    stderr: Arc<Mutex<Vec<String>>>,
    next_req: u64,
    pub exited: bool,
    /// Our control requests waiting for an answer, by request id.
    pending: HashMap<String, Control>,
}

impl Session {
    pub fn start(bin: &Path, cwd: &Path, opts: &Options, ctx: egui::Context) -> Result<Session, String> {
        let mut cmd = Command::new(bin);
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            &opts.permission_mode,
        ]);
        if let Some(m) = opts.model.as_deref().filter(|m| !m.is_empty()) {
            cmd.args(["--model", m]);
        }
        if let Some(id) = &opts.resume {
            cmd.args(["--resume", id]);
        }
        cmd.args(opts.settings.args());
        let mut child = cmd
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", bin.display()))?;

        let mut stdin = child.stdin.take().unwrap();
        let (tx, wrx) = channel::<String>();
        std::thread::spawn(move || {
            for line in wrx {
                if stdin.write_all(line.as_bytes()).is_err() || stdin.write_all(b"\n").is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });

        let stdout = child.stdout.take().unwrap();
        let (rtx, rx) = channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if rtx.send(Some(v)).is_err() {
                        return;
                    }
                    repaint.request_repaint();
                }
            }
            let _ = rtx.send(None);
            repaint.request_repaint();
        });

        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let mut stderr = child.stderr.take().unwrap();
        let sink = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                let lines: Vec<String> = text.lines().rev().take(20).map(String::from).collect();
                *sink.lock().unwrap() = lines.into_iter().rev().collect();
                if text.len() > 64 * 1024 {
                    text = text.split_off(text.len() - 32 * 1024);
                }
            }
        });

        let mut s = Session { child, tx, rx, stderr: stderr_buf, next_req: 1, exited: false, pending: HashMap::new() };
        // Like the SDK: the handshake's reply lists the models, output styles and account.
        s.control(json!({ "subtype": "initialize" }), Control::Initialize);
        Ok(s)
    }

    fn write(&self, v: &Value) {
        let _ = self.tx.send(v.to_string());
    }

    /// A prompt, with any pictures first and the text last.
    pub fn send_user_with(&self, text: &str, images: &[ImageData]) {
        self.write(&user_message(text, images));
    }

    fn control(&mut self, request: Value, kind: Control) {
        let id = format!("rim-{}", self.next_req);
        self.next_req += 1;
        self.pending.insert(id.clone(), kind);
        self.write(&json!({ "type": "control_request", "request_id": id, "request": request }));
    }

    pub fn interrupt(&mut self) {
        self.control(json!({ "subtype": "interrupt" }), Control::Other);
    }

    pub fn set_permission_mode(&mut self, mode: &str) {
        self.control(json!({ "subtype": "set_permission_mode", "mode": mode }), Control::Other);
    }

    pub fn set_model(&mut self, model: &str) {
        self.control(json!({ "subtype": "set_model", "model": model }), Control::Other);
    }

    /// Ask for the plan's usage meters and this session's cost.
    pub fn request_usage(&mut self) {
        self.control(json!({ "subtype": "get_usage", "skip_behaviors": true }), Control::Usage);
    }

    /// Ask for the /status rows (version, account, model, servers…).
    pub fn request_status(&mut self) {
        self.control(json!({ "subtype": "get_status" }), Control::Status);
    }

    /// Ask for the conversation as text, as /export writes it.
    pub fn request_export(&mut self) {
        self.control(json!({ "subtype": "export_conversation" }), Control::Export);
    }

    /// If `msg` answers one of our control requests, the parsed answer.
    pub fn reply(&mut self, msg: &Value) -> Option<Reply> {
        if msg["type"] != "control_response" {
            return None;
        }
        let r = &msg["response"];
        let kind = self.pending.remove(r["request_id"].as_str()?)?;
        if kind == Control::Other {
            return None;
        }
        if r["subtype"] == "error" {
            return Some(Reply::Error(kind, r["error"].as_str().unwrap_or("error").to_string()));
        }
        let v = &r["response"];
        Some(match kind {
            Control::Initialize => Reply::Init(parse_init(v)),
            Control::Usage => Reply::Usage(parse_usage(v)),
            Control::Status => Reply::Status(parse_status(v)),
            Control::Export => Reply::Export { text: v["text"].as_str().unwrap_or("").to_string(), filename: v["default_filename"].as_str().unwrap_or("conversation.txt").to_string() },
            Control::Other => return None,
        })
    }

    /// Answer a `can_use_tool` request.
    pub fn respond(&self, request_id: &str, response: Value) {
        self.write(&json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": response },
        }));
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().map(|l| l.join("\n")).unwrap_or_default()
    }

    /// Drain pending output. Sets `exited` when the process ends.
    pub fn poll(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Some(v) => out.push(v),
                None => {
                    self.exited = true;
                    break;
                }
            }
        }
        out
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The stream-json line for a user turn.
pub fn user_message(text: &str, images: &[ImageData]) -> Value {
    let mut content: Vec<Value> = images.iter().map(ImageData::block).collect();
    content.push(json!({ "type": "text", "text": text }));
    json!({ "type": "user", "message": { "role": "user", "content": content } })
}

// ---------- conversation model ----------

#[derive(Clone, Debug, PartialEq)]
pub enum PermState {
    Pending,
    Allowed,
    Denied,
}

#[derive(Clone, Debug)]
pub struct Perm {
    pub request_id: String,
    pub state: PermState,
    pub suggestions: Vec<Value>,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub input: Value,
    partial: String,
    pub result: Option<ToolResult>,
    pub perm: Option<Perm>,
}

impl Tool {
    fn new(id: &str, name: &str, input: Value) -> Self {
        Self { id: id.into(), name: name.into(), input, partial: String::new(), result: None, perm: None }
    }

    /// File the tool works on, if any.
    pub fn path(&self) -> Option<&str> {
        ["file_path", "notebook_path", "path"].iter().find_map(|k| self.input[*k].as_str())
    }

    /// Rough size of the input, cheap to compute every frame (changes while it streams in).
    pub fn input_size(&self) -> usize {
        self.partial.len().max(self.input.as_object().map(|o| o.len()).unwrap_or(0))
    }

    pub fn is_pending(&self) -> bool {
        self.perm.as_ref().is_some_and(|p| p.state == PermState::Pending)
    }
}

/// A picture the user sent, for the transcript view.
#[derive(Clone)]
pub struct UserImage {
    pub name: String,
    pub width: usize,
    pub height: usize,
    /// A thumbnail, for images attached in this run (not for ones read back from a transcript).
    pub texture: Option<egui::TextureHandle>,
}

impl std::fmt::Debug for UserImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UserImage({} {}x{})", self.name, self.width, self.height)
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    User { text: String, context: Option<String>, images: Vec<UserImage> },
    Text(String),
    Thinking(String),
    Tool(Tool),
    Notice { text: String, error: bool },
}

/// Things the host application should react to.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// A tool wrote this file.
    FileTouched(PathBuf),
    /// A command ran that may have changed files.
    MaybeChanged,
    /// A tool is waiting for the user's approval.
    PermissionNeeded,
    TurnDone,
}

#[derive(Default)]
pub struct Conversation {
    pub items: Vec<Item>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub busy: bool,
    pub status: String,
    pub usage_5h: Option<f32>,
    pub usage_7d: Option<f32>,
    /// Slash commands whose UI only works in the terminal (from the session's init message).
    pub terminal_only: Vec<String>,
    streamed: HashSet<String>,
    blocks: HashMap<usize, usize>,
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                Some("image") => "[image]".to_string(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Remove `<tag>…</tag>` blocks the IDE integration adds to prompts.
pub fn strip_ide_tags(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["ide_opened_file", "ide_selection", "ide_diagnostics", "system-reminder"] {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        while let Some(a) = out.find(&open) {
            match out[a..].find(&close) {
                Some(b) => out.replace_range(a..a + b + close.len(), ""),
                None => {
                    out.truncate(a);
                    break;
                }
            }
        }
    }
    out.trim().to_string()
}

impl Conversation {
    pub fn tool_mut(&mut self, id: &str) -> Option<&mut Tool> {
        self.items.iter_mut().rev().find_map(|i| match i {
            Item::Tool(t) if t.id == id => Some(t),
            _ => None,
        })
    }

    pub fn perm_tool_mut(&mut self, request_id: &str) -> Option<&mut Tool> {
        self.items.iter_mut().rev().find_map(|i| match i {
            Item::Tool(t) if t.perm.as_ref().is_some_and(|p| p.request_id == request_id) => Some(t),
            _ => None,
        })
    }

    pub fn notice(&mut self, text: impl Into<String>, error: bool) {
        self.items.push(Item::Notice { text: text.into(), error });
    }

    /// Mark anything still waiting for approval as denied (turn ended or was interrupted).
    pub fn deny_pending(&mut self) {
        for i in &mut self.items {
            if let Item::Tool(t) = i
                && let Some(p) = &mut t.perm
                && p.state == PermState::Pending
            {
                p.state = PermState::Denied;
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        self.items.iter().any(|i| matches!(i, Item::Tool(t) if t.is_pending()))
    }

    pub fn handle(&mut self, msg: &Value) -> Vec<Effect> {
        let mut fx = Vec::new();
        let ty = msg["type"].as_str().unwrap_or("");
        // Sub-agent chatter is summarized by the parent's tool card.
        if !msg["parent_tool_use_id"].is_null() && ty != "control_request" {
            return fx;
        }
        match ty {
            "system" => match msg["subtype"].as_str() {
                Some("init") => {
                    self.session_id = msg["session_id"].as_str().map(String::from);
                    self.model = msg["model"].as_str().map(String::from);
                    if let Some(a) = msg["terminal_slash_commands"].as_array() {
                        self.terminal_only = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                    }
                }
                Some("status") => self.status = msg["status"].as_str().unwrap_or("").to_string(),
                _ => {}
            },
            "stream_event" => self.stream_event(&msg["event"]),
            "assistant" => self.assistant(&msg["message"]),
            "user" => self.user(msg, &mut fx),
            "control_request" => self.control_request(msg, &mut fx),
            "rate_limit_event" => {
                let w = &msg["rate_limit_info"]["unifiedWindows"];
                self.usage_5h = w["five_hour"]["utilization"].as_f64().map(|v| v as f32);
                self.usage_7d = w["seven_day"]["utilization"].as_f64().map(|v| v as f32);
            }
            "result" => {
                self.busy = false;
                self.status.clear();
                self.deny_pending();
                let failed = msg["is_error"].as_bool().unwrap_or(false) || msg["subtype"].as_str().is_some_and(|s| s != "success");
                if failed {
                    let text = msg["result"].as_str().filter(|s| !s.is_empty()).unwrap_or("The request failed.");
                    // An interrupt ends the turn with an error result; that is not worth a banner.
                    if !text.contains("interrupted") {
                        self.notice(text, true);
                    }
                }
                fx.push(Effect::TurnDone);
            }
            _ => {}
        }
        fx
    }

    fn stream_event(&mut self, ev: &Value) {
        match ev["type"].as_str() {
            Some("message_start") => {
                if let Some(id) = ev["message"]["id"].as_str() {
                    self.streamed.insert(id.to_string());
                }
                self.blocks.clear();
            }
            Some("content_block_start") => {
                let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                let b = &ev["content_block"];
                let item = match b["type"].as_str() {
                    Some("text") => Item::Text(b["text"].as_str().unwrap_or("").to_string()),
                    Some("thinking") => Item::Thinking(String::new()),
                    Some("tool_use") => Item::Tool(Tool::new(b["id"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or(""), Value::Null)),
                    _ => return,
                };
                self.items.push(item);
                self.blocks.insert(idx, self.items.len() - 1);
            }
            Some("content_block_delta") => {
                let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                let Some(&at) = self.blocks.get(&idx) else { return };
                let d = &ev["delta"];
                match (&mut self.items[at], d["type"].as_str()) {
                    (Item::Text(t), Some("text_delta")) => t.push_str(d["text"].as_str().unwrap_or("")),
                    (Item::Thinking(t), Some("thinking_delta")) => t.push_str(d["thinking"].as_str().unwrap_or("")),
                    (Item::Tool(t), Some("input_json_delta")) => {
                        t.partial.push_str(d["partial_json"].as_str().unwrap_or(""));
                        if let Ok(v) = serde_json::from_str(&t.partial) {
                            t.input = v;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn assistant(&mut self, message: &Value) {
        let streamed = message["id"].as_str().is_some_and(|id| self.streamed.contains(id));
        let Some(blocks) = message["content"].as_array() else { return };
        for b in blocks {
            match b["type"].as_str() {
                Some("tool_use") => {
                    let (id, name) = (b["id"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or(""));
                    match self.tool_mut(id) {
                        Some(t) => t.input = b["input"].clone(),
                        None => self.items.push(Item::Tool(Tool::new(id, name, b["input"].clone()))),
                    }
                }
                // Streaming already showed these; the final message would duplicate them.
                Some("text") if !streamed => {
                    let text = b["text"].as_str().unwrap_or("");
                    if !text.trim().is_empty() {
                        self.items.push(Item::Text(text.to_string()));
                    }
                }
                Some("thinking") if !streamed => {
                    let text = b["thinking"].as_str().unwrap_or("");
                    if !text.trim().is_empty() {
                        self.items.push(Item::Thinking(text.to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    fn user(&mut self, msg: &Value, fx: &mut Vec<Effect>) {
        if msg["isMeta"].as_bool().unwrap_or(false) {
            return;
        }
        let content = &msg["message"]["content"];
        let blocks: Vec<Value> = match content {
            Value::String(s) => vec![json!({ "type": "text", "text": s })],
            Value::Array(a) => a.clone(),
            _ => return,
        };
        let images: Vec<UserImage> = blocks
            .iter()
            .filter(|b| b["type"] == "image")
            .enumerate()
            .map(|(k, b)| {
                let kind = b["source"]["media_type"].as_str().and_then(|m| m.strip_prefix("image/")).unwrap_or("image").to_uppercase();
                UserImage { name: format!("{kind} image {}", k + 1), width: 0, height: 0, texture: None }
            })
            .collect();
        let mut images = Some(images);
        for b in &blocks {
            match b["type"].as_str() {
                Some("tool_result") => {
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    let result = ToolResult { text: tool_result_text(&b["content"]), is_error: b["is_error"].as_bool().unwrap_or(false) };
                    let Some(t) = self.tool_mut(id) else { continue };
                    let ok = !result.is_error;
                    let (name, path) = (t.name.clone(), t.path().map(PathBuf::from));
                    t.result = Some(result);
                    if let Some(p) = t.perm.as_mut()
                        && p.state == PermState::Pending
                    {
                        p.state = PermState::Allowed;
                    }
                    if ok {
                        match (name.as_str(), path) {
                            ("Edit" | "Write" | "NotebookEdit" | "MultiEdit", Some(p)) => fx.push(Effect::FileTouched(p)),
                            ("Bash" | "PowerShell", _) => fx.push(Effect::MaybeChanged),
                            _ => {}
                        }
                    }
                }
                Some("text") => {
                    let text = strip_ide_tags(b["text"].as_str().unwrap_or(""));
                    if !text.is_empty() && !text.starts_with("<command-") && !text.starts_with("<local-command") {
                        self.items.push(Item::User { text, context: None, images: images.take().unwrap_or_default() });
                    }
                }
                _ => {}
            }
        }
    }

    fn control_request(&mut self, msg: &Value, fx: &mut Vec<Effect>) {
        let req = &msg["request"];
        if req["subtype"].as_str() != Some("can_use_tool") {
            return;
        }
        let perm = Perm {
            request_id: msg["request_id"].as_str().unwrap_or("").to_string(),
            state: PermState::Pending,
            suggestions: req["permission_suggestions"].as_array().cloned().unwrap_or_default(),
            description: req["description"].as_str().unwrap_or("").to_string(),
        };
        let id = req["tool_use_id"].as_str().unwrap_or("");
        match self.tool_mut(id) {
            Some(t) => {
                if t.input.is_null() || t.input.as_object().is_some_and(|o| o.is_empty()) {
                    t.input = req["input"].clone();
                }
                t.perm = Some(perm);
            }
            None => {
                let mut t = Tool::new(id, req["tool_name"].as_str().unwrap_or("tool"), req["input"].clone());
                t.perm = Some(perm);
                self.items.push(Item::Tool(t));
            }
        }
        fx.push(Effect::PermissionNeeded);
    }

    /// Rebuild the display from a saved session transcript (`~/.claude/projects/…/<id>.jsonl`).
    pub fn load_transcript(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let mut fx = Vec::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if v["isSidechain"].as_bool().unwrap_or(false) {
                continue;
            }
            match v["type"].as_str() {
                Some("assistant") => self.assistant(&v["message"]),
                Some("user") => self.user(&v, &mut fx),
                _ => {}
            }
        }
        self.session_id = path.file_stem().map(|s| s.to_string_lossy().to_string());
    }
}

pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub modified: SystemTime,
}

/// The directory Claude Code keeps a project's transcripts in.
pub fn project_dir(root: &Path) -> Option<PathBuf> {
    // Claude names the folder after the real working directory: no trailing slash, symlinks resolved.
    let real = root.canonicalize().unwrap_or_else(|_| root.components().collect());
    let name: String = real.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Some(dirs::home_dir()?.join(".claude/projects").join(name))
}

pub fn list_sessions(root: &Path) -> Vec<SessionInfo> {
    match project_dir(root) {
        Some(dir) => list_sessions_in(&dir),
        None => vec![],
    }
}

/// Every saved conversation in `dir`, newest first. A conversation is listed as long as it
/// contains a user message, even if a nice title can't be worked out.
pub fn list_sessions_in(dir: &Path) -> Vec<SessionInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<SessionInfo> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let modified = e.metadata().ok()?.modified().ok()?;
            let head = {
                let mut f = std::fs::File::open(&path).ok()?;
                let mut buf = vec![0u8; 96 * 1024];
                let n = f.read(&mut buf).ok()?;
                String::from_utf8_lossy(&buf[..n]).into_owned()
            };
            // Even a huge first line (which we can't parse) proves the conversation has content.
            let has_user = head.contains("\"type\":\"user\"");
            let title = head.lines().find_map(|l| {
                let v: Value = serde_json::from_str(l).ok()?;
                if v["type"] != "user" || v["isMeta"].as_bool().unwrap_or(false) || v["isSidechain"].as_bool().unwrap_or(false) {
                    return None;
                }
                // A prompt can be several text parts: the IDE note first, then what the user typed.
                let raw = match &v["message"]["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a.iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap_or("")).collect::<Vec<_>>().join("\n"),
                    _ => return None,
                };
                let t = strip_ide_tags(&raw);
                (!t.is_empty() && !t.starts_with("<command-") && !t.starts_with("<local-command")).then(|| t.lines().next().unwrap_or("").chars().take(90).collect::<String>())
            });
            let title = title.or_else(|| has_user.then(|| "Untitled conversation".to_string()))?;
            Some(SessionInfo { id: path.file_stem()?.to_string_lossy().to_string(), path, title, modified })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out.truncate(60);
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn feed(c: &mut Conversation, lines: &[&str]) -> Vec<Effect> {
        lines.iter().flat_map(|l| c.handle(&serde_json::from_str(l).unwrap())).collect()
    }

    #[test]
    fn streamed_text_is_not_duplicated_by_the_final_message() {
        let mut c = Conversation::default();
        feed(&mut c, &[
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"m"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg1"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}}"#,
            r#"{"type":"assistant","message":{"id":"msg1","content":[{"type":"text","text":"Hello"}]}}"#,
        ]);
        assert_eq!(c.session_id.as_deref(), Some("s1"));
        assert_eq!(c.items.len(), 1);
        assert!(matches!(&c.items[0], Item::Text(t) if t == "Hello"));
    }

    #[test]
    fn non_streamed_messages_still_show() {
        let mut c = Conversation::default();
        feed(&mut c, &[r#"{"type":"assistant","message":{"id":"m","content":[{"type":"thinking","thinking":""},{"type":"text","text":"Hi"}]}}"#]);
        assert_eq!(c.items.len(), 1);
    }

    #[test]
    fn tool_call_permission_and_result() {
        let mut c = Conversation::default();
        let fx = feed(&mut c, &[
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Write","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/w/a.txt\","}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"content\":\"hi\"}"}}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/w/a.txt","content":"hi"}}]}}"#,
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/w/a.txt","content":"hi"},"description":"a.txt","permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],"tool_use_id":"t1"}}"#,
        ]);
        assert_eq!(fx, vec![Effect::PermissionNeeded]);
        assert_eq!(c.items.len(), 1);
        assert!(c.has_pending());
        let Item::Tool(t) = &c.items[0] else { panic!() };
        assert_eq!(t.path(), Some("/w/a.txt"));
        assert_eq!(t.perm.as_ref().unwrap().suggestions.len(), 1);

        let fx = feed(&mut c, &[r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"File created"}]}}"#]);
        assert_eq!(fx, vec![Effect::FileTouched(PathBuf::from("/w/a.txt"))]);
        assert!(!c.has_pending());
        let Item::Tool(t) = &c.items[0] else { panic!() };
        assert_eq!(t.result.as_ref().unwrap().text, "File created");
    }

    #[test]
    fn permission_request_without_prior_stream_creates_a_tool() {
        let mut c = Conversation::default();
        feed(&mut c, &[r#"{"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"tb"}}"#]);
        assert!(c.has_pending());
        c.deny_pending();
        assert!(!c.has_pending());
    }

    #[test]
    fn result_ends_the_turn_and_reports_errors() {
        let mut c = Conversation { busy: true, ..Default::default() };
        let fx = feed(&mut c, &[r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in"}"#]);
        assert_eq!(fx, vec![Effect::TurnDone]);
        assert!(!c.busy);
        assert!(matches!(c.items.last(), Some(Item::Notice { error: true, .. })));
    }

    #[test]
    fn sub_agent_messages_are_hidden() {
        let mut c = Conversation::default();
        feed(&mut c, &[r#"{"type":"assistant","parent_tool_use_id":"x","message":{"id":"m","content":[{"type":"text","text":"inner"}]}}"#]);
        assert!(c.items.is_empty());
    }

    #[test]
    fn usage_windows_are_tracked() {
        let mut c = Conversation::default();
        feed(&mut c, &[r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.84},"seven_day":{"utilization":0.12}}}}"#]);
        assert_eq!(c.usage_5h, Some(0.84));
    }

    #[test]
    fn ide_tags_are_stripped_from_prompts() {
        let t = "<ide_opened_file>The user opened a.rs</ide_opened_file>\n<ide_selection>x</ide_selection>\nfix this";
        assert_eq!(strip_ide_tags(t), "fix this");
        assert_eq!(strip_ide_tags("<system-reminder>unterminated"), "");
    }

    #[test]
    fn project_folder_name_ignores_trailing_slashes() {
        let with = project_dir(Path::new("/nonexistent/user/proj/")).unwrap();
        let without = project_dir(Path::new("/nonexistent/user/proj")).unwrap();
        assert_eq!(with, without);
        assert!(with.ends_with("-nonexistent-user-proj"));
        // A nonexistent path with `..` or `.` parts is tidied too.
        assert_eq!(project_dir(Path::new("/nope/a/./b/")).unwrap(), project_dir(Path::new("/nope/a/b")).unwrap());
    }

    #[test]
    fn history_lists_every_conversation_that_has_a_user_message() {
        let dir = std::env::temp_dir().join(format!("rim-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = |name: &str, lines: &[&str]| std::fs::write(dir.join(name), lines.join("\n")).unwrap();
        // Typed after the IDE note, as separate text parts (what the VS Code extension writes).
        w("a.jsonl", &[r#"{"type":"queue-operation"}"#, r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>x</ide_opened_file>"},{"type":"text","text":"make the map look like miami"}]}}"#]);
        // rim's own format: note and prompt in one part.
        w("b.jsonl", &[r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>x</ide_opened_file>\nfix the bug\nsecond line"}]}}"#]);
        // Nothing usable for a title, but it is a real conversation.
        w("c.jsonl", &[r#"{"type":"user","message":{"role":"user","content":"<command-name>/model</command-name>"}}"#]);
        // No user message at all: not listed.
        w("d.jsonl", &[r#"{"type":"queue-operation"}"#]);
        // Sub-agent chatter and non-transcript files are ignored.
        w("e.jsonl", &[r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"inner"}}"#]);
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let mut titles: Vec<String> = list_sessions_in(&dir).into_iter().map(|s| s.title).collect();
        titles.sort();
        assert_eq!(titles, vec!["Untitled conversation", "Untitled conversation", "fix the bug", "make the map look like miami"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn transcript_loading() {
        let dir = std::env::temp_dir().join(format!("rim-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc-123.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"<ide_opened_file>a</ide_opened_file>\nhello"}}"#,
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"Hi there"},{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"/a"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"text","text":"data"}]}]}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"ignored"}}"#,
            r#"{"type":"summary","summary":"x"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let mut c = Conversation::default();
        c.load_transcript(&path);
        assert_eq!(c.session_id.as_deref(), Some("abc-123"));
        assert!(matches!(&c.items[0], Item::User { text, .. } if text == "hello"));
        assert!(matches!(&c.items[1], Item::Text(t) if t == "Hi there"));
        let Item::Tool(t) = &c.items[2] else { panic!() };
        assert_eq!(t.result.as_ref().unwrap().text, "data");
        assert_eq!(c.items.len(), 3);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn auth_status_parsing() {
        let real = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "a@b.c", "subscriptionType": "pro", "orgName": "x"}"#;
        assert_eq!(parse_auth_status(real), Some(AuthStatus { logged_in: true, email: Some("a@b.c".into()), plan: Some("pro".into()), method: Some("claude.ai".into()) }));
        assert_eq!(parse_auth_status(r#"{"loggedIn": false}"#), Some(AuthStatus::default()));
        assert_eq!(parse_auth_status("not json"), None);
    }

    /// A stand-in `claude` that supports `auth status`, `auth login` and `auth logout`.
    pub fn fake_auth_cli(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import sys, os, json
state = os.path.join(os.path.dirname(os.path.abspath(__file__)), "logged_in")
a = sys.argv[1:]
if a[:2] == ["auth", "status"]:
    print(json.dumps({"loggedIn": os.path.exists(state), "email": "me@example.test", "subscriptionType": "pro", "authMethod": "claude.ai"}))
elif a[:2] == ["auth", "login"]:
    print("Opening browser to sign in…")
    print("If the browser didn't open, visit: https://example.test/authorize?x=1&y=2")
    sys.stdout.write("Paste code here if prompted > "); sys.stdout.flush()
    code = sys.stdin.readline().strip()
    if code == "good#code":
        open(state, "w").close(); print("Login successful"); sys.exit(0)
    print("Invalid code", file=sys.stderr); sys.exit(1)
elif a[:2] == ["auth", "logout"]:
    if os.path.exists(state): os.remove(state)
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    fn wait_login(l: &mut Login) -> bool {
        for _ in 0..200 {
            if let Some(ok) = l.poll() {
                return ok;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("login did not finish");
    }

    #[test]
    fn login_flow_with_a_fake_cli() {
        let dir = std::env::temp_dir().join(format!("rim-fake-auth-{}", std::process::id()));
        let bin = fake_auth_cli(&dir);
        assert!(!auth_status(&bin).unwrap().logged_in);

        let mut bad = Login::start(&bin, false, egui::Context::default()).unwrap();
        for _ in 0..200 {
            if bad.url().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(bad.url().as_deref(), Some("https://example.test/authorize?x=1&y=2"));
        bad.submit_code("wrong");
        assert!(!wait_login(&mut bad));
        assert!(bad.output().contains("Invalid code"));

        let mut good = Login::start(&bin, true, egui::Context::default()).unwrap();
        good.submit_code("  good#code \n");
        assert!(wait_login(&mut good));
        let st = auth_status(&bin).unwrap();
        assert!(st.logged_in && st.email.as_deref() == Some("me@example.test"));

        auth_logout(&bin).unwrap();
        assert!(!auth_status(&bin).unwrap().logged_in);
        std::fs::remove_dir_all(dir).ok();
    }

    /// A scripted stand-in for the CLI over real pipes: answers the handshake with a model
    /// catalog, asks to run `ls` for a text prompt, echoes attached images, and answers
    /// usage/status/export requests.
    pub fn fake_session_cli(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import sys, json
def out(m): print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if m["type"] == "user":
        imgs = [b for b in m["message"]["content"] if b["type"] == "image"]
        out({"type":"system","subtype":"init","session_id":"fake","model":"fake-model","slash_commands":["compact","code-review","color"],"terminal_slash_commands":["color"]})
        if imgs:
            note = "images=%d media=%s data=%s" % (len(imgs), imgs[0]["source"]["media_type"], imgs[0]["source"]["data"])
            out({"type":"assistant","message":{"id":"img","content":[{"type":"text","text":note}]}})
            out({"type":"result","subtype":"success","is_error":False,"result":note})
            continue
        out({"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"tb"}})
    elif m["type"] == "control_request" and m["request"]["subtype"] == "initialize":
        out({"type":"control_response","response":{"subtype":"success","request_id":m["request_id"],"response":{
            "models":[{"value":"default","displayName":"Default (recommended)","description":"Opus 5.5","supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsFastMode":True},
                      {"value":"claude-fable-5-1","displayName":"Fable 5.1","description":"For your toughest challenges","supportedEffortLevels":["low","medium","high","xhigh","max"]},
                      {"value":"haiku","displayName":"Haiku 4.5","description":"Fastest"},
                      {"value":"old","displayName":"Retired","disabled":True}],
            "output_style":"default","available_output_styles":["default","Concise","Explanatory"],
            "commands":[{"name":"compact","description":"Free up context by summarizing the conversation so far","argumentHint":"<optional custom summarization instructions>","builtin":True},
                        {"name":"code-review","description":"Review the current diff.\nSecond line.","argumentHint":"[low|medium|high]","aliases":["review"],"builtin":True},
                        {"name":"color","description":"Set the prompt bar color","argumentHint":"[red|blue]","builtin":True},
                        {"name":"__remote-workflow","description":"internal","argumentHint":"","builtin":True}],
            "account":{"email":"me@example.test","organization":"Me's Org","subscriptionType":"Claude Pro"}}}})
    elif m["type"] == "control_request" and m["request"]["subtype"] == "get_usage":
        out({"type":"control_response","response":{"subtype":"success","request_id":m["request_id"],"response":{
            "session":{"total_cost_usd":0.25},"subscription_type":"pro",
            "rate_limits":{"five_hour":{"utilization":42},"limits":[
                {"kind":"session","group":"session","percent":42,"resets_at":"2026-09-29T23:00:00Z","severity":"normal","is_active":True,"scope":None},
                {"kind":"weekly_all","group":"weekly","percent":8,"resets_at":"2026-10-03T14:00:00Z","severity":"normal","is_active":False,"scope":None},
                {"kind":"weekly_scoped","group":"weekly","percent":90,"resets_at":None,"scope":{"model":{"display_name":"Fable","id":None},"surface":None},"severity":"warning","is_active":False}]}}}})
    elif m["type"] == "control_request" and m["request"]["subtype"] == "get_status":
        out({"type":"control_response","response":{"subtype":"success","request_id":m["request_id"],"response":{"sections":[{"title":"Session","rows":[{"label":"Version","value":"9.9"},{"value":"a sentence"}]}]}}})
    elif m["type"] == "control_request" and m["request"]["subtype"] == "export_conversation":
        out({"type":"control_response","response":{"subtype":"success","request_id":m["request_id"],"response":{"text":"> go\n\nok","default_filename":"2026-go.txt"}}})
    elif m["type"] == "control_request" and m["request"]["subtype"] == "set_model":
        out({"type":"control_response","response":{"subtype":"error","request_id":m["request_id"],"error":"no such model"}})
    elif m["type"] == "control_response":
        ok = m["response"]["response"]["behavior"] == "allow"
        out({"type":"result","subtype":"success","is_error":False,"result":"allowed" if ok else "denied"})
    elif m["type"] == "control_request" and m["request"]["subtype"] == "interrupt":
        out({"type":"result","subtype":"error_during_execution","is_error":True,"result":"interrupted"})
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    /// Drives a scripted stand-in for the CLI over real pipes: prompt → approval request → answer → result.
    #[test]
    fn session_roundtrip_with_a_fake_cli() {
        let dir = std::env::temp_dir().join(format!("rim-fake-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = fake_session_cli(&dir);
        let opts = Options { model: Some("haiku".into()), permission_mode: "manual".into(), resume: None, settings: SessionSettings::default() };
        let mut s = Session::start(&script, &dir, &opts, egui::Context::default()).unwrap();
        s.send_user_with("go", &[]);
        let mut convo = Conversation::default();
        let replies: std::cell::RefCell<Vec<Reply>> = Default::default();
        let wait = |s: &mut Session, convo: &mut Conversation, pred: &dyn Fn(&Conversation) -> bool| {
            for _ in 0..200 {
                for m in s.poll() {
                    if let Some(r) = s.reply(&m) {
                        replies.borrow_mut().push(r);
                        continue;
                    }
                    convo.handle(&m);
                }
                if pred(convo) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            panic!("timed out");
        };
        wait(&mut s, &mut convo, &|c| c.has_pending());
        // The handshake sent at start was answered: catalog, styles and account.
        let init = replies.borrow_mut().drain(..).find_map(|r| match r { Reply::Init(i) => Some(i), _ => None }).expect("initialize reply");
        let labels: Vec<&str> = init.models.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Default (recommended)", "Fable 5.1", "Haiku 4.5", "Retired"]);
        assert_eq!(init.models[0].value, None);
        assert_eq!(init.models[1].value.as_deref(), Some("claude-fable-5-1"));
        assert!(init.models[0].fast_mode && !init.models[1].fast_mode && init.models[3].disabled);
        assert_eq!(init.output_styles, ["default", "Concise", "Explanatory"]);
        assert_eq!(init.account.email.as_deref(), Some("me@example.test"));
        let names: Vec<&str> = init.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["compact", "code-review", "color"], "internal __ commands are dropped");
        assert_eq!(init.commands[1].aliases, ["review"]);
        assert_eq!(init.commands[1].summary(), "Review the current diff.");
        assert_eq!(convo.terminal_only, ["color"]);
        // Other requests are answered by kind; an error reply names it.
        s.request_usage();
        s.request_status();
        s.request_export();
        s.set_model("nope");
        wait(&mut s, &mut convo, &|_| replies.borrow().len() >= 3);
        let got = replies.borrow_mut().drain(..).collect::<Vec<_>>();
        let usage = got.iter().find_map(|r| match r { Reply::Usage(u) => Some(u.clone()), _ => None }).unwrap();
        assert_eq!(usage.rows.len(), 3);
        assert_eq!((usage.rows[0].label.as_str(), usage.rows[0].percent), ("Current session (5 hours)", 42.0));
        assert_eq!(usage.rows[1].label, "This week (all models)");
        assert_eq!((usage.rows[2].label.as_str(), usage.rows[2].severity.as_str(), usage.rows[2].resets_at.as_deref()), ("This week (Fable)", "warning", None));
        assert_eq!(usage.session_cost_usd, Some(0.25));
        let status = got.iter().find_map(|r| match r { Reply::Status(st) => Some(st.clone()), _ => None }).unwrap();
        assert_eq!(status, vec![("Session".to_string(), vec![("Version".to_string(), "9.9".to_string()), (String::new(), "a sentence".to_string())])]);
        assert!(got.iter().any(|r| matches!(r, Reply::Export { text, filename } if text == "> go\n\nok" && filename == "2026-go.txt")));
        // `set_model` answers aren't surfaced (the CLI reports the model in its next init message).
        assert!(!got.iter().any(|r| matches!(r, Reply::Error(..))));
        assert_eq!(convo.model.as_deref(), Some("fake-model"));
        s.respond("q1", json!({ "behavior": "allow", "updatedInput": { "command": "ls" } }));
        convo.busy = true;
        wait(&mut s, &mut convo, &|c| !c.busy);
        s.interrupt();
        wait(&mut s, &mut convo, &|c| !c.items.is_empty() && !c.busy);
        // A prompt with a picture: the image block goes first, base64-encoded.
        let img = ImageData { media_type: "image/png".into(), bytes: vec![1, 2, 3] };
        convo.busy = true;
        convo.items.clear();
        s.send_user_with("what is this", &[img]);
        wait(&mut s, &mut convo, &|c| !c.busy);
        drop(s);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn session_settings_become_flags() {
        let d = SessionSettings::default();
        assert!(d.args().is_empty() && d.settings_json().is_none());
        let s = SessionSettings { effort: Some("xhigh".into()), fast_mode: true, thinking: false, output_style: Some("Concise".into()), switch_models_on_flag: Some(true) };
        let a = s.args();
        assert_eq!(&a[..2], ["--effort", "xhigh"]);
        assert_eq!(a[2], "--settings");
        let j: Value = serde_json::from_str(&a[3]).unwrap();
        assert_eq!(j, json!({ "fastMode": true, "alwaysThinkingEnabled": false, "outputStyle": "Concise", "switchModelsOnFlag": true }));
        // `default` style and thinking on are the CLI's defaults: not sent.
        let s = SessionSettings { output_style: Some("default".into()), thinking: true, ..Default::default() };
        assert!(s.settings_json().is_none());
    }

    #[test]
    fn user_message_puts_images_first() {
        let img = ImageData { media_type: "image/jpeg".into(), bytes: b"hello".to_vec() };
        let m = user_message("hi", &[img]);
        let c = m["message"]["content"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["type"], "image");
        assert_eq!(c[0]["source"]["media_type"], "image/jpeg");
        assert_eq!(c[0]["source"]["data"], "aGVsbG8=");
        assert_eq!(c[1], json!({ "type": "text", "text": "hi" }));
        assert_eq!(user_message("x", &[])["message"]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn transcript_user_turns_keep_their_images() {
        let mut c = Conversation::default();
        c.handle(&json!({ "type": "user", "message": { "role": "user", "content": [
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
            { "type": "text", "text": "what is this" }
        ] } }));
        let Item::User { text, images, .. } = &c.items[0] else { panic!("{:?}", c.items) };
        assert_eq!(text, "what is this");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].name, "PNG image 1");
        assert!(images[0].texture.is_none());
    }
}
