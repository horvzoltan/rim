//! Rendering of the conversation: markdown, tool cards with approvals, inline diffs and the
//! empty state. Free functions that only emit `UiEvent`s, so the conversation is borrowed
//! immutably while drawing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use egui::text::LayoutJob;
use egui::{Align, Color32, CornerRadius, FontId, Frame, Label, Layout, Margin, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, vec2};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use super::{DiffLines, Env, NOT_FOUND, UiEvent, View};
use crate::claude::{Conversation, Item, PermState, Tool};
use crate::highlight::HlCache;
use crate::md::{self, Block, Span};
use crate::text::hash_str;

pub(super) fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

pub(super) fn empty_state(ui: &mut Ui, env: &Env, has_bin: bool) {
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

pub(super) fn rel(root: &Path, p: &str) -> String {
    Path::new(p).strip_prefix(root).map(|r| r.to_string_lossy().to_string()).unwrap_or_else(|_| p.to_string())
}

pub(super) fn abs(root: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { root.join(path) }
}

pub(super) fn items_ui(ui: &mut Ui, items: &[Item], busy: bool, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
    let t = env.theme;
    for (idx, item) in items.iter().enumerate() {
        ui.add_space(6.0);
        match item {
            Item::User { text, context, images } => {
                Frame::new().fill(t.bg_highlight).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if !images.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for im in images {
                                match &im.texture {
                                    Some(tex) => {
                                        let size = tex.size_vec2();
                                        let h = 72.0;
                                        ui.add(egui::Image::new((tex.id(), egui::vec2(size.x / size.y * h, h))).corner_radius(4.0)).on_hover_text(format!("{} ({}×{})", im.name, im.width, im.height));
                                    }
                                    None => {
                                        ui.label(RichText::new(format!("🖼 {}", im.name)).size(12.0).color(t.comment));
                                    }
                                }
                            }
                        });
                    }
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

pub(super) fn markdown_ui(ui: &mut Ui, text: &str, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
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
pub(super) fn file_ref(root: &Path, text: &str) -> Option<(PathBuf, Option<usize>)> {
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

pub(super) fn file_ref_uncached(root: &Path, text: &str) -> Option<(PathBuf, Option<usize>)> {
    let mut parts = text.split(':');
    let path = parts.next()?;
    let line = parts.next().and_then(|l| l.parse::<usize>().ok());
    let p = abs(root, path);
    (p.is_file() && p.starts_with(root)).then_some((p, line))
}

pub(super) fn inline_ui(ui: &mut Ui, spans: &[Span], env: &Env, ev: &mut Vec<UiEvent>, dim: bool) {
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

pub(super) fn highlighted(view: &mut View, env: &Env, lang: &str, text: &str, font: &FontId) -> LayoutJob {
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

pub(super) fn code_block(ui: &mut Ui, lang: &str, text: &str, view: &mut View, env: &Env) {
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

pub(super) fn tool_title(t: &Tool, root: &Path) -> (String, String) {
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

pub(super) fn tool_ui(ui: &mut Ui, idx: usize, tool: &Tool, busy: bool, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
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

pub(super) fn permission_buttons(ui: &mut Ui, tool: &Tool, env: &Env, ev: &mut Vec<UiEvent>) {
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

pub(super) fn ask_ui(ui: &mut Ui, tool: &Tool, view: &mut View, env: &Env, ev: &mut Vec<UiEvent>) {
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

pub(super) fn compute_diff(old: &str, new: &str) -> DiffLines {
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

pub(super) fn diff_ui(ui: &mut Ui, view: &mut View, key: (&str, usize), old: &str, new: &str, env: &Env) {
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
    c.items.insert(0, Item::User { text: "please fix it".into(), context: Some("src/main.rs lines 3-5".into()), images: vec![] });
    c.items.push(Item::Notice { text: "Something failed".into(), error: true });
    c
}
