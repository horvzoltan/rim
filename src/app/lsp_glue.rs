//! Formatting and language-server plumbing for the app.

use super::*;

/// Where a server's diagnostics are kept: pushed and pulled ones separately, so neither replaces the other.
pub(super) fn diag_key(server: &str, pulled: bool) -> String {
    if pulled { format!("{server}\u{1}pull") } else { server.to_string() }
}

/// The server a `diag_key` belongs to.
pub(super) fn diag_owner(key: &str) -> &str {
    key.strip_suffix("\u{1}pull").unwrap_or(key)
}

impl App {
    pub(super) fn format(&mut self, i: usize, then_save: bool) {
        let doc = &self.docs[i];
        let Some(path) = doc.path.clone() else { return };
        let cmd = self.cfg.formatters.get(&doc.ext).cloned();
        let has_prog = cmd.as_ref().and_then(|c| c.first()).is_some_and(|p| format::resolve_program(p, &self.root).is_some());
        if let (Some(cmd), true) = (cmd, has_prog) {
            let (tx, ctx, root) = (self.tasks_tx.clone(), self.ctx.clone(), self.root.clone());
            let (doc_id, base_hash, text) = (doc.id, doc.hash, doc.text.clone());
            std::thread::spawn(move || {
                let result = format::run_formatter(&cmd, &path, &root, &text);
                let _ = tx.send(Task::Format { doc_id, base_hash, then_save, result });
                ctx.request_repaint();
            });
            return;
        }
        let (doc_id, base_hash) = (doc.id, doc.hash);
        let (tab, spaces) = (self.cfg.tab_size, self.cfg.insert_spaces);
        if let Some(name) = self.ensure_lsp(i) {
            let c = self.lsp.get_mut(&name).unwrap();
            if !c.ready || c.supports("documentFormattingProvider") {
                c.request(
                    "textDocument/formatting",
                    json!({ "textDocument": { "uri": lsp::uri(&path) }, "options": { "tabSize": tab, "insertSpaces": spaces } }),
                    Pending::Formatting { doc_id, base_hash, then_save },
                );
                return;
            }
        }
        if then_save {
            self.write_doc(i);
        } else {
            let ext = self.docs[i].ext.clone();
            self.error(format!("No formatter available for .{ext} files (see settings)"));
        }
    }

    pub(super) fn apply_formatted(&mut self, doc_id: u64, base_hash: u64, then_save: bool, result: Result<String, String>) {
        let Some(i) = self.doc_index(doc_id) else { return };
        match result {
            Ok(text) if self.docs[i].hash == base_hash => {
                let d = &mut self.docs[i];
                if text != d.text {
                    let (line, col) = d.line_col();
                    d.set_text(text);
                    let l = line.min(d.index.line_count() - 1);
                    let c = d.index.char_starts[l] + col.min(d.index.line_text(&d.text, l).chars().count());
                    d.pending_sel = Some((c, c));
                }
            }
            Ok(_) => return, // edited meanwhile; drop the stale result
            Err(e) => self.error(format!("Format failed: {e}")),
        }
        if then_save {
            self.write_doc(i);
        }
    }

    /// Make sure the doc's language servers run and know the doc. Returns the main server's name
    /// (the one for completion, hover and formatting); add-on servers only report diagnostics.
    pub(super) fn ensure_lsp(&mut self, i: usize) -> Option<String> {
        self.docs[i].path.as_ref()?;
        let mut main = None;
        for (k, server) in self.servers_for_doc(i).into_iter().enumerate() {
            let started = self.ensure_server(i, &server);
            if k == 0 && started {
                main = Some(server.name);
            }
        }
        main
    }

