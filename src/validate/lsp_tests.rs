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
PULL = bool(os.environ.get("FAKE_LSP_PULL"))
STRICT = False
def problems(uri):
    text = docs.get(uri, "")
    defined = any("fn missing" in t for t in docs.values())
    words = [("bad", "bad word")] + ([] if defined else [("missing", "cannot find missing")]) + ([("meh", "meh is not allowed")] if STRICT else [])
    diags = []
    for i, l in enumerate(text.split("\n")):
        for w, msg in words:
            j = l.find(w)
            if j >= 0 and not (w == "missing" and "fn missing" in l):
                col = len(l[:j].encode("utf-16-le")) // 2
                diags.append({"range":{"start":{"line":i,"character":col},"end":{"line":i,"character":col+len(w)}},"severity":1,"message":msg,"source":"fake"})
    return diags
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
            "completionProvider":{"triggerCharacters":[".", "::"], "resolveProvider":True},
            "codeActionProvider":{"resolveProvider":True},
            "executeCommandProvider":{"commands":["fake.addHeader"]},
            "hoverProvider":True, "definitionProvider":True, "documentFormattingProvider":True,
            **({"diagnosticProvider":{"identifier":"fake","interFileDependencies":True,"workspaceDiagnostics":False}} if PULL else {})}}})
    elif meth == "initialized":
        send({"jsonrpc":"2.0","method":"$/progress","params":{"token":"t","value":{"kind":"begin","title":"Indexing","percentage":5}}})
        send({"jsonrpc":"2.0","method":"window/showMessage","params":{"type":2,"message":"fake server ready"}})
    elif meth in ("textDocument/didOpen", "textDocument/didChange"):
        td = m["params"]["textDocument"]
        text = td["text"] if meth.endswith("didOpen") else m["params"]["contentChanges"][-1]["text"]
        docs[td["uri"]] = text
        if PULL:
            continue
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
        items = []
        if cur_line[ws:p["character"]].startswith("T"):
            # An auto-import: the import edit only comes with completionItem/resolve.
            items.append({"label":"Thing","kind":7,"sortText":"0","data":{"import":"use things::Thing;\n"}})
        send({"jsonrpc":"2.0","id":mid,"result":{"isIncomplete":False,"items":items + [
            {"label":"println!","kind":3,"insertTextFormat":2,"insertText":"println!(\"${1:fmt}\")$0","sortText":"1"},
            {"label":"print_all","kind":3,"sortText":"2",
             "textEdit":{"range":{"start":{"line":p["line"],"character":ws},"end":p},"newText":"print_all()"},
             "additionalTextEdits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"use all;\n"}]},
            {"label":"zzz","kind":6,"sortText":"3"}]}})
    elif meth == "completionItem/resolve":
        item = dict(m["params"])
        imp = (item.get("data") or {}).get("import")
        if imp:
            item["additionalTextEdits"] = [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":imp}]
        send({"jsonrpc":"2.0","id":mid,"result":item})
    elif meth == "textDocument/codeAction":
        uri = m["params"]["textDocument"]["uri"]
        acts = []
        for d in m["params"]["context"]["diagnostics"]:
            if d.get("message") == "bad word":
                acts.append({"title":"Replace 'bad' with 'good'","kind":"quickfix","isPreferred":True,"diagnostics":[d],
                             "edit":{"changes":{uri:[{"range":d["range"],"newText":"good"}]}}})
        acts.append({"title":"Add a header (command)","kind":"source","command":{"title":"x","command":"fake.addHeader","arguments":[uri]}})
        acts.append({"title":"Add a footer (resolved later)","kind":"refactor.rewrite","data":{"uri":uri}})
        other = os.environ.get("FAKE_LSP_OTHER")
        if other:
            acts.append({"title":"Touch the other file","kind":"quickfix",
                         "edit":{"documentChanges":[{"textDocument":{"uri":other,"version":None},"edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"// touched\n"}]}]}})
        send({"jsonrpc":"2.0","id":mid,"result":acts})
    elif meth == "codeAction/resolve":
        act = dict(m["params"])
        uri = act["data"]["uri"]
        n = len(docs.get(uri, "").split("\n")) - 1
        act["edit"] = {"documentChanges":[{"textDocument":{"uri":uri,"version":None},"edits":[{"range":{"start":{"line":n,"character":0},"end":{"line":n,"character":0}},"newText":"// footer\n"}]}]}
        send({"jsonrpc":"2.0","id":mid,"result":act})
    elif meth == "textDocument/diagnostic":
        uri = m["params"]["textDocument"]["uri"]
        diags = problems(uri)
        rid = str(abs(hash(json.dumps(diags, sort_keys=True))))
        if m["params"].get("previousResultId") == rid:
            send({"jsonrpc":"2.0","id":mid,"result":{"kind":"unchanged","resultId":rid}})
        else:
            send({"jsonrpc":"2.0","id":mid,"result":{"kind":"full","resultId":rid,"items":diags}})
    elif meth == "workspace/executeCommand" and m["params"]["command"] == "fake.strict":
        STRICT = True
        send({"jsonrpc":"2.0","id":mid,"result":None})
        send({"jsonrpc":"2.0","id":next_id,"method":"workspace/diagnostic/refresh","params":None}); next_id += 1
    elif meth == "workspace/executeCommand":
        uri = m["params"]["arguments"][0]
        # The server applies the command's effect by asking the client to edit.
        send({"jsonrpc":"2.0","id":next_id,"method":"workspace/applyEdit","params":{"label":"header","edit":{"changes":{uri:[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"// header\n"}]}}}}); next_id += 1
        send({"jsonrpc":"2.0","id":mid,"result":None})
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

#[test]
fn workspace_configuration_answers_from_settings() {
    let dir = scratch("settings");
    let bin = fake_lsp(&dir);
    let settings = json!({ "x": { "a": 1 }, "yaml": { "format": { "enable": true } } });
    let mut env = std::collections::BTreeMap::new();
    env.insert("FAKE_LSP_LOG".to_string(), dir.join("custom.log").to_string_lossy().to_string());
    let mut c = Client::start_with(&[bin.to_string_lossy().to_string()], &dir, Value::Null, settings, &env, egui::Context::default()).unwrap();
    // The env var reached the server: it logs to the custom file.
    let read = || -> Vec<Value> {
        std::fs::read_to_string(dir.join("custom.log")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    };
    pump(&mut c, |_, c| c.ready && read().iter().any(|m| m["id"] == 1000));
    let log = read();
    // The fake asks for sections "x" and "y" before answering initialize.
    let answer = log.iter().find(|m| m["id"] == 1000).expect("configuration answer");
    assert_eq!(answer["result"], json!([{ "a": 1 }, null]));
    assert_eq!(section(&json!({ "yaml": { "format": { "enable": true } } }), "yaml.format"), json!({ "enable": true }));
    assert_eq!(section(&json!({ "a": 1 }), ""), json!({ "a": 1 }));
    assert_eq!(section(&Value::Null, "a"), Value::Null);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn workspace_edits_and_code_actions_parse() {
    let e = |l: u64, c: u64, t: &str| json!({ "range": { "start": { "line": l, "character": c }, "end": { "line": l, "character": c } }, "newText": t });
    let changes = json!({ "changes": { "file:///a": [e(0, 0, "x")], "file:///b": [e(1, 2, "y"), e(3, 0, "z")] } });
    let mut got = workspace_edits(&changes).unwrap();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(got.len(), 2);
    assert_eq!(got[1].1, vec![((1, 2), (1, 2), "y".to_string()), ((3, 0), (3, 0), "z".to_string())]);
    // documentChanges wins over changes, and edits for one file are merged in order.
    let dc = json!({ "documentChanges": [
        { "textDocument": { "uri": "file:///a", "version": 3 }, "edits": [e(0, 0, "1")] },
        { "textDocument": { "uri": "file:///a", "version": 3 }, "edits": [e(0, 0, "2")] },
    ], "changes": { "file:///ignored": [e(0, 0, "no")] } });
    assert_eq!(workspace_edits(&dc).unwrap(), vec![("file:///a".to_string(), vec![((0, 0), (0, 0), "1".to_string()), ((0, 0), (0, 0), "2".to_string())])]);
    let create = json!({ "documentChanges": [{ "kind": "create", "uri": "file:///new" }] });
    assert!(workspace_edits(&create).unwrap_err().contains("create"));
    // Commands and code actions both parse; disabled ones are dropped.
    let acts = json!([
        { "title": "Run it", "command": "x.run", "arguments": [] },
        { "title": "Fix", "kind": "quickfix", "isPreferred": true, "edit": {} },
        { "title": "Nope", "kind": "quickfix", "disabled": { "reason": "no" } },
    ]);
    let a = parse_code_actions(&acts, "srv");
    assert_eq!(a.iter().map(|a| (a.title.as_str(), a.kind.as_str(), a.preferred)).collect::<Vec<_>>(), [("Run it", "", false), ("Fix", "quickfix", true)]);
    assert_eq!(a[0].server, "srv");
}

/// Against the real typescript-language-server (when installed): a missing import is offered as
/// a quick fix, and the import for a completion arrives with `completionItem/resolve`.
/// Run with `cargo test typescript_auto_import -- --ignored`.
#[test]
#[ignore]
fn typescript_auto_import_with_the_real_server() {
    let Some(ts) = find_in_path("typescript-language-server") else { panic!("typescript-language-server not on PATH") };
    let dir = scratch("ts-real");
    std::fs::write(dir.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"target":"es2020","module":"es2020"}}"#).unwrap();
    std::fs::write(dir.join("hooks.ts"), "export interface AfterViewInit { ngAfterViewInit(): void }\n").unwrap();
    let text = "export class Oven implements AfterViewInit {\n  ngAfterViewInit() {}\n}\nconst x: AfterV\n";
    let file = dir.join("oven.ts");
    std::fs::write(&file, text).unwrap();
    let u = uri(&file);
    // typescript-language-server on TypeScript 5's tsserver (a project's own `typescript`, or rim's copy).
    let tsserver = std::env::var("RIM_TSSERVER").map(PathBuf::from).unwrap_or_else(|_| {
        dirs::data_dir().unwrap().join("rim/servers/typescript-language-server/node_modules/typescript/lib/tsserver.js")
    });
    assert!(tsserver.is_file(), "no tsserver.js at {} (set RIM_TSSERVER)", tsserver.display());
    let cmd = vec![ts.to_string_lossy().to_string(), "--stdio".into()];
    let opts = json!({ "tsserver": { "path": tsserver } });
    let mut c = Client::start(&cmd, &dir, opts, egui::Context::default()).unwrap();
    c.did_open(&u, "typescript", 1, text);
    let diags = pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Diagnostics { diagnostics, .. } if diagnostics.iter().any(|d| d.message.contains("AfterViewInit")))));
    eprintln!("CAPS diagnosticProvider={}", c.capabilities["diagnosticProvider"]);
    assert!(!c.pulls_diagnostics(), "this server pushes; pulling too would double every problem");
    let d = diags
        .iter()
        .find_map(|e| match e {
            LspEvent::Diagnostics { diagnostics, .. } => diagnostics.iter().find(|d| d.message.contains("AfterViewInit")).cloned(),
            _ => None,
        })
        .unwrap();
    // Quick fix for "Cannot find name 'AfterViewInit'".
    let params = json!({ "textDocument": { "uri": u }, "range": d.raw["range"], "context": { "diagnostics": [d.raw] } });
    c.request("textDocument/codeAction", params, Pending::CodeActions { seq: 1, server: "ts".into() });
    let ev = pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Response { pending: Pending::CodeActions { .. }, .. })));
    let result = ev.into_iter().find_map(|e| match e { LspEvent::Response { pending: Pending::CodeActions { .. }, result } => Some(result.unwrap()), _ => None }).unwrap();
    let actions = parse_code_actions(&result, "ts");
    eprintln!("actions: {:?}", actions.iter().map(|a| &a.title).collect::<Vec<_>>());
    let fix = actions.iter().find(|a| a.title.contains("import") && a.title.contains("./hooks")).expect("an add-import fix");
    let edits = workspace_edits(&fix.raw["edit"]).unwrap();
    eprintln!("fix edit: {edits:?}");
    assert!(edits[0].1.iter().any(|e| e.2.contains("import") && e.2.contains("AfterViewInit")));
    // Completion for `AfterV` at the end of the last line, then resolve for its import.
    c.request("textDocument/completion", json!({ "textDocument": { "uri": u }, "position": { "line": 3, "character": 15 } }), Pending::Completion { doc_id: 1, anchor: 0 });
    let ev = pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Response { pending: Pending::Completion { .. }, .. })));
    let result = ev.into_iter().find_map(|e| match e { LspEvent::Response { pending: Pending::Completion { .. }, result } => Some(result.unwrap()), _ => None }).unwrap();
    let (items, _) = parse_completions(&result);
    let item = items.iter().find(|i| i.label == "AfterViewInit").expect("AfterViewInit offered");
    eprintln!("item before resolve: additional={:?}", item.additional);
    assert!(item.additional.is_empty(), "the import is not in the list itself");
    c.request("completionItem/resolve", item.raw.clone(), Pending::ResolveCompletion { doc_id: 1, at: 0, prefix_hash: 0 });
    let ev = pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Response { pending: Pending::ResolveCompletion { .. }, .. })));
    let result = ev.into_iter().find_map(|e| match e { LspEvent::Response { pending: Pending::ResolveCompletion { .. }, result } => Some(result.unwrap()), _ => None }).unwrap();
    let extra = text_edits(&result["additionalTextEdits"]);
    eprintln!("after resolve: {extra:?}");
    assert!(extra.iter().any(|e| e.2.contains("import") && e.2.contains("./hooks")));
}

