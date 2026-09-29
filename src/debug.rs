//! Debug sessions over the Debug Adapter Protocol: launch configuration, breakpoints, the
//! adapter's events (stops, output, exit) turned into stack, scopes and variables to show,
//! and the stepping requests. No UI here, so it can be driven by tests.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dap::{Adapter, Msg, Pending};
use crate::format::find_in_path;

/// Which adapter runs the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// `gdb -i dap` (Rust, C, C++; `rust-gdb` when present for Rust).
    #[default]
    Gdb,
    /// `python -m debugpy.adapter`.
    Debugpy,
    /// `dlv dap` over TCP (Go; builds the package itself).
    Delve,
    /// Any adapter speaking the protocol on stdio, given as `command`.
    Custom,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Gdb, Kind::Debugpy, Kind::Delve, Kind::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Gdb => "gdb (Rust, C, C++)",
            Kind::Debugpy => "debugpy (Python)",
            Kind::Delve => "delve (Go)",
            Kind::Custom => "custom adapter",
        }
    }

    /// The tool the adapter needs on PATH, and how to get it.
    pub fn requirement(self) -> (&'static str, &'static str) {
        match self {
            Kind::Gdb => ("gdb", "install gdb 14 or newer (its DAP mode)"),
            Kind::Debugpy => ("python3", "pip install debugpy"),
            Kind::Delve => ("dlv", "go install github.com/go-delve/delve/cmd/dlv@latest"),
            Kind::Custom => ("", ""),
        }
    }
}

/// What to run; kept in `<root>/.rim/debug.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LaunchConfig {
    pub kind: Kind,
    /// The executable (gdb), script (debugpy) or package directory (delve).
    pub program: String,
    /// Arguments, split like a shell would (quotes allowed).
    pub args: String,
    /// Working directory of the program; the project root when empty.
    pub cwd: String,
    /// A shell command run before launching (`cargo build`, `make`); empty for none.
    pub build: String,
    /// The adapter command for `Kind::Custom`.
    pub command: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Saved {
    launch: LaunchConfig,
    /// File → 1-based lines.
    breakpoints: BTreeMap<String, Vec<usize>>,
}

/// Guess a configuration for the project: Cargo → gdb on the built binary, go.mod → delve,
/// a Python file → debugpy on it.
pub fn detect(root: &Path, active: Option<&Path>) -> LaunchConfig {
    let ext = active.and_then(|p| p.extension()).and_then(|e| e.to_str()).unwrap_or("");
    if ext == "py" {
        return LaunchConfig { kind: Kind::Debugpy, program: active.map(|p| p.display().to_string()).unwrap_or_default(), ..Default::default() };
    }
    if let Ok(toml) = std::fs::read_to_string(root.join("Cargo.toml")) {
        let name = cargo_bin_name(&toml).unwrap_or_default();
        return LaunchConfig { kind: Kind::Gdb, program: format!("target/debug/{name}"), build: "cargo build".into(), ..Default::default() };
    }
    if root.join("go.mod").exists() {
        let dir = active.and_then(|p| p.parent()).filter(|d| d.starts_with(root)).map(|d| d.display().to_string()).unwrap_or_else(|| ".".into());
        return LaunchConfig { kind: Kind::Delve, program: dir, ..Default::default() };
    }
    if root.join("Makefile").exists() {
        return LaunchConfig { kind: Kind::Gdb, build: "make".into(), ..Default::default() };
    }
    LaunchConfig::default()
}

/// The binary a `cargo build` produces: the first `[[bin]]` name, else the package name.
fn cargo_bin_name(toml: &str) -> Option<String> {
    let doc: toml::Table = toml::from_str(toml).ok()?;
    if let Some(name) = doc.get("bin").and_then(|b| b.as_array()).and_then(|a| a.first()).and_then(|b| b.get("name")).and_then(|n| n.as_str()) {
        return Some(name.to_string());
    }
    doc.get("package")?.get("name")?.as_str().map(String::from)
}

