//! Validation: LSP client against a fake language server (python, JSON-RPC over stdio).
//! Included from lsp.rs as a child module; `fake_lsp` is reused by the app tests.

use super::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const FAKE_LSP: &str = r#"#!/usr/bin/env python3
import sys, json, os
LOG = os.environ.get("FAKE_LSP_LOG") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "lsp.log")
out = sys.stdout.buffer
inp = sys.stdin.buffer
def send(m):
    b = json.dumps(m).encode()
    out.write(b"Content-Length: %d\r\n\r\n" % len(b) + b); out.flush()
def log(m):
    with open(LOG, "a") as f: f.write(json.dumps(m) + "\n")
docs = {}
ROOT = ""
next_id = 1000
while True:
    headers = {}
    while True:
        line = inp.readline()
        if not line: sys.exit(0)
        line = line.decode().strip()
        if not line: break
        k, v = line.split(":", 1); headers[k.lower()] = v.strip()
    body = inp.read(int(headers["content-length"]))
    m = json.loads(body)
    log(m)
    meth = m.get("method"); mid = m.get("id")
    if meth == "initialize":
        ROOT = m["params"]["rootUri"]
        # Ask the client something before answering (server->client request).
        send({"jsonrpc":"2.0","id":next_id,"method":"workspace/configuration","params":{"items":[{"section":"x"},{"section":"y"}]}}); next_id += 1
        send({"jsonrpc":"2.0","id":mid,"result":{"capabilities":{
            "textDocumentSync":1,
            "completionProvider":{"triggerCharacters":[".", "::"]},
            "hoverProvider":True, "definitionProvider":True, "documentFormattingProvider":True}}})
    elif meth == "initialized":
        send({"jsonrpc":"2.0","method":"$/progress","params":{"token":"t","value":{"kind":"begin","title":"Indexing","percentage":5}}})
        send({"jsonrpc":"2.0","method":"window/showMessage","params":{"type":2,"message":"fake server ready"}})
    elif meth in ("textDocument/didOpen", "textDocument/didChange"):
        td = m["params"]["textDocument"]
        text = td["text"] if meth.endswith("didOpen") else m["params"]["contentChanges"][-1]["text"]
        docs[td["uri"]] = text
        diags = []
        for i, l in enumerate(text.split("\n")):
            # utf-16 column of the word "bad"
            j = l.find("bad")
            if j >= 0:
                col = len(l[:j].encode("utf-16-le")) // 2
                diags.append({"range":{"start":{"line":i,"character":col},"end":{"line":i,"character":col+3}},"severity":1,"message":"bad word","source":"fake"})
        send({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":td["uri"],"version":td.get("version"),"diagnostics":diags}})
    elif meth == "textDocument/completion":
        p = m["params"]["position"]
        # Replace range: from the start of the identifier at the cursor (utf-16 columns; ASCII here).
        cur_line = docs.get(m["params"]["textDocument"]["uri"], "").split("\n")[p["line"]] if docs.get(m["params"]["textDocument"]["uri"]) is not None else ""
        ws = p["character"]
        while ws > 0 and ws <= len(cur_line) and (cur_line[ws-1].isalnum() or cur_line[ws-1] == "_"):
            ws -= 1
        send({"jsonrpc":"2.0","id":mid,"result":{"isIncomplete":False,"items":[
            {"label":"println!","kind":3,"insertTextFormat":2,"insertText":"println!(\"${1:fmt}\")$0","sortText":"1"},
            {"label":"print_all","kind":3,"sortText":"2",
             "textEdit":{"range":{"start":{"line":p["line"],"character":ws},"end":p},"newText":"print_all()"},
             "additionalTextEdits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"use all;\n"}]},
            {"label":"zzz","kind":6,"sortText":"3"}]}})
    elif meth == "textDocument/hover":
        send({"jsonrpc":"2.0","id":mid,"result":{"contents":{"kind":"markdown","value":"```rust\nfn fake()\n```\n---\nDocs here"}}})
    elif meth == "textDocument/definition":
        target = os.environ.get("FAKE_LSP_DEF") or (ROOT + "/def.rs")
        send({"jsonrpc":"2.0","id":mid,"result":[{"targetUri":target,"targetRange":{"start":{"line":1,"character":0},"end":{"line":1,"character":5}},"targetSelectionRange":{"start":{"line":1,"character":3},"end":{"line":1,"character":5}}}]})
    elif meth == "textDocument/formatting":
        uri = m["params"]["textDocument"]["uri"]
        text = docs.get(uri, "")
        lines = text.split("\n")
        edits = []
        # Two inserts at the same position: LSP says they apply in array order -> "AB".
        edits.append({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"A"})
        edits.append({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"B"})
        # Replace the first line's trailing spaces.
        l0 = lines[0]
        stripped = l0.rstrip(" ")
        if stripped != l0:
            edits.append({"range":{"start":{"line":0,"character":len(stripped.encode("utf-16-le"))//2},"end":{"line":0,"character":len(l0.encode("utf-16-le"))//2}},"newText":""})
        send({"jsonrpc":"2.0","id":mid,"result":edits})
    elif meth == "shutdown":
        send({"jsonrpc":"2.0","id":mid,"result":None})
    elif mid is not None and meth is None:
        pass  # response to our request
"#;

pub fn fake_lsp(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join("fake-lsp");
    std::fs::write(&p, FAKE_LSP).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

pub fn read_log(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("lsp.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-lsp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn pump(c: &mut Client, until: impl Fn(&[LspEvent], &Client) -> bool) -> Vec<LspEvent> {
    let start = Instant::now();
    let mut all = Vec::new();
    while start.elapsed() < Duration::from_secs(10) {
        all.extend(c.poll());
        if until(&all, c) {
            return all;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for LSP");
}

#[test]
fn initialize_queue_sync_and_requests() {
    let dir = scratch("client");
    let bin = fake_lsp(&dir);
    let file = dir.join("a.rs");
    let u = uri(&file);
    let mut c = Client::start(&[bin.to_string_lossy().to_string()], &dir, json!({"opt": 1}), egui::Context::default()).unwrap();
    // Messages sent before the server is ready are queued, then flushed after `initialized`.
    c.did_open(&u, "rust", 1, "let bad = 1;\n");
    c.did_change(&u, 2, "let 😀 = bad;\n");
    c.request("textDocument/completion", json!({"textDocument":{"uri":u},"position":{"line":0,"character":5}}), Pending::Completion { doc_id: 1, anchor: 4 });
    assert!(!c.ready);
    let evs = pump(&mut c, |e, _| e.iter().filter(|e| matches!(e, LspEvent::Diagnostics { .. })).count() >= 2 && e.iter().any(|e| matches!(e, LspEvent::Response { .. })));
    assert!(c.ready);
    assert_eq!(c.trigger_characters(), vec![".".to_string(), "::".to_string()]);
    assert!(c.supports("documentFormattingProvider"));
    assert!(!c.supports("renameProvider"));
    assert!(evs.iter().any(|e| matches!(e, LspEvent::ShowMessage { level: 2, message } if message == "fake server ready")));
    assert_eq!(c.progress_text().as_deref(), Some("Indexing 5%"));

    // Diagnostics: the second one is after an emoji: utf-16 column 9 ("let 😀 = " = 4 + 2 + 3).
    let diags: Vec<&Vec<Diagnostic>> = evs.iter().filter_map(|e| if let LspEvent::Diagnostics { diagnostics, .. } = e { Some(diagnostics) } else { None }).collect();
    assert_eq!(diags[0][0].start, (0, 4));
    assert_eq!(diags[1][0].start, (0, 9));
    assert_eq!(diags[1][0].severity, 1);
    assert_eq!(diags[1][0].source, "fake");

    // Completion response parses: snippet + textEdit + additionalTextEdits.
    let Some(LspEvent::Response { result: Ok(v), pending: Pending::Completion { .. } }) = evs.iter().find(|e| matches!(e, LspEvent::Response { .. })) else { panic!() };
    let (items, incomplete) = parse_completions(v);
    assert!(!incomplete);
    assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["println!", "print_all", "zzz"]);
    assert!(items[0].snippet);
    assert_eq!(items[1].edit_start, Some((0, 5)), "cursor is after a space: empty word");
    assert_eq!(items[1].additional.len(), 1);

    // Order of what the server saw.
    let log = read_log(&dir);
    let methods: Vec<&str> = log.iter().filter_map(|m| m["method"].as_str()).collect();
    assert_eq!(&methods[..5], ["initialize", "initialized", "textDocument/didOpen", "textDocument/didChange", "textDocument/completion"]);
    assert_eq!(log[0]["params"]["initializationOptions"]["opt"], 1);
    assert_eq!(log[0]["params"]["capabilities"]["textDocument"]["completion"]["completionItem"]["snippetSupport"], true);
    // The server->client workspace/configuration request was answered with one null per item.
    let reply = log.iter().find(|m| m["id"] == 1000 && m.get("method").is_none()).expect("client answered workspace/configuration");
    assert_eq!(reply["result"], json!([null, null]));
    // didChange is a full-text sync with the version.
    let change = log.iter().find(|m| m["method"] == "textDocument/didChange").unwrap();
    assert_eq!(change["params"]["textDocument"]["version"], 2);
    assert_eq!(change["params"]["contentChanges"][0]["text"], "let 😀 = bad;\n");

    // Hover / definition.
    c.request("textDocument/hover", json!({"textDocument":{"uri":u},"position":{"line":0,"character":1}}), Pending::Hover { doc_id: 1, char_idx: 1 });
    c.request("textDocument/definition", json!({"textDocument":{"uri":u},"position":{"line":0,"character":1}}), Pending::Definition);
    let evs = pump(&mut c, |e, _| e.iter().filter(|e| matches!(e, LspEvent::Response { .. })).count() >= 2);
    for e in &evs {
        match e {
            LspEvent::Response { pending: Pending::Hover { .. }, result: Ok(v) } => assert_eq!(hover_text(v), "fn fake()\nDocs here"),
            LspEvent::Response { pending: Pending::Definition, result: Ok(v) } => {
                let (_, pos) = first_location(v).unwrap();
                assert_eq!(pos, (1, 3), "LocationLink uses targetSelectionRange");
            }
            _ => {}
        }
    }
    // didOpen twice for the same uri is ignored; didClose then re-open works.
    c.did_open(&u, "rust", 3, "x");
    c.did_close(&u);
    c.did_close(&u);
    c.did_open(&u, "rust", 4, "y");
    std::thread::sleep(Duration::from_millis(300));
    c.poll();
    let log = read_log(&dir);
    let opens = log.iter().filter(|m| m["method"] == "textDocument/didOpen").count();
    let closes = log.iter().filter(|m| m["method"] == "textDocument/didClose").count();
    assert_eq!((opens, closes), (2, 1));
    drop(c);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn server_that_exits_or_is_missing() {
    let dir = scratch("exit");
    let p = dir.join("dies");
    std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = Client::start(&[p.to_string_lossy().to_string()], &dir, Value::Null, egui::Context::default()).unwrap();
    pump(&mut c, |e, _| e.iter().any(|e| matches!(e, LspEvent::Exited)));
    assert!(Client::start(&["definitely-not-a-real-lsp-binary".into()], &dir, Value::Null, egui::Context::default()).is_err());
    assert!(Client::start(&[], &dir, Value::Null, egui::Context::default()).is_err());
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn initialize_error_reports_init_failed() {
    let dir = scratch("initerr");
    let p = dir.join("err-lsp");
    std::fs::write(&p, r#"#!/usr/bin/env python3
import sys, json
h = {}
while True:
    l = sys.stdin.buffer.readline().decode().strip()
    if not l: break
    k, v = l.split(":", 1); h[k.lower()] = v
m = json.loads(sys.stdin.buffer.read(int(h["content-length"])))
b = json.dumps({"jsonrpc":"2.0","id":m["id"],"error":{"code":-32603,"message":"nope"}}).encode()
sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b); sys.stdout.buffer.flush()
sys.stdin.read()
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = Client::start(&[p.to_string_lossy().to_string()], &dir, Value::Null, egui::Context::default()).unwrap();
    let evs = pump(&mut c, |e, _| e.iter().any(|e| matches!(e, LspEvent::InitFailed(_))));
    assert!(evs.iter().any(|e| matches!(e, LspEvent::InitFailed(m) if m == "nope")));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn read_message_handles_extra_headers_and_garbage() {
    let body = r#"{"jsonrpc":"2.0","method":"x"}"#;
    let raw = format!("garbage line\r\n\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let mut r = std::io::BufReader::new(raw.as_bytes());
    let v = read_message(&mut r).unwrap();
    assert_eq!(v["method"], "x");
}

#[test]
fn completion_parsing_edge_cases() {
    // insertReplace edit, labelDetails, plain array, missing fields.
    let (items, _) = parse_completions(&json!([
        { "label": "a", "textEdit": { "insert": {"start":{"line":0,"character":1},"end":{"line":0,"character":2}}, "replace": {"start":{"line":0,"character":1},"end":{"line":0,"character":4}}, "newText": "abc" }, "labelDetails": {"description": "desc"} },
        { },
    ]));
    assert_eq!(items.len(), 2);
    let a = items.iter().find(|i| i.label == "a").unwrap();
    assert_eq!(a.insert, "abc");
    assert_eq!(a.edit_start, Some((0, 1)));
    assert_eq!(a.detail, "desc");
    assert_eq!(parse_completions(&Value::Null).0.len(), 0);
    assert_eq!(hover_text(&json!({"contents": ["a", {"language":"rust","value":"b"}]})), "a\n\nb");
    assert_eq!(first_location(&json!({"uri":"file:///x","range":{"start":{"line":2,"character":3}}})), Some(("file:///x".into(), (2, 3))));
    assert_eq!(first_location(&json!([])), None);
}
