//! A Debug Adapter Protocol client: talks to an adapter over stdio or TCP, matches
//! responses to the requests that asked for them, and hands events to the session.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::format::find_in_path;

/// What a request was for, so its response can be routed.
#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    Initialize,
    Launch,
    SetBreakpoints(std::path::PathBuf),
    ConfigurationDone,
    Threads,
    StackTrace,
    Scopes,
    Variables { reference: i64 },
    Evaluate,
    /// continue / next / stepIn / stepOut / pause.
    Control,
    Disconnect,
    Other,
}

pub enum Msg {
    Event { event: String, body: Value },
    Response { pending: Pending, command: String, success: bool, body: Value, message: String },
    /// A request from the adapter to us (`runInTerminal`, …).
    Request { seq: i64, command: String },
}

pub struct Adapter {
    child: Option<Child>,
    tx: Sender<Vec<u8>>,
    rx: Receiver<Option<Value>>,
    stderr: Arc<Mutex<String>>,
    seq: i64,
    pending: HashMap<i64, Pending>,
    pub exited: bool,
}

impl Adapter {
    /// Start an adapter that speaks the protocol on its stdin/stdout.
    pub fn start(command: &[String], cwd: &Path, ctx: Option<egui::Context>) -> Result<Adapter, String> {
        let (prog, args) = command.split_first().ok_or("empty adapter command")?;
        let prog = find_in_path(prog).ok_or_else(|| format!("`{prog}` not found in PATH"))?;
        let mut child = Command::new(prog)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Ok(Self::wire(Some(child), Box::new(stdin), Box::new(stdout), Some(Box::new(stderr)), ctx))
    }

    /// Connect to an adapter listening on TCP (delve); `child` is the process serving it, if we
    /// started one. Retries for a few seconds while it comes up.
    pub fn connect(addr: &str, child: Option<Child>, ctx: Option<egui::Context>) -> Result<Adapter, String> {
        let started = Instant::now();
        let stream = loop {
            match TcpStream::connect(addr) {
                Ok(s) => break s,
                Err(e) if started.elapsed() < Duration::from_secs(8) => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("couldn't connect to the debug adapter at {addr}: {e}")),
            }
        };
        let write = stream.try_clone().map_err(|e| e.to_string())?;
        let stderr: Option<Box<dyn Read + Send>> = child.as_ref().and(None);
        let mut child = child;
        let err = child.as_mut().and_then(|c| c.stderr.take()).map(|e| Box::new(e) as Box<dyn Read + Send>);
        Ok(Self::wire(child, Box::new(write), Box::new(stream), err.or(stderr), ctx))
    }

    fn wire(child: Option<Child>, mut w: Box<dyn Write + Send>, r: Box<dyn Read + Send>, err: Option<Box<dyn Read + Send>>, ctx: Option<egui::Context>) -> Adapter {
        let (tx, wrx) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for body in wrx {
                let header = format!("Content-Length: {}\r\n\r\n", body.len());
                if w.write_all(header.as_bytes()).is_err() || w.write_all(&body).is_err() {
                    break;
                }
                let _ = w.flush();
            }
        });
        let (rtx, rx) = channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(r);
            while let Some(msg) = read_message(&mut r) {
                if rtx.send(Some(msg)).is_err() {
                    return;
                }
                if let Some(c) = &repaint {
                    c.request_repaint();
                }
            }
            let _ = rtx.send(None);
            if let Some(c) = &repaint {
                c.request_repaint();
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut e) = err {
            let buf = stderr.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = e.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut s = buf.lock().unwrap();
                    s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if s.len() > 64 << 10 {
                        let cut = s.len() - (32 << 10);
                        s.drain(..cut);
                    }
                }
            });
        }
        Adapter { child, tx, rx, stderr, seq: 1, pending: HashMap::new(), exited: false }
    }

    pub fn request(&mut self, command: &str, arguments: Value, pending: Pending) -> i64 {
        let seq = self.seq;
        self.seq += 1;
        self.pending.insert(seq, pending);
        let msg = json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments });
        let _ = self.tx.send(msg.to_string().into_bytes());
        seq
    }

    /// Answer a request the adapter sent us.
    pub fn respond(&mut self, request_seq: i64, command: &str, success: bool, body: Value) {
        let seq = self.seq;
        self.seq += 1;
        let msg = json!({ "seq": seq, "type": "response", "request_seq": request_seq, "command": command, "success": success, "body": body });
        let _ = self.tx.send(msg.to_string().into_bytes());
    }

    pub fn poll(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            let Some(v) = m else {
                self.exited = true;
                break;
            };
            match v["type"].as_str() {
                Some("event") => out.push(Msg::Event { event: v["event"].as_str().unwrap_or("").to_string(), body: v["body"].clone() }),
                Some("response") => {
                    let pending = v["request_seq"].as_i64().and_then(|s| self.pending.remove(&s)).unwrap_or(Pending::Other);
                    out.push(Msg::Response {
                        pending,
                        command: v["command"].as_str().unwrap_or("").to_string(),
                        success: v["success"].as_bool().unwrap_or(false),
                        body: v["body"].clone(),
                        message: v["message"].as_str().unwrap_or("").to_string(),
                    });
                }
                Some("request") => out.push(Msg::Request { seq: v["seq"].as_i64().unwrap_or(0), command: v["command"].as_str().unwrap_or("").to_string() }),
                _ => {}
            }
        }
        if let Some(c) = &mut self.child
            && matches!(c.try_wait(), Ok(Some(_)))
        {
            self.exited = true;
        }
        out
    }

    pub fn stderr_tail(&self) -> String {
        let s = self.stderr.lock().unwrap();
        s.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    pub fn kill(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.exited = true;
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        self.kill();
    }
}

