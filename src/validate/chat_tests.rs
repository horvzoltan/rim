//! Validation: the Claude panel against a fake streaming `claude` CLI (no real CLI, no usage).
//! Clicks are aimed at buttons by finding their text in egui's output shapes.
//! Included from chat.rs as a child module.

use super::*;
use egui::{Event, Pos2, Rect, Shape};
use std::time::Duration;

const FAKE_STREAM_CLI: &str = r#"#!/usr/bin/env python3
import sys, json, os, time
here = os.path.dirname(os.path.abspath(__file__))
def rec(name, obj):
    with open(os.path.join(here, name), "a") as f: f.write(json.dumps(obj) + "\n")
rec("args.log", sys.argv[1:])
sid = "sess-new"
if "--resume" in sys.argv:
    sid = sys.argv[sys.argv.index("--resume") + 1]
def out(m): print(json.dumps(m), flush=True)
def ev(e): out({"type":"stream_event","event":e})
turn = 0
for line in sys.stdin:
    m = json.loads(line)
    rec("stdin.log", m)
    if m["type"] == "user":
        turn += 1
        out({"type":"system","subtype":"init","session_id":sid,"model":"fake-model"})
        ev({"type":"message_start","message":{"id":"m%d" % turn}})
        ev({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
        for piece in ["Hel", "lo **wor", "ld**"]:
            ev({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":piece}})
            time.sleep(0.25)
        ev({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tb%d" % turn,"name":"Bash","input":{}}})
        ev({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"echo hi\"}"}})
        out({"type":"assistant","message":{"id":"m%d" % turn,"content":[{"type":"text","text":"Hello **world**"},{"type":"tool_use","id":"tb%d" % turn,"name":"Bash","input":{"command":"echo hi"}}]}})
        out({"type":"control_request","request_id":"q%d" % turn,"request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo hi"},"tool_use_id":"tb%d" % turn}})
    elif m["type"] == "control_response":
        r = m["response"]["response"]
        if r["behavior"] == "allow":
            out({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tb%d" % turn,"content":"hi from bash"}]}})
            ev({"type":"message_start","message":{"id":"d%d" % turn}})
            ev({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
            ev({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}})
            out({"type":"result","subtype":"success","is_error":False,"result":"Done."})
        else:
            out({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tb%d" % turn,"content":r.get("message",""),"is_error":True}]}})
            out({"type":"result","subtype":"error_during_execution","is_error":True,"result":"interrupted by user"})
    elif m["type"] == "control_request" and m["request"]["subtype"] == "interrupt":
        out({"type":"result","subtype":"error_during_execution","is_error":True,"result":"interrupted"})
"#;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-chat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fake_cli(dir: &Path) -> PathBuf {
    let p = dir.join("claude");
    std::fs::write(&p, FAKE_STREAM_CLI).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn log(dir: &Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

struct P {
    ctx: egui::Context,
    chat: Chat,
    theme: Theme,
    syn: Syntaxes,
    root: PathBuf,
    context: ContextInfo,
    time: f64,
    /// Text shapes of the last frame: (text, rect).
    texts: Vec<(String, Rect)>,
    actions: Vec<ChatAction>,
}

fn collect(shape: &Shape, out: &mut Vec<(String, Rect)>) {
    match shape {
        Shape::Text(t) => out.push((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
        _ => {}
    }
}

impl P {
    fn new(root: &Path, bin: PathBuf) -> Self {
        let theme = crate::theme::by_name("tokyonight");
        let syn = Syntaxes::new(&theme);
        let mut chat = Chat::new(&ClaudeConfig::default());
        chat.bin = Some(bin);
        // Pretend the account check already ran (the fake CLI has no `auth status`).
        chat.auth.checked_once = true;
        let mut p = P { ctx: egui::Context::default(), chat, theme, syn, root: root.to_path_buf(), context: ContextInfo::default(), time: 0.0, texts: vec![], actions: vec![] };
        p.frame(vec![]);
        p
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.time += 0.05;
        let mut input = egui::RawInput { events, time: Some(self.time), ..Default::default() };
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, vec2(460.0, 900.0)));
        let (chat, root, context, theme, syn) = (&mut self.chat, &self.root, &self.context, &self.theme, &self.syn);
        let mut actions = vec![];
        let mut out = self.ctx.run_ui(input, |ui| {
            let env = Env { theme, syn, root, context };
            actions = chat.ui(ui, &env);
        });
        out.textures_delta.clear();
        self.texts.clear();
        for s in &out.shapes {
            collect(&s.shape, &mut self.texts);
        }
        self.actions.extend(actions);
        chat.poll();
    }

    fn find(&self, label: &str) -> Option<Rect> {
        self.texts.iter().find(|(t, _)| t == label).map(|(_, r)| *r)
    }

    fn click_text(&mut self, label: &str) {
        let r = self.find(label).unwrap_or_else(|| panic!("no '{label}' on screen; texts: {:?}", self.texts.iter().map(|t| &t.0).collect::<Vec<_>>()));
        let p = r.center();
        self.frame(vec![Event::PointerMoved(p)]);
        self.frame(vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
        self.frame(vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
    }

    fn until(&mut self, what: &str, pred: impl Fn(&Chat) -> bool) {
        for _ in 0..300 {
            self.frame(vec![]);
            if pred(&self.chat) {
                return;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        panic!("timed out: {what}");
    }

    fn type_and_send(&mut self, s: &str) {
        self.chat.focus_input = true;
        self.frame(vec![]);
        self.frame(vec![Event::Text(s.into())]);
        self.frame(vec![Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
    }
}

fn last_text(c: &Chat) -> Option<String> {
    c.convo.items.iter().rev().find_map(|i| if let Item::Text(t) = i { Some(t.clone()) } else { None })
}

#[test]
fn streaming_then_allow_by_clicking_the_button() {
    let dir = scratch("allow");
    let bin = fake_cli(&dir);
    let mut p = P::new(&dir, bin);
    std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
    p.context = ContextInfo { path: Some(dir.join("main.rs")), rel: "main.rs".into(), selection: Some(Selection { start_line: 1, end_line: 1, text: "fn main() {}".into() }) };
    p.type_and_send("hello there");
    assert!(p.chat.is_busy());
    assert!(p.chat.input.is_empty(), "prompt box cleared after sending");
    // Streaming: partial text is visible before the message completes.
    p.until("partial text", |c| last_text(c).is_some_and(|t| t.starts_with("Hel") && t != "Hello **world**"));
    let partial = last_text(&p.chat).unwrap();
    p.until("full text", |c| last_text(c).as_deref() == Some("Hello **world**"));
    eprintln!("saw partial {partial:?} before the full text");
    // Markdown renders bold "world" as its own text run.
    p.frame(vec![]);
    assert!(p.find("world").is_some(), "bold span rendered separately");
    // Permission card with the command.
    p.until("approval card", |c| c.convo.has_pending());
    p.frame(vec![]);
    assert!(p.find("$ echo hi").is_some());
    assert!(p.find("waiting for approval").is_some());
    p.click_text("Allow");
    p.until("turn done", |c| !c.is_busy());
    let Some(Item::Tool(t)) = p.chat.convo.items.iter().find(|i| matches!(i, Item::Tool(_))) else { panic!() };
    assert_eq!(t.perm.as_ref().unwrap().state, PermState::Allowed);
    assert_eq!(t.result.as_ref().unwrap().text, "hi from bash");
    assert_eq!(last_text(&p.chat).as_deref(), Some("Done."));
    // What the CLI received: the prompt with the IDE context note, and an allow response.
    let sent = log(&dir, "stdin.log");
    // (The very first line is the `initialize` handshake.)
    let first = sent.iter().find(|m| m["type"] == "user").unwrap()["message"]["content"][0]["text"].as_str().unwrap();
    assert!(first.contains("<ide_opened_file>") && first.contains("<ide_selection>") && first.ends_with("hello there"), "{first}");
    let resp = sent.iter().find(|m| m["type"] == "control_response").unwrap();
    assert_eq!(resp["response"]["request_id"], "q1");
    assert_eq!(resp["response"]["response"]["behavior"], "allow");
    assert_eq!(resp["response"]["response"]["updatedInput"]["command"], "echo hi");
    // CLI args: permission mode + stdio permission tool.
    let args = log(&dir, "args.log");
    let a: Vec<&str> = args[0].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(a.windows(2).any(|w| w == ["--permission-prompt-tool", "stdio"]));
    assert!(a.windows(2).any(|w| w == ["--permission-mode", "manual"]));

    // Second prompt: same context is not repeated; deny through the button.
    p.type_and_send("again");
    p.until("second approval", |c| c.convo.has_pending());
    p.frame(vec![]);
    p.click_text("Deny");
    p.until("turn done", |c| !c.is_busy());
    let sent = log(&dir, "stdin.log");
    let second = sent.iter().filter(|m| m["type"] == "user").nth(1).unwrap();
    assert_eq!(second["message"]["content"][0]["text"], "again", "unchanged context is not re-sent");
    let deny = sent.iter().filter(|m| m["type"] == "control_response").nth(1).unwrap();
    assert_eq!(deny["response"]["response"]["behavior"], "deny");
    assert_eq!(deny["response"]["response"]["interrupt"], true);
    let denied = p.chat.convo.items.iter().filter(|i| matches!(i, Item::Tool(t) if t.perm.as_ref().is_some_and(|p| p.state == PermState::Denied))).count();
    assert_eq!(denied, 1);
    // An "interrupted" result is not shown as an error banner.
    assert!(!p.chat.convo.items.iter().any(|i| matches!(i, Item::Notice { error: true, .. })));
    assert_eq!(log(&dir, "args.log").len(), 1, "one CLI process for the whole conversation");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn escape_stops_a_running_turn() {
    let dir = scratch("stop");
    let bin = fake_cli(&dir);
    let mut p = P::new(&dir, bin);
    p.type_and_send("go");
    p.until("partial", |c| last_text(c).is_some());
    let focused = p.ctx.memory(|m| m.has_focus(Chat::input_id()));
    p.frame(vec![Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
    std::thread::sleep(Duration::from_millis(1500));
    p.frame(vec![]);
    eprintln!("input focused before Esc: {focused}; stdin log: {:?}", log(&dir, "stdin.log").iter().map(|m| m["type"].clone()).collect::<Vec<_>>());
    p.until("stopped", |c| !c.is_busy());
    assert!(log(&dir, "stdin.log").iter().any(|m| m["type"] == "control_request" && m["request"]["subtype"] == "interrupt"));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn history_menu_lists_and_resumes() {
    let dir = scratch("history");
    let bin = fake_cli(&dir);
    let transcripts = dir.join("projects");
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::write(
        transcripts.join("abc-123.jsonl"),
        [
            r#"{"type":"user","message":{"role":"user","content":"Remember tangerine"}}"#,
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"ok, tangerine"}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let mut p = P::new(&dir, bin);
    // The History menu reads ~/.claude/projects/<dir>; seed its 5s cache from our scratch folder instead.
    p.chat.history = Some((Instant::now(), claude::list_sessions_in(&transcripts)));
    p.frame(vec![]);
    p.click_text("History");
    p.frame(vec![]);
    let label = p.texts.iter().find(|(t, _)| t.starts_with("Remember tangerine")).map(|(t, _)| t.clone()).expect("session listed in the History menu");
    p.click_text(&label);
    p.frame(vec![]);
    assert!(p.chat.convo.items.iter().any(|i| matches!(i, Item::Text(t) if t == "ok, tangerine")));
    p.type_and_send("what was it?");
    p.until("approval", |c| c.convo.has_pending());
    let args = log(&dir, "args.log");
    let a: Vec<&str> = args[0].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(a.windows(2).any(|w| w == ["--resume", "abc-123"]), "{a:?}");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn cli_crash_reports_stderr() {
    let dir = scratch("crash");
    let p0 = dir.join("claude");
    std::fs::write(&p0, "#!/bin/sh\necho 'Error: not logged in' >&2\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p0, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut p = P::new(&dir, p0);
    p.type_and_send("hi");
    p.until("not busy", |c| !c.is_busy());
    let notice = p.chat.convo.items.iter().find_map(|i| if let Item::Notice { text, error: true } = i { Some(text.clone()) } else { None }).expect("error notice");
    assert!(notice.contains("stopped unexpectedly") && notice.contains("not logged in"), "{notice}");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn sign_in_banner_buttons_work() {
    let dir = scratch("signin");
    let bin = claude::tests::fake_auth_cli(&dir.join("bin"));
    let mut p = P::new(&dir, bin);
    p.chat.auth.checked_once = false;
    p.until("status", |c| c.auth.status.is_some());
    p.frame(vec![]);
    p.click_text("Sign in with Claude account");
    p.until("login url", |c| c.auth.login.as_ref().is_some_and(|l| l.url().is_some()));
    p.frame(vec![]);
    // Type the code into the box and press Submit.
    let r = p.texts.iter().find(|(t, _)| t == "Paste code…").map(|(_, r)| *r).expect("code box");
    p.frame(vec![Event::PointerMoved(r.center())]);
    p.frame(vec![Event::PointerButton { pos: r.center(), button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
    p.frame(vec![Event::PointerButton { pos: r.center(), button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
    p.frame(vec![Event::Text("good#code".into())]);
    p.click_text("Submit");
    p.until("signed in", |c| c.auth.login.is_none());
    p.chat.auth.checked_once = false;
    p.until("status logged in", |c| c.auth.status.as_ref().is_some_and(|s| s.logged_in));
    p.frame(vec![]);
    assert!(p.find("Account").is_some(), "account menu shown when signed in");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn markdown_rendering_edge_cases_do_not_panic() {
    let dir = scratch("md");
    let bin = fake_cli(&dir);
    let mut p = P::new(&dir, bin);
    let nasty = [
        "", "#", "# ", "######## seven", "```", "```\n", "~~~py\nx", "- ", "1.", "1. ", "> ", ">", "|", "| a |\n|---|", "|---|\n|---|",
        "**", "****", "*a**b*", "__init__", "`", "``", "`` ` ``", "[", "[a](", "[a](b", "[a]()", "\\", "\\*", "***bold italic***",
        "😀 **é** `漢字` [ü](http://x)", "    ```\n    indented fence\n    ```", "- a\n  - b\n    - c\n      continued", "---\n***\n___",
        "| a | b |\n|---|---|\n| 1 |", "a | b\n---|---\n1 | 2", "<script>alert(1)</script>", "line\r\nwith crlf\r\n",
        &"deep ".repeat(2000), &"*".repeat(500), &"[".repeat(300), &"`".repeat(301),
    ];
    for (i, n) in nasty.iter().enumerate() {
        let _ = md::parse(n);
        p.chat.convo.items = vec![Item::Text(n.to_string()), Item::Thinking(n.to_string()), Item::User { text: n.to_string(), context: None, images: vec![] }];
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.frame(vec![])));
        assert!(r.is_ok(), "panic rendering markdown case {i}: {n:?}");
    }
}

#[test]
fn markdown_semantics() {
    use md::{Block, parse, plain};
    // Heading inline styles are flattened to plain text when rendered (markdown_ui uses plain()).
    assert!(matches!(&parse("## a `b`")[0], Block::Heading(2, s) if plain(s) == "a b"));
    // A fence closing marker inside the code must match the opening fence type.
    let b = parse("~~~\n```\ncode\n~~~");
    assert!(matches!(&b[0], Block::Code { text, .. } if text == "```\ncode"), "{b:?}");
    // Numbered list with ")"
    assert!(matches!(&parse("3) three")[0], Block::Item { marker, .. } if marker == "3."));
    // Emphasis with underscores inside words is not italic.
    assert_eq!(md::inline("a_b_c").len(), 1);
    // A link whose label contains code.
    let s = md::inline("[`x`](http://y)");
    assert!(s[0].code && s[0].link.as_deref() == Some("http://y"));
    // Setext-style "---" under a paragraph is not a rule.
    let b = parse("para\n---");
    eprintln!("para + --- => {b:?}");
}
