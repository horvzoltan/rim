//! Minimal Language Server Protocol client over stdio (JSON-RPC).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};

use serde_json::{Value, json};

use crate::format::find_in_path;

/// What a request was for, so the response can be routed.
#[derive(Clone, Debug)]
pub enum Pending {
    Initialize,
    Completion { doc_id: u64, anchor: usize },
    Hover { doc_id: u64, char_idx: usize },
    Definition,
    Formatting { doc_id: u64, base_hash: u64, then_save: bool },
    /// `completionItem/resolve` after a completion was accepted: its import edits land if the
    /// text before `at` (a char index) still hashes to `prefix_hash`.
    ResolveCompletion { doc_id: u64, at: usize, prefix_hash: u64 },
    /// `textDocument/codeAction` for the quick-fix menu (request `seq`).
    CodeActions { seq: u64, server: String },
    /// `codeAction/resolve` for an action picked without its edit.
    ResolveCodeAction { server: String },
    ExecuteCommand,
    /// `textDocument/diagnostic` (pull diagnostics) for `uri` at document `version`.
    PullDiagnostics { uri: String, version: i32 },
}

pub enum LspEvent {
    Response { pending: Pending, result: Result<Value, String> },
    /// `server` is filled in by the app (a client doesn't know its name). `pulled`: the answer to a
    /// `textDocument/diagnostic` request rather than a pushed `publishDiagnostics`; a server may
    /// do both (e.g. its own checks pulled, a build's pushed), so the two are kept apart.
    Diagnostics { server: String, uri: String, diagnostics: Vec<Diagnostic>, pulled: bool },
    ShowMessage { level: u8, message: String },
    /// The server wants a workspace edit applied (`workspace/applyEdit`); answer with `Client::reply`.
    ApplyEdit { id: Value, edit: Value },
    /// The server rejected `initialize`; it is unusable.
    InitFailed(String),
    Exited,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// (line, utf16 col) positions.
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// 1 = error, 2 = warning, 3 = info, 4 = hint.
    pub severity: u8,
    pub message: String,
    pub source: String,
    /// The diagnostic as the server sent it, passed back as code-action context.
    pub raw: Value,
}

pub struct Client {
    child: Option<Child>,
    tx: Sender<Vec<u8>>,
    rx: Receiver<Option<Value>>,
    next_id: i64,
    pending: HashMap<i64, Pending>,
    pub ready: bool,
    queue: Vec<Value>,
    pub capabilities: Value,
    triggers: Vec<String>,
    /// Active `$/progress` messages by token.
    progress: HashMap<String, String>,
    pub opened: std::collections::HashSet<String>,
    /// Document version last sent, by URI.
    sent: HashMap<String, i32>,
    /// Pull diagnostics: the server's id for its last answer per URI (sent back to get "unchanged"),
    /// and the URIs with a request in flight (`true`: ask again when it's answered).
    result_ids: HashMap<String, String>,
    pulling: HashMap<String, bool>,
    /// Answers to `workspace/configuration` (by section); null when the server has none.
    settings: Value,
}

/// The part of `settings` for a dotted `section` (`"yaml.format"`); the whole object for "".
fn section(settings: &Value, section: &str) -> Value {
    if settings.is_null() {
        return Value::Null;
    }
    section.split('.').filter(|s| !s.is_empty()).try_fold(settings, |v, key| v.get(key)).cloned().unwrap_or(Value::Null)
}

pub fn uri(path: &Path) -> String {
    url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_default()
}

pub fn uri_to_path(u: &str) -> Option<std::path::PathBuf> {
    url::Url::parse(u).ok()?.to_file_path().ok()
}

pub fn pos(v: &Value) -> (usize, usize) {
    (v["line"].as_u64().unwrap_or(0) as usize, v["character"].as_u64().unwrap_or(0) as usize)
}

impl Client {
    #[cfg(test)]
    pub fn start(command: &[String], root: &Path, init_options: Value, ctx: egui::Context) -> Result<Client, String> {
        Self::start_with(command, root, init_options, Value::Null, &Default::default(), ctx)
    }