/// Which TypeScript runs, for projects with and without their own, next to a global TypeScript 7
/// (no `tsserver.js`) or 6, and rim's bundled copy.
#[test]
fn typescript_launch_prefers_project_then_global_tsserver_then_bundled() {
    let d = scratch("ts-launch");
    let fake_ts = |dir: &Path, tsserver: bool| -> PathBuf {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("bin/tsc"), "").unwrap();
        if tsserver {
            std::fs::write(dir.join("lib/tsserver.js"), "").unwrap();
        }
        dir.join("bin/tsc")
    };
    let ts7 = fake_ts(&d.join("global7"), false);
    let ts6 = fake_ts(&d.join("global6"), true);
    let bundled_dir = d.join("rim/node_modules/typescript");
    fake_ts(&bundled_dir, true);
    let bundled = bundled_dir.join("lib/tsserver.js");
    let default = vec!["typescript-language-server".to_string(), "--stdio".into()];
    let path_of = |v: &Value| v["tsserver"]["path"].as_str().map(PathBuf::from);
    let proj = d.join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    // No TypeScript in the project, global TypeScript 7: rim's bundled copy.
    let (cmd, opts) = typescript_launch_with(&proj, &default, Some(&bundled), Some(ts7.clone()));
    assert_eq!(cmd, default);
    assert_eq!(path_of(&opts), Some(bundled.clone()));
    // ...a global TypeScript 6 still wins over the bundled copy.
    let (_, opts) = typescript_launch_with(&proj, &default, Some(&bundled), Some(ts6.clone()));
    assert_eq!(path_of(&opts), Some(d.join("global6/lib/tsserver.js").canonicalize().unwrap()));
    // No bundled copy (the server came from PATH): TypeScript 7's own server.
    let (cmd, _) = typescript_launch_with(&proj, &default, None, Some(ts7.clone()));
    assert_eq!(cmd[1..], ["--lsp", "--stdio"]);
    let (cmd, opts) = typescript_launch_with(&proj, &default, Some(&d.join("missing.js")), None);
    assert_eq!((cmd, opts), (default.clone(), json!({})));

    // The project's own TypeScript wins over everything, 6 or 7.
    let own = proj.join("node_modules/typescript");
    fake_ts(&own, true);
    std::fs::create_dir_all(proj.join("node_modules/.bin")).unwrap();
    std::os::unix::fs::symlink(own.join("bin/tsc"), proj.join("node_modules/.bin/tsc")).unwrap();
    let (_, opts) = typescript_launch_with(&proj, &default, Some(&bundled), Some(ts7.clone()));
    assert_eq!(path_of(&opts), Some(own.join("lib/tsserver.js").canonicalize().unwrap()));
    std::fs::remove_file(own.join("lib/tsserver.js")).unwrap();
    let (cmd, _) = typescript_launch_with(&proj, &default, Some(&bundled), Some(ts6));
    assert_eq!(cmd[1..], ["--lsp", "--stdio"], "a project on TypeScript 7 gets its own server");
}