    /// Start `server` if needed and open / update the doc in it.
    fn ensure_server(&mut self, i: usize, server: &config::ServerConfig) -> bool {
        let Some(path) = self.docs[i].path.clone() else { return false };
        if self.lsp_failed.contains(&server.name) {
            return false;
        }
        if !self.lsp.contains_key(&server.name) {
            let mut opts = server
                .init_json
                .clone()
                .or_else(|| server.initialization_options.as_ref().and_then(|t| serde_json::to_value(t).ok()))
                .unwrap_or_else(|| json!({}));
            let mut command = server.command.clone();
            // Only auto-detect when the user hasn't customized the command.
            if server.name == "typescript-language-server"
                && (server.managed || command.first().is_some_and(|c| c == "typescript-language-server"))
            {
                // rim's own copy of TypeScript (from its install), preferred over a global TypeScript 7.
                let bundled = opts["tsserver"]["fallbackPath"].as_str().map(PathBuf::from);
                let (cmd, ts_opts) = lsp::typescript_launch(&self.root, &command, bundled.as_deref());
                command = cmd;
                if let (Some(o), Value::Object(ts)) = (opts.as_object_mut(), ts_opts) {
                    for (k, v) in ts {
                        match (o.get_mut(&k), v) {
                            // The project's TypeScript wins over the bundled fallback.
                            (Some(Value::Object(mine)), Value::Object(theirs)) => mine.extend(theirs),
                            (_, v) => {
                                o.insert(k, v);
                            }
                        }
                    }
                }
            }
            let settings = server.settings.clone().unwrap_or(Value::Null);
            match lsp::Client::start_with(&command, &self.root, opts, settings, &server.env, self.ctx.clone()) {
                Ok(c) => {
                    self.lsp.insert(server.name.clone(), c);
                    self.lsp_gen += 1;
                }
                Err(e) => {
                    self.lsp_failed.insert(server.name.clone());
                    self.lsp_mgr.errors.insert(server.name.clone(), e.clone());
                    self.info(format!("{}: {e}", server.name));
                    return false;
                }
            }
        }
        let Some(client) = self.lsp.get_mut(&server.name) else { return false };
        let doc = &mut self.docs[i];
        let u = lsp::uri(&path);
        let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        // One version number per document, shared by its servers; each server gets the text it hasn't seen.
        if doc.lsp_hash != doc.hash || doc.lsp_version == 0 {
            doc.lsp_version += 1;
            doc.lsp_hash = doc.hash;
        }
        if !client.opened.contains(&u) {
            client.did_open(&u, config::language_id_for(&doc.ext, &file_name), doc.lsp_version, &doc.text);
        } else if client.version_sent(&u) != Some(doc.lsp_version) {
            client.did_change(&u, doc.lsp_version, &doc.text);
        }
        true
    }

    /// Keep language servers in sync with the open documents. Cheap when nothing changed:
    /// a document is only looked at again after an edit or when the set of servers changed.
    pub(super) fn sync_lsp(&mut self) {
        for i in 0..self.docs.len() {
            let d = &self.docs[i];
            if d.path.is_none() || d.read_only || (d.lsp_gen == self.lsp_gen && d.lsp_hash == d.hash) {
                continue;
            }
            self.ensure_lsp(i);
            let lsp_gen = self.lsp_gen;
            let d = &mut self.docs[i];
            d.lsp_gen = lsp_gen;
            d.lsp_hash = d.hash;
        }
    }

    pub(super) fn lsp_request(&mut self, i: usize, method: &str, char_idx: usize, pending: Pending) {
        let Some(name) = self.ensure_lsp(i) else { return };
        let d = &self.docs[i];
        let (line, character) = d.index.char_to_lsp(&d.text, char_idx);
        let u = lsp::uri(d.path.as_ref().unwrap());
        let params = json!({ "textDocument": { "uri": u }, "position": { "line": line, "character": character } });
        self.lsp.get_mut(&name).unwrap().request(method, params, pending);
    }

    pub(super) fn poll_lsp(&mut self) {
        let mut events = vec![];
        let mut failed = vec![];
        let mut apply = vec![];
        for (name, c) in self.lsp.iter_mut() {
            for e in c.poll() {
                match e {
                    LspEvent::ApplyEdit { id, edit } => apply.push((name.clone(), id, edit)),
                    LspEvent::Diagnostics { uri, diagnostics, pulled, .. } => {
                        events.push(LspEvent::Diagnostics { server: diag_key(name, pulled), uri, diagnostics, pulled })
                    }
                    LspEvent::Exited => failed.push((name.clone(), "exited".to_string())),
                    LspEvent::InitFailed(msg) => failed.push((name.clone(), format!("failed to start: {msg}"))),
                    e => events.push(e),
                }
            }
        }
        for (name, why) in failed {
            if self.lsp.remove(&name).is_some() {
                self.lsp_gen += 1;
                self.lsp_failed.insert(name.clone());
                self.error(format!("{name} {why}\n(restart via command palette)"));
            }
        }
        // Edits a server asks for (usually while running a quick fix's command).
        for (name, id, edit) in apply {
            let result = self.apply_workspace_edit(&edit);
            if let Err(e) = &result {
                self.error(format!("{name}: couldn't apply its edit: {e}"));
            }
            if let Some(c) = self.lsp.get_mut(&name) {
                c.reply(id, match result {
                    Ok(()) => json!({ "applied": true }),
                    Err(e) => json!({ "applied": false, "failureReason": e }),
                });
            }
        }
        for e in events {
            match e {
                LspEvent::Diagnostics { server, uri, diagnostics, .. } => self.set_diagnostics(server, uri, diagnostics),
                LspEvent::ShowMessage { level, message } => {
                    if level <= 1 {
                        self.error(message);
                    } else if level == 2 {
                        self.info(message);
                    }
                }
                LspEvent::Response { pending, result } => self.handle_response(pending, result),
                LspEvent::ApplyEdit { .. } | LspEvent::Exited | LspEvent::InitFailed(_) => {}
            }
        }
    }