    pub fn start_with(
        command: &[String],
        root: &Path,
        init_options: Value,
        settings: Value,
        env: &std::collections::BTreeMap<String, String>,
        ctx: egui::Context,
    ) -> Result<Client, String> {
        let (prog, args) = command.split_first().ok_or("empty command")?;
        let prog = find_in_path(prog).ok_or_else(|| format!("`{prog}` not found in PATH"))?;
        let mut child = Command::new(prog)
            .args(args)
            .envs(env)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;

        let mut stdin = child.stdin.take().unwrap();
        let (tx, wrx) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for body in wrx {
                let header = format!("Content-Length: {}\r\n\r\n", body.len());
                if stdin.write_all(header.as_bytes()).is_err() || stdin.write_all(&body).is_err() {
                    break;
                }
                let _ = stdin.flush();
            }
        });

        let stdout = child.stdout.take().unwrap();
        let (rtx, rx) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut r) {
                if rtx.send(Some(msg)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            let _ = rtx.send(None);
            ctx.request_repaint();
        });

        // Drain stderr so the server never blocks on a full pipe.
        let mut stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(stderr.read(&mut buf), Ok(n) if n > 0) {}
        });

        let mut client = Client {
            child: Some(child),
            tx,
            rx,
            next_id: 1,
            pending: HashMap::new(),
            ready: false,
            queue: Vec::new(),
            capabilities: Value::Null,
            triggers: Vec::new(),
            progress: HashMap::new(),
            opened: Default::default(),
            settings,
            sent: HashMap::new(),
            result_ids: HashMap::new(),
            pulling: HashMap::new(),
        };
        let root_uri = uri(root);
        let root_name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "rim", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": root_uri,
            "rootPath": root.to_string_lossy(),
            "workspaceFolders": [{ "uri": root_uri, "name": root_name }],
            "initializationOptions": init_options,
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] },
                "window": { "workDoneProgress": true },
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                    "applyEdit": true,
                    "workspaceEdit": { "documentChanges": true },
                    "diagnostics": { "refreshSupport": true },
                },
                "textDocument": {
                    "synchronization": { "didSave": true, "dynamicRegistration": false },
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true,
                            "labelDetailsSupport": true,
                            "documentationFormat": ["plaintext", "markdown"],
                            // Auto-import edits may come later, from `completionItem/resolve`.
                            "resolveSupport": { "properties": ["additionalTextEdits", "detail", "documentation"] },
                        },
                        "contextSupport": true,
                    },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": { "linkSupport": true },
                    "formatting": {},
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": { "valueSet": ["", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports", "source.fixAll"] },
                        },
                        "isPreferredSupport": true,
                        "dataSupport": true,
                        "resolveSupport": { "properties": ["edit"] },
                    },
                    "publishDiagnostics": { "relatedInformation": false },
                    "diagnostic": { "dynamicRegistration": false, "relatedDocumentSupport": false },
                },
            },
        });
        let id = client.alloc(Pending::Initialize);
        client.write(&json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": params }));
        Ok(client)
    }

    fn alloc(&mut self, p: Pending) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, p);
        id
    }

    fn write(&self, msg: &Value) {
        let _ = self.tx.send(serde_json::to_vec(msg).unwrap());
    }

    fn send(&mut self, msg: Value) {
        if self.ready { self.write(&msg) } else { self.queue.push(msg) }
    }

    pub fn request(&mut self, method: &str, params: Value, p: Pending) {
        let id = self.alloc(p);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    /// Answer a server-to-client request that was handed to the app (see `LspEvent::ApplyEdit`).
    pub fn reply(&mut self, id: Value, result: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn version_sent(&self, uri: &str) -> Option<i32> {
        self.sent.get(uri).copied()
    }

    pub fn did_open(&mut self, uri: &str, lang: &str, version: i32, text: &str) {
        self.sent.insert(uri.to_string(), version);
        if self.opened.insert(uri.to_string()) {
            self.notify(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": lang, "version": version, "text": text } }),
            );
            self.pull_diagnostics(uri);
        }
    }

    pub fn did_change(&mut self, uri: &str, version: i32, text: &str) {
        self.sent.insert(uri.to_string(), version);
        self.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
        );
        self.pull_diagnostics(uri);
        // Other files' problems can depend on this one (a changed export, a fixed type).
        if self.capabilities["diagnosticProvider"]["interFileDependencies"].as_bool() == Some(true) {
            let others: Vec<String> = self.opened.iter().filter(|u| *u != uri).cloned().collect();
            for u in others {
                self.pull_diagnostics(&u);
            }
        }
    }

    pub fn did_save(&mut self, uri: &str, text: &str) {
        self.notify("textDocument/didSave", json!({ "textDocument": { "uri": uri }, "text": text }));
    }

    pub fn did_close(&mut self, uri: &str) {
        self.sent.remove(uri);
        self.result_ids.remove(uri);
        if self.opened.remove(uri) {
            self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
        }
    }

    /// Whether the server wants its diagnostics asked for (`textDocument/diagnostic`).
    pub fn pulls_diagnostics(&self) -> bool {
        self.ready && self.supports("diagnosticProvider")
    }

    /// Ask for a document's diagnostics (pull model). One request per document at a time: asking
    /// again while one is in flight re-asks once it is answered, so typing doesn't pile them up.
    pub fn pull_diagnostics(&mut self, uri: &str) {
        if !self.pulls_diagnostics() || !self.opened.contains(uri) {
            return;
        }
        if let Some(again) = self.pulling.get_mut(uri) {
            *again = true;
            return;
        }
        self.pulling.insert(uri.to_string(), false);
        let version = self.sent.get(uri).copied().unwrap_or(0);
        let mut params = json!({ "textDocument": { "uri": uri } });
        if let Some(id) = self.result_ids.get(uri) {
            params["previousResultId"] = json!(id);
        }
        if let Some(id) = self.capabilities["diagnosticProvider"]["identifier"].as_str() {
            params["identifier"] = json!(id);
        }
        self.request("textDocument/diagnostic", params, Pending::PullDiagnostics { uri: uri.to_string(), version });
    }

    /// Handle a pull-diagnostics answer: new diagnostics become an event (unless the document
    /// changed since it was asked for, in which case it is asked again).
    fn pulled(&mut self, uri: String, version: i32, msg: &Value, events: &mut Vec<LspEvent>) {
        let mut again = self.pulling.remove(&uri).unwrap_or(false);
        if !self.opened.contains(&uri) {
            return;
        }
        if let Some(e) = msg.get("error") {
            // ServerCancelled (e.g. the document changed): retry unless the server says not to.
            again |= e["code"].as_i64() == Some(-32802) && e["data"]["retriggerRequest"].as_bool() != Some(false);
        } else {
            let r = &msg["result"];
            match r["resultId"].as_str() {
                Some(id) => self.result_ids.insert(uri.clone(), id.to_string()),
                None => self.result_ids.remove(&uri),
            };
            let current = self.sent.get(&uri) == Some(&version);
            if r["kind"] == "full" && current {
                let diagnostics = r["items"].as_array().map(|a| a.iter().map(parse_diagnostic).collect()).unwrap_or_default();
                events.push(LspEvent::Diagnostics { server: String::new(), uri: uri.clone(), diagnostics, pulled: true });
            } else if !current {
                self.result_ids.remove(&uri);
                again = true;
            }
        }
        if again {
            self.pull_diagnostics(&uri);
        }
    }

    pub fn trigger_characters(&self) -> &[String] {
        &self.triggers
    }

    pub fn supports(&self, cap: &str) -> bool {
        let v = &self.capabilities[cap];
        !v.is_null() && v != &Value::Bool(false)
    }

    pub fn progress_text(&self) -> Option<String> {
        self.progress.values().next().cloned()
    }

    /// Drain incoming messages, answering server-to-client requests internally.
    pub fn poll(&mut self) -> Vec<LspEvent> {
        let mut events = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            let Some(msg) = msg else {
                events.push(LspEvent::Exited);
                break;
            };
            let method = msg["method"].as_str().map(str::to_string);
            match (method, msg.get("id")) {
                // Response to one of our requests.
                (None, Some(id)) => {
                    let Some(p) = id.as_i64().and_then(|id| self.pending.remove(&id)) else { continue };
                    if let Pending::PullDiagnostics { uri, version } = p {
                        self.pulled(uri, version, &msg, &mut events);
                        continue;
                    }
                    let result = match msg.get("error") {
                        Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                        None => Ok(msg["result"].clone()),
                    };
                    if let Pending::Initialize = p {
                        match &result {
                            Ok(r) => {
                                self.capabilities = r["capabilities"].clone();
                                self.triggers = self.capabilities["completionProvider"]["triggerCharacters"]
                                    .as_array()
                                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                    .unwrap_or_default();
                            }
                            Err(e) => {
                                events.push(LspEvent::InitFailed(e.clone()));
                                continue;
                            }
                        }
                        self.ready = true;
                        self.write(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
                        for m in std::mem::take(&mut self.queue) {
                            self.write(&m);
                        }
                        // Documents opened while starting up: ask for their diagnostics now.
                        let opened: Vec<String> = self.opened.iter().cloned().collect();
                        for u in opened {
                            self.pull_diagnostics(&u);
                        }
                    } else {
                        events.push(LspEvent::Response { pending: p, result });
                    }
                }
                // The app applies workspace edits and answers itself.
                (Some(method), Some(id)) if method == "workspace/applyEdit" => {
                    events.push(LspEvent::ApplyEdit { id: id.clone(), edit: msg["params"]["edit"].clone() });
                }
                // Request from the server: reply with something harmless.
                (Some(method), Some(id)) => {
                    let refresh = method == "workspace/diagnostic/refresh";
                    let result = match method.as_str() {
                        "workspace/configuration" => {
                            let items = msg["params"]["items"].as_array().cloned().unwrap_or_default();
                            Value::Array(items.iter().map(|it| section(&self.settings, it["section"].as_str().unwrap_or(""))).collect())
                        }
                        "workspace/workspaceFolders" => Value::Array(vec![]),
                        _ => Value::Null,
                    };
                    self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                    // The server's view changed (project loaded, config edited): ask again for every file.
                    if refresh {
                        self.result_ids.clear();
                        let opened: Vec<String> = self.opened.iter().cloned().collect();
                        for u in opened {
                            self.pull_diagnostics(&u);
                        }
                    }
                }
                // Notification.
                (Some(method), None) => match method.as_str() {
                    "textDocument/publishDiagnostics" => {
                        let p = &msg["params"];
                        let diagnostics = p["diagnostics"]
                            .as_array()
                            .map(|a| a.iter().map(parse_diagnostic).collect())
                            .unwrap_or_default();
                        events.push(LspEvent::Diagnostics { server: String::new(), uri: p["uri"].as_str().unwrap_or("").into(), diagnostics, pulled: false });
                    }
                    "$/progress" => {
                        let p = &msg["params"];
                        let token = p["token"].to_string();
                        let v = &p["value"];
                        match v["kind"].as_str() {
                            Some("end") => {
                                self.progress.remove(&token);
                            }
                            Some(kind) => {
                                let prev = self.progress.get(&token).cloned().unwrap_or_default();
                                let title = if kind == "begin" {
                                    v["title"].as_str().unwrap_or("").to_string()
                                } else {
                                    prev.split(':').next().unwrap_or("").to_string()
                                };
                                let mut text = title;
                                if let Some(m) = v["message"].as_str() {
                                    text = format!("{text}: {m}");
                                }
                                if let Some(pct) = v["percentage"].as_u64() {
                                    text = format!("{text} {pct}%");
                                }
                                self.progress.insert(token, text);
                            }
                            None => {}
                        }
                    }
                    "window/showMessage" => {
                        let p = &msg["params"];
                        events.push(LspEvent::ShowMessage {
                            level: p["type"].as_u64().unwrap_or(3) as u8,
                            message: p["message"].as_str().unwrap_or("").to_string(),
                        });
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        events
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Ask the server to shut down cleanly (so it can save caches, e.g. rust-analyzer),
        // then make sure it is gone, without blocking the UI.
        let id = self.next_id;
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null }));
        self.write(&json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                while start.elapsed() < std::time::Duration::from_millis(800) {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

fn read_message(r: &mut impl BufRead) -> Option<Value> {
    loop {
        let mut len = None;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse::<usize>().ok();
            }
        }
        let Some(len) = len else { continue };
        let mut body = vec![0; len];
        r.read_exact(&mut body).ok()?;
        if let Ok(v) = serde_json::from_slice(&body) {
            return Some(v);
        }
    }
}

fn parse_diagnostic(d: &Value) -> Diagnostic {
    Diagnostic {
        start: pos(&d["range"]["start"]),
        end: pos(&d["range"]["end"]),
        severity: d["severity"].as_u64().unwrap_or(1) as u8,
        message: d["message"].as_str().unwrap_or("").to_string(),
        source: d["source"].as_str().unwrap_or("").to_string(),
        raw: d.clone(),
    }
}

/// Flatten hover contents (MarkupContent | MarkedString | MarkedString[]) to plain text.
pub fn hover_text(result: &Value) -> String {
    fn one(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            _ => String::new(),
        }
    }
    let c = &result["contents"];
    let raw = match c {
        Value::Array(a) => a.iter().map(one).collect::<Vec<_>>().join("\n\n"),
        _ => one(c),
    };
    // Drop markdown code fences and horizontal rules; keep the content.
    raw.lines()
        .filter(|l| !l.trim_start().starts_with("```") && l.trim() != "---")
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// An extra edit applied with a completion (e.g. an auto-import): start, end, new text.
pub type AdditionalEdit = ((usize, usize), (usize, usize), String);

#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    pub kind: u8,
    pub detail: String,
    pub filter: String,
    pub sort: String,
    pub insert: String,
    pub snippet: bool,
    /// Replace range start (line, utf16 col) if the server gave a textEdit.
    pub edit_start: Option<(usize, usize)>,
    /// Replace range end, if the server gave a textEdit.
    pub edit_end: Option<(usize, usize)>,
    pub additional: Vec<AdditionalEdit>,
    /// The item as the server sent it, for `completionItem/resolve`.
    pub raw: Value,
}

pub fn parse_completions(result: &Value) -> (Vec<CompletionItem>, bool) {
    let (items, incomplete) = match result {
        Value::Array(a) => (a.clone(), false),
        Value::Object(_) => (
            result["items"].as_array().cloned().unwrap_or_default(),
            result["isIncomplete"].as_bool().unwrap_or(false),
        ),
        _ => (vec![], false),
    };
    let mut out: Vec<CompletionItem> = items
        .iter()
        .map(|it| {
            let label = it["label"].as_str().unwrap_or("").to_string();
            let edit = &it["textEdit"];
            let (insert, edit_start, edit_end) = if edit.is_object() {
                let range = if edit["range"].is_object() { &edit["range"] } else { &edit["replace"] };
                (edit["newText"].as_str().unwrap_or(&label).to_string(), Some(pos(&range["start"])), Some(pos(&range["end"])))
            } else {
                (it["insertText"].as_str().unwrap_or(&label).to_string(), None, None)
            };
            let mut detail = it["detail"].as_str().unwrap_or("").to_string();
            if detail.is_empty() {
                detail = it["labelDetails"]["description"].as_str().unwrap_or("").to_string();
            }
            let additional = text_edits(&it["additionalTextEdits"]);
            CompletionItem {
                filter: it["filterText"].as_str().unwrap_or(&label).to_string(),
                sort: it["sortText"].as_str().unwrap_or(&label).to_string(),
                kind: it["kind"].as_u64().unwrap_or(1) as u8,
                snippet: it["insertTextFormat"].as_u64() == Some(2),
                label,
                detail,
                insert,
                edit_start,
                edit_end,
                additional,
                raw: it.clone(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.sort.cmp(&b.sort));
    (out, incomplete)
}

/// `TextEdit[]` as (start, end, new text), positions in (line, utf-16 col).
pub fn text_edits(v: &Value) -> Vec<AdditionalEdit> {
    v.as_array()
        .map(|a| a.iter().map(|e| (pos(&e["range"]["start"]), pos(&e["range"]["end"]), e["newText"].as_str().unwrap_or("").into())).collect())
        .unwrap_or_default()
}

/// A `WorkspaceEdit` as text edits per document URI, in the order the server listed them.
/// Resource operations (create / rename / delete files) are not supported and are reported as an error.
pub fn workspace_edits(edit: &Value) -> Result<Vec<(String, Vec<AdditionalEdit>)>, String> {
    let mut out: Vec<(String, Vec<AdditionalEdit>)> = Vec::new();
    let mut add = |uri: &str, edits: Vec<AdditionalEdit>| match out.iter_mut().find(|(u, _)| u == uri) {
        Some((_, e)) => e.extend(edits),
        None => out.push((uri.to_string(), edits)),
    };
    if let Some(changes) = edit["documentChanges"].as_array() {
        for c in changes {
            match c["kind"].as_str() {
                Some(k) => return Err(format!("file {k} operations are not supported")),
                None => add(c["textDocument"]["uri"].as_str().unwrap_or(""), text_edits(&c["edits"])),
            }
        }
    } else if let Some(changes) = edit["changes"].as_object() {
        for (uri, edits) in changes {
            add(uri, text_edits(edits));
        }
    }
    out.retain(|(u, e)| !u.is_empty() && !e.is_empty());
    Ok(out)
}

/// One entry of the quick-fix menu.
#[derive(Clone, Debug)]
pub struct CodeAction {
    pub title: String,
    /// `quickfix`, `source.organizeImports`, … (empty for a bare `Command`).
    pub kind: String,
    pub preferred: bool,
    /// The server that offered it (edits are resolved and commands run there).
    pub server: String,
    /// The `CodeAction` or `Command` as the server sent it.
    pub raw: Value,
}

/// Parse a `textDocument/codeAction` result (`(Command | CodeAction)[]`), dropping disabled ones.
pub fn parse_code_actions(result: &Value, server: &str) -> Vec<CodeAction> {
    result
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|v| v["disabled"].is_null())
                .filter_map(|v| {
                    Some(CodeAction {
                        title: v["title"].as_str()?.to_string(),
                        kind: v["kind"].as_str().unwrap_or("").to_string(),
                        preferred: v["isPreferred"].as_bool().unwrap_or(false),
                        server: server.to_string(),
                        raw: v.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Normalize Location | Location[] | LocationLink[] to (uri, start position).
pub fn first_location(result: &Value) -> Option<(String, (usize, usize))> {
    let loc = match result {
        Value::Array(a) => a.first()?,
        Value::Object(_) => result,
        _ => return None,
    };
    if let Some(u) = loc["targetUri"].as_str() {
        return Some((u.to_string(), pos(&loc["targetSelectionRange"]["start"])));
    }
    Some((loc["uri"].as_str()?.to_string(), pos(&loc["range"]["start"])))
}

/// Decide how to run the TypeScript/JavaScript server (see `typescript_launch_with`).
/// TypeScript 7+ (native) has a built-in server, `tsc --lsp --stdio`; older versions go through
/// typescript-language-server, which only looks in the project's node_modules, so it gets the
/// path to `tsserver.js`.
pub fn typescript_launch(root: &Path, default: &[String], bundled: Option<&Path>) -> (Vec<String>, Value) {
    typescript_launch_with(root, default, bundled, find_in_path("tsc"))
}

/// How to run TypeScript for `root`: the project's own TypeScript first; without one, a global
/// TypeScript that still ships `tsserver.js` (5, 6), then rim's bundled copy (`bundled`, the path
/// of its `tsserver.js`) over a global TypeScript 7, whose native `tsc --lsp` is the last resort.
/// Returns the command and the initialization options to merge.
pub fn typescript_launch_with(root: &Path, default: &[String], bundled: Option<&Path>, global_tsc: Option<std::path::PathBuf>) -> (Vec<String>, Value) {
    let ts_dir = |tsc: &Path| tsc.canonicalize().ok().and_then(|p| Some(p.parent()?.parent()?.to_path_buf()));
    let tsserver_of = |tsc: &Path| ts_dir(tsc).map(|d| d.join("lib/tsserver.js")).filter(|p| p.is_file());
    let native = |tsc: &Path| (vec![tsc.to_string_lossy().to_string(), "--lsp".into(), "--stdio".into()], json!({}));
    let with_tsserver = |p: &Path| (default.to_vec(), json!({ "tsserver": { "path": p } }));
    let local = root.join("node_modules/.bin/tsc");
    if local.is_file() {
        // The project's own TypeScript, whichever version it is.
        return match tsserver_of(&local) {
            Some(ts) => with_tsserver(&ts),
            None => native(&local),
        };
    }
    if let Some(ts) = global_tsc.as_deref().and_then(tsserver_of) {
        return with_tsserver(&ts);
    }
    if let Some(b) = bundled.filter(|b| b.is_file()) {
        return with_tsserver(b);
    }
    match global_tsc {
        Some(tsc) => native(&tsc),
        None => (default.to_vec(), json!({})),
    }
}

pub fn kind_label(kind: u8) -> &'static str {
    match kind {
        2 => "method",
        3 => "fn",
        4 => "ctor",
        5 => "field",
        6 => "var",
        7 => "class",
        8 => "iface",
        9 => "mod",
        10 => "prop",
        12 => "value",
        13 => "enum",
        14 => "kw",
        15 => "snip",
        17 => "file",
        19 => "folder",
        20 => "variant",
        21 => "const",
        22 => "struct",
        23 => "event",
        24 => "op",
        25 => "type",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to a real rust-analyzer; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn rust_analyzer_roundtrip() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = root.join("src/text.rs");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut c = Client::start(&["rust-analyzer".into()], &root, Value::Null, egui::Context::default()).unwrap();
        let u = uri(&path);
        c.did_open(&u, "rust", 1, &text);
        // Hover over `hash_str` in its definition line.
        let line = text.lines().position(|l| l.starts_with("pub fn hash_str")).unwrap();
        let start = std::time::Instant::now();
        let mut got_hover = false;
        let mut got_completion = false;
        let mut asked = false;
        while start.elapsed().as_secs() < 120 && !(got_hover && got_completion) {
            if c.ready && (!asked || start.elapsed().as_millis() % 3000 < 20) {
                let p = json!({ "textDocument": { "uri": u }, "position": { "line": line, "character": 9 } });
                c.request("textDocument/hover", p.clone(), Pending::Hover { doc_id: 0, char_idx: 0 });
                let p2 = json!({ "textDocument": { "uri": u }, "position": { "line": line + 2, "character": 4 } });
                c.request("textDocument/completion", p2, Pending::Completion { doc_id: 0, anchor: 0 });
                asked = true;
            }
            for e in c.poll() {
                if let LspEvent::Response { pending, result: Ok(v) } = e {
                    match pending {
                        Pending::Hover { .. } if !hover_text(&v).is_empty() => {
                            println!("hover: {}", hover_text(&v));
                            got_hover = true;
                        }
                        Pending::Completion { .. } => {
                            let (items, _) = parse_completions(&v);
                            if !items.is_empty() {
                                println!("{} completions, e.g. {}", items.len(), items[0].label);
                                got_completion = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(got_hover && got_completion);
    }
}

#[cfg(test)]
#[path = "validate/lsp_tests.rs"]
pub(crate) mod validate_tests;