/// Against TypeScript 7's native server (a global `tsc` 7+): problems are pulled, and the
/// add-import quick fix works from them. Run with `cargo test typescript7 -- --ignored`.
#[test]
#[ignore]
fn typescript7_native_server_pull_diagnostics() {
    let tsc = find_in_path("tsc").expect("tsc on PATH");
    let dir = scratch("ts7");
    std::fs::write(dir.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"target":"es2020","module":"es2020"}}"#).unwrap();
    std::fs::write(dir.join("hooks.ts"), "export interface AfterViewInit { ngAfterViewInit(): void }\n").unwrap();
    let text = "export class Oven implements AfterViewInit {\n  ngAfterViewInit() {}\n}\n";
    let file = dir.join("oven.ts");
    std::fs::write(&file, text).unwrap();
    let u = uri(&file);
    let mut c = Client::start(&[tsc.to_string_lossy().to_string(), "--lsp".into(), "--stdio".into()], &dir, json!({}), egui::Context::default()).unwrap();
    c.did_open(&u, "typescript", 1, text);
    let ev = pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Diagnostics { uri, diagnostics, .. } if *uri == u && !diagnostics.is_empty())));
    let (d, pulled) = ev
        .iter()
        .find_map(|e| match e {
            LspEvent::Diagnostics { uri, diagnostics, pulled, .. } if *uri == u && !diagnostics.is_empty() => Some((diagnostics[0].clone(), *pulled)),
            _ => None,
        })
        .unwrap();
    eprintln!("pulled={pulled} {}: {}", d.source, d.message);
    assert!(pulled && d.message.contains("AfterViewInit"));
    // Fix the file: the next pull reports it clean.
    let fixed = format!("import {{ AfterViewInit }} from \"./hooks\";\n{text}");
    c.did_change(&u, 2, &fixed);
    pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Diagnostics { uri, diagnostics, .. } if *uri == u && diagnostics.is_empty())));
    // Back to broken: the quick fix offers the import.
    c.did_change(&u, 3, text);
    pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Diagnostics { uri, diagnostics, .. } if *uri == u && !diagnostics.is_empty())));
    let params = json!({ "textDocument": { "uri": u }, "range": d.raw["range"], "context": { "diagnostics": [d.raw] } });
    c.request("textDocument/codeAction", params, Pending::CodeActions { seq: 1, server: "ts".into() });
    let ev = pump(&mut c, |ev, _| ev.iter().any(|e| matches!(e, LspEvent::Response { pending: Pending::CodeActions { .. }, .. })));
    let result = ev.into_iter().find_map(|e| match e { LspEvent::Response { pending: Pending::CodeActions { .. }, result } => Some(result.unwrap()), _ => None }).unwrap();
    let titles: Vec<String> = parse_code_actions(&result, "ts").into_iter().map(|a| a.title).collect();
    eprintln!("actions: {titles:?}");
    assert!(titles.iter().any(|t| t.contains("import") && t.contains("./hooks")));
}