fn read_message<R: BufRead>(r: &mut R) -> Option<Value> {
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

/// A free local TCP port for an adapter that listens instead of using stdio.
pub fn free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    /// A fake adapter: stops at the first breakpoint it was given, steps one line at a time,
    /// has a `Locals` scope with `x = 42` (expandable) and prints `done` when continued.
    pub fn fake_adapter(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("fake-dap");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import sys, json
seq = 100
def send(m):
    global seq
    m["seq"] = seq; seq += 1
    b = json.dumps(m).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b); sys.stdout.buffer.flush()
def ok(req, body=None): send({"type":"response","request_seq":req["seq"],"command":req["command"],"success":True,"body":body})
def event(name, body=None): send({"type":"event","event":name,"body":body})
bp = None; line = 0
while True:
    header = sys.stdin.buffer.readline()
    if not header: break
    n = int(header.split(b":")[1]); sys.stdin.buffer.readline()
    req = json.loads(sys.stdin.buffer.read(n)); a = req.get("arguments") or {}; c = req["command"]
    if c == "initialize":
        ok(req, {"supportsConfigurationDoneRequest": True}); event("initialized")
    elif c == "launch":
        launch = req
        if str(a.get("program", "")).endswith("fail"):
            send({"type":"response","request_seq":req["seq"],"command":"launch","success":False,"message":"no such program"})
    elif c == "setBreakpoints":
        lines = [b["line"] for b in a.get("breakpoints", [])]
        if lines and bp is None: bp = (a["source"]["path"], lines[0])
        ok(req, {"breakpoints": [{"verified": True, "line": l} for l in lines]})
    elif c == "configurationDone":
        ok(req); ok(launch)
        event("output", {"category": "stdout", "output": "started\n"})
        if bp:
            line = bp[1]; event("stopped", {"reason": "breakpoint", "threadId": 1, "allThreadsStopped": True})
        else:
            event("exited", {"exitCode": 0}); event("terminated")
    elif c == "threads": ok(req, {"threads": [{"id": 1, "name": "main"}]})
    elif c == "stackTrace":
        ok(req, {"stackFrames": [{"id": 7, "name": "main", "line": line, "column": 1, "source": {"path": bp[0]}}, {"id": 8, "name": "_start", "line": 1, "column": 1}]})
    elif c == "scopes": ok(req, {"scopes": [{"name": "Locals", "variablesReference": 1, "expensive": False}, {"name": "Registers", "variablesReference": 3, "expensive": True}]})
    elif c == "variables":
        r = a["variablesReference"]
        if r == 1: ok(req, {"variables": [{"name": "x", "value": "42", "type": "int", "variablesReference": 0}, {"name": "p", "value": "{...}", "type": "Point", "variablesReference": 2}]})
        elif r == 2: ok(req, {"variables": [{"name": "px", "value": "1", "variablesReference": 0}, {"name": "py", "value": "2", "variablesReference": 0}]})
        else: ok(req, {"variables": []})
    elif c == "next":
        ok(req); line += 1; event("stopped", {"reason": "step", "threadId": 1})
    elif c == "continue":
        ok(req, {"allThreadsContinued": True}); event("continued", {"threadId": 1})
        event("output", {"category": "stdout", "output": "done\n"}); event("exited", {"exitCode": 3}); event("terminated")
    elif c == "evaluate": ok(req, {"result": a["expression"] + " = 7", "variablesReference": 0})
    elif c == "disconnect":
        ok(req); break
    else: ok(req)
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
}