/// Split a command line the way a shell would for plain words and quoted strings.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Building,
    Starting,
    Running,
    Stopped,
    Ended,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub id: i64,
    pub name: String,
    pub path: Option<PathBuf>,
    /// 0-based.
    pub line: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Var {
    pub name: String,
    pub value: String,
    pub ty: String,
    pub reference: i64,
    /// `None` until fetched (only for `reference > 0`).
    pub children: Option<Vec<Var>>,
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub name: String,
    pub reference: i64,
    pub expensive: bool,
    pub vars: Option<Vec<Var>>,
    pub loading: bool,
}

/// One console line: `kind` is stdout / stderr / console / error / cmd.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub kind: &'static str,
    pub text: String,
}

/// Things the app should do because of the session.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Show this file with its execution line (0-based).
    Reveal { path: PathBuf, line: usize },
    /// The program stopped or ended: bring the panel up.
    Attention,
}

pub struct Debugger {
    pub config: LaunchConfig,
    /// 0-based lines per file.
    pub breakpoints: BTreeMap<PathBuf, BTreeSet<usize>>,
    /// Lines the adapter could not bind (shown hollow).
    pub unverified: HashMap<PathBuf, BTreeSet<usize>>,
    pub phase: Phase,
    pub reason: String,
    adapter: Option<Adapter>,
    building: Option<Receiver<Result<String, String>>>,
    capabilities: Value,
    pub thread: Option<i64>,
    pub threads: Vec<(i64, String)>,
    pub frames: Vec<Frame>,
    pub frame_sel: usize,
    pub scopes: Vec<Scope>,
    pub console: Vec<Line>,
    pub eval: String,
    effects: Vec<Effect>,
    ctx: Option<egui::Context>,
    root: PathBuf,
}

impl Default for Debugger {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Debugger {
    pub fn new(ctx: Option<egui::Context>) -> Self {
        Self {
            config: LaunchConfig::default(),
            breakpoints: BTreeMap::new(),
            unverified: HashMap::new(),
            phase: Phase::Idle,
            reason: String::new(),
            adapter: None,
            building: None,
            capabilities: Value::Null,
            thread: None,
            threads: vec![],
            frames: vec![],
            frame_sel: 0,
            scopes: vec![],
            console: vec![],
            eval: String::new(),
            effects: vec![],
            ctx,
            root: PathBuf::new(),
        }
    }

    // ---------- configuration ----------

    fn file(root: &Path) -> PathBuf {
        root.join(".rim").join("debug.toml")
    }

    /// Load the saved configuration and breakpoints of a project (a new session state).
    pub fn load(&mut self, root: &Path) {
        self.stop();
        self.root = root.to_path_buf();
        self.breakpoints.clear();
        self.unverified.clear();
        self.config = LaunchConfig::default();
        if let Ok(text) = std::fs::read_to_string(Self::file(root))
            && let Ok(saved) = toml::from_str::<Saved>(&text)
        {
            self.config = saved.launch;
            for (file, lines) in saved.breakpoints {
                let set: BTreeSet<usize> = lines.into_iter().filter(|l| *l > 0).map(|l| l - 1).collect();
                if !set.is_empty() {
                    self.breakpoints.insert(root.join(file), set);
                }
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        if self.root.as_os_str().is_empty() {
            return Ok(());
        }
        let breakpoints = self
            .breakpoints
            .iter()
            .filter(|(_, l)| !l.is_empty())
            .map(|(p, l)| (p.strip_prefix(&self.root).unwrap_or(p).display().to_string(), l.iter().map(|x| x + 1).collect()))
            .collect();
        let saved = Saved { launch: self.config.clone(), breakpoints };
        let text = toml::to_string_pretty(&saved).map_err(|e| e.to_string())?;
        let file = Self::file(&self.root);
        std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&file, text).map_err(|e| e.to_string())
    }

    /// Nothing to run yet: the configuration needs the user.
    pub fn needs_config(&self) -> bool {
        match self.config.kind {
            Kind::Custom => self.config.command.is_empty(),
            _ => self.config.program.trim().is_empty(),
        }
    }

    // ---------- breakpoints ----------

    pub fn toggle_breakpoint(&mut self, path: &Path, line: usize) {
        let set = self.breakpoints.entry(path.to_path_buf()).or_default();
        if !set.remove(&line) {
            set.insert(line);
        }
        if set.is_empty() {
            self.breakpoints.remove(path);
        }
        if self.adapter.is_some() {
            self.send_breakpoints(path);
        }
        let _ = self.save();
    }

    pub fn lines_for(&self, path: &Path) -> Vec<usize> {
        self.breakpoints.get(path).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    fn send_breakpoints(&mut self, path: &Path) {
        let lines: Vec<usize> = self.lines_for(path);
        let Some(a) = self.adapter.as_mut() else { return };
        let bps: Vec<Value> = lines.iter().map(|l| json!({ "line": l + 1 })).collect();
        a.request("setBreakpoints", json!({ "source": { "path": path.display().to_string() }, "breakpoints": bps }), Pending::SetBreakpoints(path.to_path_buf()));
    }

    // ---------- lifecycle ----------

    pub fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Building | Phase::Starting | Phase::Running | Phase::Stopped)
    }

