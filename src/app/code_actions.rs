//! Auto-imports and quick fixes: resolving accepted completions for their import edits,
//! the quick-fix menu (LSP code actions), and applying workspace edits from servers.

use super::*;
use crate::lsp::CodeAction;

/// Quick-fix requests in flight and the actions collected so far.
#[derive(Default)]
pub(super) struct QuickFix {
    pub seq: u64,
    /// Servers that haven't answered the current request yet.
    pub waiting: usize,
    pub actions: Vec<CodeAction>,
}

fn prefix_hash(text: &str, chars: usize) -> u64 {
    let end = text.char_indices().nth(chars).map(|(b, _)| b).unwrap_or(text.len());
    crate::text::hash_str(&text[..end])
}

/// Quick fixes before refactorings, preferred ones first, then as the server listed them.
fn rank(a: &CodeAction) -> u8 {
    match (a.preferred, a.kind.starts_with("quickfix")) {
        (true, _) => 0,
        (false, true) => 1,
        _ if a.kind.starts_with("source") => 3,
        _ => 2,
    }
}

impl App {
    // ---------- completions ----------

    /// A completion was accepted without extra edits: ask the server for the item's details,
    /// which is where servers like typescript-language-server put the import it needs.
    pub(super) fn resolve_completion(&mut self, i: usize, item: lsp::CompletionItem, at: usize) {
        let Some(name) = self.ensure_lsp(i) else { return };
        let Some(c) = self.lsp.get_mut(&name) else { return };
        if !c.ready || c.capabilities["completionProvider"]["resolveProvider"].as_bool() != Some(true) || item.raw.is_null() {
            return;
        }
        let d = &self.docs[i];
        let pending = Pending::ResolveCompletion { doc_id: d.id, at, prefix_hash: prefix_hash(&d.text, at) };
        c.request("completionItem/resolve", item.raw, pending);
    }

    /// Apply the import edits from a resolved completion, if the text they refer to is unchanged:
    /// only edits before the completion's start, and only while nothing before it was edited.
    pub(super) fn apply_resolved_completion(&mut self, doc_id: u64, at: usize, hash: u64, result: &Value) {
        let edits = lsp::text_edits(&result["additionalTextEdits"]);
        let Some(i) = self.doc_index(doc_id).filter(|_| !edits.is_empty()) else { return };
        let d = &mut self.docs[i];
        if d.read_only || prefix_hash(&d.text, at) != hash {
            return;
        }
        let before_start = edits.iter().all(|(_, e, _)| d.index.lsp_to_char(&d.text, e.0, e.1) <= at);
        if before_start {
            editor::apply_text_edits(d, &edits);
        }
    }

    // ---------- quick fixes ----------

    /// Ask every language server of the active file for code actions at the cursor or selection,
    /// passing each the diagnostics it reported there. Answers fill the quick-fix menu.
    pub(super) fn quick_fix(&mut self) {
        let Some(i) = self.active else { return };
        let Some(path) = self.docs[i].path.clone() else {
            self.info("Quick fixes need a saved file");
            return;
        };
        self.sync_lsp();
        let servers: Vec<String> = self.servers_for_doc(i).into_iter().map(|s| s.name).collect();
        let d = &self.docs[i];
        let uri = lsp::uri(&path);
        let (a, b) = d.sel;
        let (a, b) = if a == b { (d.cursor_char, d.cursor_char) } else { (a, b) };
        let start = d.index.char_to_lsp(&d.text, a);
        let end = d.index.char_to_lsp(&d.text, b);
        let cursor_line = start.0;
        // Diagnostics touching the range; with none there, those on the cursor's line.
        let touches = |g: &Diagnostic| g.start <= end && start <= g.end;
        let on_line = |g: &Diagnostic| g.start.0 <= cursor_line && cursor_line <= g.end.0;
        self.quick_fix.seq += 1;
        self.quick_fix.waiting = 0;
        self.quick_fix.actions.clear();
        let seq = self.quick_fix.seq;
        for name in servers {
            let diags: Vec<Diagnostic> = [false, true]
                .iter()
                .filter_map(|&pulled| self.lsp_diags.get(&(lsp_glue::diag_key(&name, pulled), uri.clone())))
                .flatten()
                .cloned()
                .collect();
            let mut context: Vec<&Diagnostic> = diags.iter().filter(|g| touches(g)).collect();
            if context.is_empty() {
                context = diags.iter().filter(|g| on_line(g)).collect();
            }
            // Ask for the whole span of those diagnostics, so fixes for them are offered.
            let (mut rs, mut re) = (start, end);
            for g in &context {
                rs = rs.min(g.start);
                re = re.max(g.end);
            }
            let Some(c) = self.lsp.get_mut(&name) else { continue };
            if c.ready && !c.supports("codeActionProvider") {
                continue;
            }
            let params = json!({
                "textDocument": { "uri": uri },
                "range": { "start": { "line": rs.0, "character": rs.1 }, "end": { "line": re.0, "character": re.1 } },
                "context": { "diagnostics": context.iter().map(|g| g.raw.clone()).collect::<Vec<_>>(), "triggerKind": 1 },
            });
            c.request("textDocument/codeAction", params, Pending::CodeActions { seq, server: name.clone() });
            self.quick_fix.waiting += 1;
        }
        if self.quick_fix.waiting == 0 {
            self.info("No language server offers quick fixes for this file");
        }
    }

