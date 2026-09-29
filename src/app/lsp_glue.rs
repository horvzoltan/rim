//! Formatting and language-server plumbing for the app.

use super::*;

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

    /// Make sure the doc's language server runs and knows the doc. Returns the server name.
    pub(super) fn ensure_lsp(&mut self, i: usize) -> Option<String> {
        let doc = &self.docs[i];
        let path = doc.path.clone()?;
        let server = self.cfg.server_for_ext(&doc.ext)?.clone();
        if self.lsp_failed.contains(&server.name) {
            return None;
        }
        if !self.lsp.contains_key(&server.name) {
            let mut opts = server
                .initialization_options
                .as_ref()
                .and_then(|t| serde_json::to_value(t).ok())
                .unwrap_or_else(|| json!({}));
            let mut command = server.command.clone();
            // Only auto-detect when the user hasn't customized the command.
            if server.name == "typescript-language-server" && command.first().is_some_and(|c| c == "typescript-language-server") {
                let (cmd, ts_opts) = lsp::typescript_launch(&self.root, &command);
                command = cmd;
                if let (Some(o), Value::Object(ts)) = (opts.as_object_mut(), ts_opts) {
                    for (k, v) in ts {
                        o.entry(k).or_insert(v);
                    }
                }
            }
            match lsp::Client::start(&command, &self.root, opts, self.ctx.clone()) {
                Ok(c) => {
                    self.lsp.insert(server.name.clone(), c);
                    self.lsp_gen += 1;
                }
                Err(e) => {
                    self.lsp_failed.insert(server.name.clone());
                    self.info(format!("{}: {e}", server.name));
                    return None;
                }
            }
        }
        let client = self.lsp.get_mut(&server.name)?;
        let doc = &mut self.docs[i];
        let u = lsp::uri(&path);
        if !client.opened.contains(&u) {
            doc.lsp_version = 1;
            doc.lsp_hash = doc.hash;
            client.did_open(&u, config::language_id(&doc.ext), doc.lsp_version, &doc.text);
        } else if doc.lsp_hash != doc.hash {
            doc.lsp_version += 1;
            doc.lsp_hash = doc.hash;
            client.did_change(&u, doc.lsp_version, &doc.text);
        }
        Some(server.name)
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
        for (name, c) in self.lsp.iter_mut() {
            for e in c.poll() {
                match e {
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
        for e in events {
            match e {
                LspEvent::Diagnostics { uri, diagnostics } => {
                    self.diagnostics.insert(uri, diagnostics);
                }
                LspEvent::ShowMessage { level, message } => {
                    if level <= 1 {
                        self.error(message);
                    } else if level == 2 {
                        self.info(message);
                    }
                }
                LspEvent::Response { pending, result } => self.handle_response(pending, result),
                LspEvent::Exited | LspEvent::InitFailed(_) => {}
            }
        }
    }

    pub(super) fn handle_response(&mut self, pending: Pending, result: Result<Value, String>) {
        let result = match result {
            Ok(v) => v,
            Err(e) => {
                if matches!(pending, Pending::Formatting { .. }) {
                    self.error(format!("Format failed: {e}"));
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
            Pending::Initialize => {}
        }
    }

    pub(super) fn restart_lsp(&mut self) {
        self.lsp_gen += 1;
        self.lsp.clear();
        self.lsp_failed.clear();
        self.diagnostics.clear();
        self.info("Language servers restarted");
    }
}