    fn log(&mut self, kind: &'static str, text: impl Into<String>) {
        let text = text.into();
        for l in text.lines() {
            self.console.push(Line { kind, text: l.to_string() });
        }
        if self.console.len() > 5000 {
            self.console.drain(..1000);
        }
    }

    /// Build (if configured) and launch. Errors are reported in the console.
    pub fn start(&mut self) {
        if self.is_active() {
            return;
        }
        self.console.clear();
        self.reset_state();
        let cwd = self.cwd();
        if !self.config.build.trim().is_empty() {
            self.phase = Phase::Building;
            self.log("cmd", format!("$ {}", self.config.build));
            let (tx, rx) = channel();
            let build = self.config.build.clone();
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                let out = Command::new("sh").args(["-c", &build]).current_dir(&cwd).stdin(Stdio::null()).output();
                let r = match out {
                    Ok(o) => {
                        let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                        if o.status.success() { Ok(text) } else { Err(text) }
                    }
                    Err(e) => Err(format!("couldn't run sh: {e}")),
                };
                let _ = tx.send(r);
                if let Some(c) = ctx {
                    c.request_repaint();
                }
            });
            self.building = Some(rx);
        } else {
            self.launch();
        }
    }

    fn cwd(&self) -> PathBuf {
        let c = self.config.cwd.trim();
        if c.is_empty() { self.root.clone() } else { self.root.join(c) }
    }

    fn program(&self) -> String {
        let p = self.config.program.trim();
        if p.is_empty() || Path::new(p).is_absolute() { p.to_string() } else { self.root.join(p).display().to_string() }
    }

    fn reset_state(&mut self) {
        self.thread = None;
        self.threads.clear();
        self.frames.clear();
        self.frame_sel = 0;
        self.scopes.clear();
        self.reason.clear();
        self.unverified.clear();
    }

    fn launch(&mut self) {
        let cwd = self.cwd();
        let kind = self.config.kind;
        let (tool, how) = kind.requirement();
        if !tool.is_empty() && find_in_path(tool).is_none() {
            self.log("error", format!("`{tool}` is not installed: {how}"));
            self.phase = Phase::Ended;
            return;
        }
        let result = match kind {
            Kind::Gdb => {
                let gdb = if self.root.join("Cargo.toml").exists() && find_in_path("rust-gdb").is_some() { "rust-gdb" } else { "gdb" };
                Adapter::start(&[gdb.into(), "-i".into(), "dap".into()], &cwd, self.ctx.clone())
            }
            Kind::Debugpy => Adapter::start(&["python3".into(), "-m".into(), "debugpy.adapter".into()], &cwd, self.ctx.clone()),
            Kind::Delve => match crate::dap::free_port() {
                Some(port) => {
                    let addr = format!("127.0.0.1:{port}");
                    Command::new("dlv")
                        .args(["dap", "--listen", &addr])
                        .current_dir(&cwd)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::piped())
                        .spawn()
                        .map_err(|e| format!("couldn't start dlv: {e}"))
                        .and_then(|child| Adapter::connect(&addr, Some(child), self.ctx.clone()))
                }
                None => Err("no free TCP port".into()),
            },
            Kind::Custom => Adapter::start(&self.config.command, &cwd, self.ctx.clone()),
        };
        match result {
            Ok(mut a) => {
                self.phase = Phase::Starting;
                a.request(
                    "initialize",
                    json!({ "clientID": "rim", "clientName": "rim", "adapterID": match kind { Kind::Gdb => "gdb", Kind::Debugpy => "debugpy", Kind::Delve => "go", Kind::Custom => "custom" },
                            "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path", "supportsRunInTerminalRequest": false, "supportsVariableType": true }),
                    Pending::Initialize,
                );
                self.adapter = Some(a);
            }
            Err(e) => {
                self.log("error", format!("Couldn't start the debug adapter: {e}"));
                self.phase = Phase::Ended;
            }
        }
    }

    fn launch_args(&self) -> Value {
        let args = split_args(&self.config.args);
        let cwd = self.cwd().display().to_string();
        let program = self.program();
        match self.config.kind {
            Kind::Gdb => json!({ "program": program, "args": args, "cwd": cwd, "stopAtBeginningOfMainSubprogram": false }),
            Kind::Debugpy => json!({ "program": program, "args": args, "cwd": cwd, "console": "internalConsole", "justMyCode": true, "stopOnEntry": false }),
            Kind::Delve => json!({ "mode": "debug", "program": program, "args": args, "cwd": cwd }),
            Kind::Custom => json!({ "program": program, "args": args, "cwd": cwd }),
        }
    }

    /// Stop the program and the adapter.
    pub fn stop(&mut self) {
        self.building = None;
        if let Some(mut a) = self.adapter.take() {
            if !a.exited {
                let terminate = self.capabilities["supportsTerminateRequest"].as_bool() == Some(true);
                if terminate {
                    a.request("terminate", json!({}), Pending::Other);
                }
                a.request("disconnect", json!({ "terminateDebuggee": true }), Pending::Disconnect);
                // Give it a moment to shut down cleanly before the kill in `Drop`.
                let started = std::time::Instant::now();
                while !a.exited && started.elapsed() < std::time::Duration::from_millis(1500) {
                    a.poll();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        if self.is_active() {
            self.phase = Phase::Ended;
            self.log("console", "Debugging stopped.");
        }
        self.thread = None;
        self.frames.clear();
        self.scopes.clear();
    }

    // ---------- control ----------

    fn control(&mut self, command: &str) {
        if self.phase != Phase::Stopped {
            return;
        }
        let thread = self.thread.unwrap_or(1);
        if let Some(a) = self.adapter.as_mut() {
            a.request(command, json!({ "threadId": thread }), Pending::Control);
            self.phase = Phase::Running;
            self.frames.clear();
            self.scopes.clear();
        }
    }

    pub fn continue_(&mut self) {
        self.control("continue");
    }

    pub fn step_over(&mut self) {
        self.control("next");
    }

    pub fn step_into(&mut self) {
        self.control("stepIn");
    }

    pub fn step_out(&mut self) {
        self.control("stepOut");
    }

    pub fn pause(&mut self) {
        if self.phase == Phase::Running
            && let Some(a) = self.adapter.as_mut()
        {
            a.request("pause", json!({ "threadId": self.thread.unwrap_or(1) }), Pending::Control);
        }
    }

    pub fn select_frame(&mut self, i: usize) {
        if i >= self.frames.len() {
            return;
        }
        self.frame_sel = i;
        self.scopes.clear();
        let id = self.frames[i].id;
        if let Some(a) = self.adapter.as_mut() {
            a.request("scopes", json!({ "frameId": id }), Pending::Scopes);
        }
        if let Some(p) = self.frames[i].path.clone() {
            self.effects.push(Effect::Reveal { path: p, line: self.frames[i].line });
        }
    }

    /// Fetch the children of a variable (`path` are indices from the scope down).
    pub fn expand(&mut self, scope: usize, path: &[usize]) {
        let Some(v) = var_at_mut(&mut self.scopes, scope, path) else { return };
        if v.children.is_some() || v.loading || v.reference <= 0 {
            return;
        }
        v.loading = true;
        let reference = v.reference;
        if let Some(a) = self.adapter.as_mut() {
            a.request("variables", json!({ "variablesReference": reference }), Pending::Variables { reference });
        }
    }

    pub fn fetch_scope(&mut self, i: usize) {
        let Some(s) = self.scopes.get_mut(i) else { return };
        if s.vars.is_some() || s.loading {
            return;
        }
        s.loading = true;
        let reference = s.reference;
        if let Some(a) = self.adapter.as_mut() {
            a.request("variables", json!({ "variablesReference": reference }), Pending::Variables { reference });
        }
    }

    /// Evaluate an expression in the selected frame; the answer goes to the console.
    pub fn evaluate(&mut self, expr: &str) {
        let expr = expr.trim();
        if expr.is_empty() {
            return;
        }
        self.log("cmd", format!("> {expr}"));
        let frame = self.frames.get(self.frame_sel).map(|f| f.id);
        match self.adapter.as_mut() {
            Some(a) => {
                a.request("evaluate", json!({ "expression": expr, "frameId": frame, "context": "repl" }), Pending::Evaluate);
            }
            None => self.log("error", "No debug session."),
        }
    }

    // ---------- events ----------

    /// Drive the session: build results, adapter messages. Returns what the app should do.
    pub fn poll(&mut self) -> Vec<Effect> {
        if let Some(rx) = &self.building
            && let Ok(r) = rx.try_recv()
        {
            self.building = None;
            match r {
                Ok(out) => {
                    self.log("stdout", out);
                    self.launch();
                }
                Err(out) => {
                    self.log("stderr", out);
                    self.log("error", "Build failed.");
                    self.phase = Phase::Ended;
                    self.effects.push(Effect::Attention);
                }
            }
        }
        let msgs = match self.adapter.as_mut() {
            Some(a) => a.poll(),
            None => vec![],
        };
        for m in msgs {
            // `stop()` (after a failed launch) drops the adapter: the rest of the batch is stale.
            if self.adapter.is_none() {
                break;
            }
            match m {
                Msg::Event { event, body } => self.event(&event, body),
                Msg::Response { pending, command, success, body, message } => self.response(pending, &command, success, body, message),
                Msg::Request { seq, command } => {
                    if let Some(a) = self.adapter.as_mut() {
                        a.respond(seq, &command, false, json!({ "error": { "format": "not supported by rim" } }));
                    }
                }
            }
        }
        if self.adapter.as_ref().is_some_and(|a| a.exited) {
            let tail = self.adapter.as_ref().map(|a| a.stderr_tail()).unwrap_or_default();
            self.adapter = None;
            if self.is_active() {
                self.phase = Phase::Ended;
                self.log("error", "The debug adapter exited.");
                if !tail.trim().is_empty() {
                    self.log("stderr", tail);
                }
                self.effects.push(Effect::Attention);
            }
        }
        std::mem::take(&mut self.effects)
    }

    fn event(&mut self, event: &str, body: Value) {
        match event {
            "initialized" => {
                let files: Vec<PathBuf> = self.breakpoints.keys().cloned().collect();
                for f in files {
                    self.send_breakpoints(&f);
                }
                let Some(a) = self.adapter.as_mut() else { return };
                a.request("setExceptionBreakpoints", json!({ "filters": [] }), Pending::Other);
                if self.capabilities["supportsConfigurationDoneRequest"].as_bool() != Some(false) {
                    a.request("configurationDone", json!({}), Pending::ConfigurationDone);
                }
            }
            "stopped" => {
                self.phase = Phase::Stopped;
                self.reason = body["reason"].as_str().unwrap_or("").to_string();
                if let Some(d) = body["description"].as_str().or(body["text"].as_str()) {
                    self.reason = d.to_string();
                }
                self.thread = body["threadId"].as_i64().or(self.thread);
                self.frames.clear();
                self.scopes.clear();
                if let Some(a) = self.adapter.as_mut() {
                    a.request("threads", json!({}), Pending::Threads);
                    a.request("stackTrace", json!({ "threadId": self.thread.unwrap_or(1), "startFrame": 0, "levels": 50 }), Pending::StackTrace);
                }
                self.effects.push(Effect::Attention);
            }
            "continued" => {
                self.phase = Phase::Running;
                self.frames.clear();
                self.scopes.clear();
            }
            "output" => {
                let kind = match body["category"].as_str().unwrap_or("console") {
                    "stderr" => "stderr",
                    "console" | "important" => "console",
                    _ => "stdout",
                };
                let text = body["output"].as_str().unwrap_or("").to_string();
                self.log(kind, text);
            }
            "exited" => {
                let code = body["exitCode"].as_i64().unwrap_or(0);
                self.log("console", format!("Program exited with code {code}."));
            }
            "terminated" => {
                self.phase = Phase::Ended;
                self.thread = None;
                self.frames.clear();
                self.scopes.clear();
                if let Some(a) = self.adapter.as_mut() {
                    a.request("disconnect", json!({}), Pending::Disconnect);
                }
                self.effects.push(Effect::Attention);
            }
            "thread" => {
                let id = body["threadId"].as_i64().unwrap_or(0);
                match body["reason"].as_str() {
                    Some("exited") => self.threads.retain(|(t, _)| *t != id),
                    _ => {
                        if !self.threads.iter().any(|(t, _)| *t == id) {
                            self.threads.push((id, format!("thread {id}")));
                        }
                    }
                }
            }
            "breakpoint" => {
                let bp = &body["breakpoint"];
                if let (Some(path), Some(line)) = (bp["source"]["path"].as_str(), bp["line"].as_u64()) {
                    let path = PathBuf::from(path);
                    let set = self.unverified.entry(path).or_default();
                    if bp["verified"].as_bool() == Some(true) {
                        set.remove(&(line as usize - 1));
                    } else {
                        set.insert(line as usize - 1);
                    }
                }
            }
            _ => {}
        }
    }

    fn response(&mut self, pending: Pending, command: &str, success: bool, body: Value, message: String) {
        if !success && !matches!(pending, Pending::Disconnect | Pending::Other) {
            let msg = if message.is_empty() { body["error"]["format"].as_str().unwrap_or("failed").to_string() } else { message };
            self.log("error", format!("{command}: {msg}"));
        }
        match pending {
            Pending::Initialize => {
                if !success {
                    self.phase = Phase::Ended;
                    return;
                }
                self.capabilities = body;
                let args = self.launch_args();
                if let Some(a) = self.adapter.as_mut() {
                    a.request("launch", args, Pending::Launch);
                }
            }
            Pending::Launch => {
                if success {
                    if self.phase == Phase::Starting {
                        self.phase = Phase::Running;
                    }
                } else {
                    self.stop();
                    self.phase = Phase::Ended;
                    self.effects.push(Effect::Attention);
                }
            }
            Pending::ConfigurationDone => {
                if self.phase == Phase::Starting {
                    self.phase = Phase::Running;
                }
            }
            Pending::SetBreakpoints(path) => {
                let wanted = self.lines_for(&path);
                let mut bad = BTreeSet::new();
                if let Some(arr) = body["breakpoints"].as_array() {
                    for (i, b) in arr.iter().enumerate() {
                        if b["verified"].as_bool() != Some(true)
                            && let Some(&l) = wanted.get(i)
                        {
                            bad.insert(l);
                        }
                    }
                }
                self.unverified.insert(path, bad);
            }
            Pending::Threads => {
                if let Some(arr) = body["threads"].as_array() {
                    self.threads = arr.iter().map(|t| (t["id"].as_i64().unwrap_or(0), t["name"].as_str().unwrap_or("").to_string())).collect();
                }
            }
            Pending::StackTrace => {
                self.frames = body["stackFrames"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|f| Frame {
                                id: f["id"].as_i64().unwrap_or(0),
                                name: f["name"].as_str().unwrap_or("?").to_string(),
                                path: f["source"]["path"].as_str().map(PathBuf::from),
                                line: (f["line"].as_u64().unwrap_or(1).max(1) - 1) as usize,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.frame_sel = 0;
                if !self.frames.is_empty() {
                    self.select_frame(0);
                }
            }
            Pending::Scopes => {
                self.scopes = body["scopes"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|s| Scope {
                                name: s["name"].as_str().unwrap_or("").to_string(),
                                reference: s["variablesReference"].as_i64().unwrap_or(0),
                                expensive: s["expensive"].as_bool().unwrap_or(false),
                                vars: None,
                                loading: false,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // The first scope (locals) right away; the others when they are opened.
                self.fetch_scope(0);
            }
            Pending::Variables { reference } => {
                let vars: Vec<Var> = body["variables"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|v| Var {
                                name: v["name"].as_str().unwrap_or("").to_string(),
                                value: v["value"].as_str().unwrap_or("").to_string(),
                                ty: v["type"].as_str().unwrap_or("").to_string(),
                                reference: v["variablesReference"].as_i64().unwrap_or(0),
                                children: None,
                                loading: false,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(s) = self.scopes.iter_mut().find(|s| s.reference == reference) {
                    s.vars = Some(vars);
                    s.loading = false;
                } else if let Some(v) = find_var_mut(&mut self.scopes, reference) {
                    v.children = Some(vars);
                    v.loading = false;
                }
            }
            Pending::Evaluate => {
                if success {
                    let r = body["result"].as_str().unwrap_or("").to_string();
                    self.log("stdout", r);
                }
            }
            Pending::Control | Pending::Disconnect | Pending::Other => {}
        }
    }
}

fn var_at_mut<'a>(scopes: &'a mut [Scope], scope: usize, path: &[usize]) -> Option<&'a mut Var> {
    let (&first, rest) = path.split_first()?;
    let mut v = scopes.get_mut(scope)?.vars.as_mut()?.get_mut(first)?;
    for &i in rest {
        v = v.children.as_mut()?.get_mut(i)?;
    }
    Some(v)
}

fn find_var_mut(scopes: &mut [Scope], reference: i64) -> Option<&mut Var> {
    fn walk(vars: &mut [Var], reference: i64) -> Option<&mut Var> {
        for v in vars {
            if v.reference == reference {
                return Some(v);
            }
            if let Some(c) = v.children.as_mut()
                && let Some(f) = walk(c, reference)
            {
                return Some(f);
            }
        }
        None
    }
    scopes.iter_mut().filter_map(|s| s.vars.as_mut()).find_map(|v| walk(v, reference))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spin(d: &mut Debugger, what: &str, pred: impl Fn(&Debugger) -> bool) -> Vec<Effect> {
        let mut fx = Vec::new();
        for _ in 0..600 {
            fx.extend(d.poll());
            if pred(d) {
                return fx;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}; console: {:?}", d.console);
    }

    #[test]
    fn split_args_handles_quotes() {
        assert_eq!(split_args("a  \"b c\" 'd' e"), ["a", "b c", "d", "e"]);
        assert!(split_args("   ").is_empty());
        assert_eq!(split_args("\"\""), [""]);
    }

    #[test]
    fn detection_and_saving() {
        let root = std::env::temp_dir().join(format!("rim-debug-detect-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"my-app\"\n").unwrap();
        let c = detect(&root, Some(&root.join("src/main.rs")));
        assert_eq!((c.kind, c.program.as_str(), c.build.as_str()), (Kind::Gdb, "target/debug/my-app", "cargo build"));
        let c = detect(&root, Some(&root.join("tool.py")));
        assert_eq!(c.kind, Kind::Debugpy);
        assert!(c.program.ends_with("tool.py"));
        assert_eq!(cargo_bin_name("[package]\nname = \"p\"\n[[bin]]\nname = \"cli\"\n").as_deref(), Some("cli"));
        let mut d = Debugger::new(None);
        d.load(&root);
        d.config = detect(&root, None);
        d.toggle_breakpoint(&root.join("src/main.rs"), 4);
        d.toggle_breakpoint(&root.join("src/main.rs"), 9);
        d.toggle_breakpoint(&root.join("src/main.rs"), 4);
        let mut e = Debugger::new(None);
        e.load(&root);
        assert_eq!(e.config, d.config);
        assert_eq!(e.lines_for(&root.join("src/main.rs")), [9]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_session_against_the_fake_adapter() {
        let root = std::env::temp_dir().join(format!("rim-debug-fake-{}", std::process::id()));
        let script = crate::dap::tests::fake_adapter(&root);
        let file = root.join("prog.c");
        std::fs::write(&file, "1\n2\n3\n4\n5\n").unwrap();
        let mut d = Debugger::new(None);
        d.load(&root);
        d.config = LaunchConfig { kind: Kind::Custom, command: vec![script.display().to_string()], program: "prog".into(), args: "--flag 'two words'".into(), ..Default::default() };
        d.toggle_breakpoint(&file, 2);
        d.start();
        let fx = spin(&mut d, "stop at the breakpoint", |d| d.phase == Phase::Stopped && !d.frames.is_empty() && d.scopes.first().is_some_and(|s| s.vars.is_some()));
        assert!(fx.contains(&Effect::Reveal { path: file.clone(), line: 2 }), "{fx:?}");
        assert_eq!(d.frames[0].name, "main");
        assert_eq!(d.reason, "breakpoint");
        let locals = d.scopes[0].vars.as_ref().unwrap();
        assert_eq!((locals[0].name.as_str(), locals[0].value.as_str(), locals[0].ty.as_str()), ("x", "42", "int"));
        assert!(d.scopes[1].vars.is_none(), "expensive scopes are not fetched");
        // Expanding a structured variable fetches its children.
        d.expand(0, &[1]);
        spin(&mut d, "children", |d| d.scopes[0].vars.as_ref().unwrap()[1].children.is_some());
        assert_eq!(d.scopes[0].vars.as_ref().unwrap()[1].children.as_ref().unwrap()[1].name, "py");
        // Step: next line.
        d.step_over();
        let fx = spin(&mut d, "step", |d| d.phase == Phase::Stopped && d.frames.first().is_some_and(|f| f.line == 3));
        assert!(fx.contains(&Effect::Reveal { path: file.clone(), line: 3 }));
        assert_eq!(d.reason, "step");
        d.evaluate("x + 1");
        spin(&mut d, "evaluate", |d| d.console.iter().any(|l| l.text == "x + 1 = 7"));
        d.continue_();
        spin(&mut d, "exit", |d| d.phase == Phase::Ended);
        let texts: Vec<&str> = d.console.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"done") && texts.contains(&"Program exited with code 3."), "{texts:?}");
        spin(&mut d, "adapter gone", |d| d.adapter.is_none());
        // A failed launch ends the session with the adapter's message.
        d.config.program = "fail".into();
        d.start();
        spin(&mut d, "launch failure", |d| d.phase == Phase::Ended);
        assert!(d.console.iter().any(|l| l.kind == "error" && l.text.contains("no such program")), "{:?}", d.console);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Needs gdb (with DAP) and a C compiler. `cargo test gdb_session -- --ignored`.
    #[test]
    #[ignore]
    fn gdb_session_on_a_c_program() {
        let root = std::env::temp_dir().join(format!("rim-debug-gdb-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("hello.c");
        std::fs::write(&src, "#include <stdio.h>\nint main(void) {\n    int x = 42;\n    printf(\"hi\\n\");\n    return x - 42;\n}\n").unwrap();
        let mut d = Debugger::new(None);
        d.load(&root);
        d.config = LaunchConfig { kind: Kind::Gdb, program: "hello".into(), build: "cc -g -O0 -o hello hello.c".into(), ..Default::default() };
        d.toggle_breakpoint(&src, 3);
        d.start();
        spin(&mut d, "stop", |d| d.phase == Phase::Stopped && d.scopes.first().is_some_and(|s| s.vars.is_some()));
        assert_eq!(d.frames[0].line, 3);
        assert!(d.scopes[0].vars.as_ref().unwrap().iter().any(|v| v.name == "x" && v.value == "42"));
        d.continue_();
        spin(&mut d, "exit", |d| d.phase == Phase::Ended);
        assert!(d.console.iter().any(|l| l.text == "hi"), "{:?}", d.console);
        std::fs::remove_dir_all(&root).ok();
    }
}