    /// One server's answer (or failure, as `None`) to the current quick-fix request.
    pub(super) fn code_actions_arrived(&mut self, seq: u64, server: &str, result: Option<&Value>) {
        if seq != self.quick_fix.seq {
            return;
        }
        self.quick_fix.waiting = self.quick_fix.waiting.saturating_sub(1);
        if let Some(r) = result {
            self.quick_fix.actions.extend(lsp::parse_code_actions(r, server));
            self.quick_fix.actions.sort_by_key(rank);
        }
        let actions = &self.quick_fix.actions;
        if actions.is_empty() {
            if self.quick_fix.waiting == 0 {
                self.info("No quick fixes here");
            }
            return;
        }
        let items: Vec<Item> = actions
            .iter()
            .enumerate()
            .map(|(k, a)| {
                let detail = if a.kind.is_empty() { a.server.clone() } else { format!("{} · {}", a.kind, a.server) };
                Item::new(k.to_string(), a.title.clone(), detail)
            })
            .collect();
        if self.palette.open && self.palette.mode == Mode::CodeActions {
            self.palette.set_items(items);
        } else {
            self.palette.open(Mode::CodeActions, items, 0);
        }
    }

    /// Run the picked quick fix: resolve it if the server left out the edit, apply its edit,
    /// then run its command.
    pub(super) fn run_code_action(&mut self, idx: usize) {
        let Some(a) = self.quick_fix.actions.get(idx).cloned() else { return };
        let raw = &a.raw;
        let is_command = raw["command"].is_string();
        if !is_command && raw["edit"].is_null() && raw["command"].is_null() {
            let can_resolve = self
                .lsp
                .get(&a.server)
                .is_some_and(|c| c.capabilities["codeActionProvider"]["resolveProvider"].as_bool() == Some(true));
            if can_resolve {
                let c = self.lsp.get_mut(&a.server).unwrap();
                c.request("codeAction/resolve", raw.clone(), Pending::ResolveCodeAction { server: a.server.clone() });
            } else {
                self.error(format!("\"{}\" has nothing to apply", a.title));
            }
            return;
        }
        self.perform_code_action(&a.server, raw);
    }

    /// Apply a code action's edit and run its command (a bare `Command` is run directly).
    pub(super) fn perform_code_action(&mut self, server: &str, raw: &Value) {
        if raw["command"].is_string() {
            self.execute_command(server, raw);
            return;
        }
        if !raw["edit"].is_null()
            && let Err(e) = self.apply_workspace_edit(&raw["edit"])
        {
            self.error(format!("Couldn't apply \"{}\": {e}", raw["title"].as_str().unwrap_or("fix")));
            return;
        }
        if raw["command"].is_object() {
            self.execute_command(server, &raw["command"]);
        }
    }

    fn execute_command(&mut self, server: &str, cmd: &Value) {
        let Some(c) = self.lsp.get_mut(server) else { return };
        let mut params = json!({ "command": cmd["command"] });
        if !cmd["arguments"].is_null() {
            params["arguments"] = cmd["arguments"].clone();
        }
        c.request("workspace/executeCommand", params, Pending::ExecuteCommand);
    }

    /// Apply a `WorkspaceEdit` to the open documents. Files that aren't open are opened in a tab
    /// (left unsaved, so the change can be reviewed or undone); the active tab stays active.
    pub(super) fn apply_workspace_edit(&mut self, edit: &Value) -> Result<(), String> {
        let changes = lsp::workspace_edits(edit)?;
        let active = self.active.map(|i| self.docs[i].id);
        let mut others = 0;
        for (uri, edits) in changes {
            let Some(path) = lsp::uri_to_path(&uri) else { return Err(format!("not a file: {uri}")) };
            let path = path.canonicalize().unwrap_or(path);
            let i = match self.docs.iter().position(|d| d.path.as_ref() == Some(&path)) {
                Some(i) => i,
                None => self.open_path(&path).ok_or_else(|| format!("couldn't open {}", path.display()))?,
            };
            if self.docs[i].read_only {
                return Err(format!("{} is read-only", self.docs[i].title));
            }
            if editor::apply_text_edits(&mut self.docs[i], &edits) && Some(self.docs[i].id) != active {
                others += 1;
            }
        }
        if let Some(i) = active.and_then(|id| self.doc_index(id))
            && self.active != Some(i)
        {
            self.activate(i);
        }
        if others > 0 {
            self.info(format!("Also changed {others} other file(s); they are open and unsaved"));
        }
        self.focus_editor = true;
        Ok(())
    }
}