    pub(super) fn handle_response(&mut self, pending: Pending, result: Result<Value, String>) {
        let result = match result {
            Ok(v) => v,
            Err(e) => {
                match pending {
                    Pending::Formatting { .. } => self.error(format!("Format failed: {e}")),
                    Pending::CodeActions { seq, server } => self.code_actions_arrived(seq, &server, None),
                    Pending::ResolveCodeAction { .. } | Pending::ExecuteCommand => self.error(format!("Quick fix failed: {e}")),
                    _ => {}
                }
                return;
            }
        };
        match pending {
            Pending::Completion { doc_id, anchor } => {
                let Some(i) = self.doc_index(doc_id) else { return };
                if self.active != Some(i) {
                    return;
                }
                let (items, incomplete) = lsp::parse_completions(&result);
                if items.is_empty() {
                    self.completion = None;
                    return;
                }
                let d = &self.docs[i];
                let cursor = d.cursor_char.max(anchor).min(d.index.total_chars);
                let prefix: String = d.text.chars().skip(anchor).take(cursor.saturating_sub(anchor)).collect();
                let mut c = Completion::new(doc_id, items, anchor, incomplete);
                c.refilter(&prefix);
                self.completion = (!c.filtered.is_empty()).then_some(c);
            }
            Pending::Hover { doc_id, char_idx } => {
                if let Some(h) = self.hover.as_mut().filter(|h| h.doc_id == doc_id && h.char_idx == char_idx) {
                    let t = lsp::hover_text(&result);
                    h.text = Some(t);
                }
            }
            Pending::Definition => {
                let Some((u, (line, col))) = lsp::first_location(&result) else {
                    self.info("No definition found");
                    return;
                };
                let Some(path) = lsp::uri_to_path(&u) else { return };
                if let Some(i) = self.open_path(&path) {
                    self.docs[i].goto_lsp(line, col);
                }
            }
            Pending::Formatting { doc_id, base_hash, then_save } => {
                let Some(i) = self.doc_index(doc_id) else { return };
                let d = &self.docs[i];
                let mut text = d.text.clone();
                // Byte ranges in the original text, via the line index (no whole-text scans per edit).
                let to_byte = |p: (usize, usize)| d.index.char_to_byte(&d.text, d.index.lsp_to_char(&d.text, p.0, p.1));
                let mut edits: Vec<(usize, usize, usize, String)> = result
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .enumerate()
                            .map(|(n, e)| {
                                let (s, en) = (to_byte(lsp::pos(&e["range"]["start"])), to_byte(lsp::pos(&e["range"]["end"])));
                                (s, en.max(s), n, e["newText"].as_str().unwrap_or("").to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Apply back to front; edits at the same position keep their order (per the LSP spec).
                edits.sort_by_key(|e| std::cmp::Reverse((e.0, e.2)));
                for (s, e, _, t) in edits {
                    text.replace_range(s..e, &t);
                }
                self.apply_formatted(doc_id, base_hash, then_save, Ok(text));
            }
            Pending::ResolveCompletion { doc_id, at, prefix_hash } => self.apply_resolved_completion(doc_id, at, prefix_hash, &result),
            Pending::CodeActions { seq, server } => self.code_actions_arrived(seq, &server, Some(&result)),
            Pending::ResolveCodeAction { server } => self.perform_code_action(&server, &result),
            // Pull-diagnostics answers are handled inside the client.
            Pending::PullDiagnostics { .. } | Pending::ExecuteCommand | Pending::Initialize => {}
        }
    }

    pub(super) fn restart_lsp(&mut self) {
        self.lsp_gen += 1;
        self.lsp.clear();
        self.lsp_failed.clear();
        self.diagnostics.clear();
        self.lsp_diags.clear();
        self.lsp_mgr.clear_cache();
        self.info("Language servers restarted");
    }

    /// Store one server's diagnostics for a document and rebuild the merged list.
    pub(super) fn set_diagnostics(&mut self, server: String, uri: String, diagnostics: Vec<Diagnostic>) {
        self.lsp_diags.insert((server, uri.clone()), diagnostics);
        let merged: Vec<Diagnostic> =
            self.lsp_diags.iter().filter(|((_, u), _)| *u == uri).flat_map(|(_, d)| d.iter().cloned()).collect();
        self.diagnostics.insert(uri, merged);
    }

    /// Stop a server (after an install, uninstall or when it's disabled) and forget its diagnostics.
    /// Open documents move to whatever server now fits on the next sync.
    pub(super) fn stop_server(&mut self, name: &str) {
        self.lsp.remove(name);
        self.lsp_failed.remove(name);
        self.lsp_mgr.errors.remove(name);
        let keys: Vec<(String, String)> = self.lsp_diags.keys().filter(|(s, _)| diag_owner(s) == name).cloned().collect();
        for (s, u) in keys {
            self.set_diagnostics(s.clone(), u.clone(), Vec::new());
            self.lsp_diags.remove(&(s, u));
        }
        self.lsp_mgr.clear_cache();
        self.lsp_gen += 1;
    }
}