/// rust-analyzer supports both models. With pull support advertised, its own diagnostics must not
/// arrive both ways (which would show every problem twice). Run with `-- --ignored`.
#[test]
#[ignore]
fn rust_analyzer_does_not_report_twice() {
    let ra = find_in_path("rust-analyzer").expect("rust-analyzer on PATH");
    let dir = scratch("ra-pull");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    // A syntax error: reported by rust-analyzer itself, not by `cargo check`.
    let text = "fn main() {\n    let x = ;\n}\n";
    let file = dir.join("src/main.rs");
    std::fs::write(&file, text).unwrap();
    let u = uri(&file);
    let mut c = Client::start(&[ra.to_string_lossy().to_string()], &dir, json!({ "checkOnSave": false }), egui::Context::default()).unwrap();
    c.did_open(&u, "rust", 1, text);
    let start = Instant::now();
    let (mut pushed, mut pulled) = (Vec::new(), Vec::new());
    while start.elapsed() < Duration::from_secs(40) {
        for e in c.poll() {
            if let LspEvent::Diagnostics { uri, diagnostics, pulled: p, .. } = e
                && uri == u
            {
                let m: Vec<String> = diagnostics.iter().map(|d| d.message.clone()).collect();
                if p { pulled = m } else { pushed = m }
            }
        }
        if !pushed.is_empty() || !pulled.is_empty() {
            // Give the other channel time to answer too.
            if start.elapsed() > Duration::from_secs(20) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("RA pulls={} pushed={pushed:?} pulled={pulled:?}", c.pulls_diagnostics());
    assert!(!pushed.is_empty() || !pulled.is_empty(), "no diagnostics from rust-analyzer");
    for m in &pulled {
        assert!(!pushed.contains(m), "reported both ways: {m}");
    }
}
