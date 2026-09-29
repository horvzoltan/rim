//! Results of background work and the periodic housekeeping tick.

use super::*;

impl App {
    pub(super) fn poll_tasks(&mut self) {
        while let Ok(t) = self.tasks_rx.try_recv() {
            match t {
                Task::Git { op, result } => {
                    self.git_busy = None;
                    match result {
                        Ok(out) => self.info(out.trim().lines().last().unwrap_or("done").to_string()),
                        Err(e) => self.error(format!("git {op} failed: {e}")),
                    }
                    self.tree.invalidate();
                    self.refresh_git(true);
                }
                Task::Format { doc_id, base_hash, then_save, result } => {
                    self.apply_formatted(doc_id, base_hash, then_save, result)
                }
                Task::GitStatus { root, result, bases } => {
                    self.git_status_running = false;
                    // Ignore results for a folder that is no longer open.
                    if self.repo.as_ref().is_some_and(|r| r.root == root) {
                        self.apply_git_status(result, bases);
                    }
                    if let Some(reload) = self.git_status_queued.take() {
                        self.refresh_git(reload);
                    }
                }
                Task::Commit { result } => {
                    self.git_busy = None;
                    match result {
                        Ok(out) => {
                            self.commit_msg.clear();
                            self.info(out.lines().next().unwrap_or("Committed").to_string());
                        }
                        Err(e) => self.error(format!("Commit failed: {e}")),
                    }
                    self.refresh_git(true);
                }
                Task::LspInstall { folder, event } => self.lsp_install_event(folder, event),
                Task::Search { seq, results, truncated } => self.find_files.set_results(seq, results, truncated),
                Task::Files { root, files } => {
                    self.file_list_running = false;
                    if root == self.root {
                        if self.palette.open && matches!(self.palette.mode, Mode::Files | Mode::Mention) {
                            self.palette.set_items(files.iter().map(|f| Item::new(f.clone(), f.clone(), "")).collect());
                        }
                        // Files came or went: the graph is stale.
                        if self.graph.data.as_ref().is_some_and(|g| g.file_count != files.len()) {
                            self.graph.dirty = true;
                        }
                        self.file_list = Some((Instant::now(), files));
                    }
                }
                Task::Graph { root, graph, cache } => self.apply_graph(root, graph, cache),
                Task::Describe { root, rel, hash, result } => self.describe_result(root, rel, hash, result),
            }
        }
    }

    pub(super) fn periodic(&mut self) {
        self.ctx.request_repaint_after(Duration::from_secs(3));
        if self.last_tick.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.last_tick = Instant::now();
        self.tree.invalidate();
        self.refresh_git(false);
        self.reload_clean_docs();
        self.toasts.retain(|t| t.until > Instant::now());
    }

    /// Reload open files changed on disk (git checkout, Claude, formatters…) unless modified here.
    pub(super) fn reload_clean_docs(&mut self) {
        for d in &mut self.docs {
            let Some(p) = &d.path else { continue };
            let mtime = std::fs::metadata(p).and_then(|m| m.modified()).ok();
            if mtime.is_some() && mtime != d.mtime && !d.is_dirty()
                && let Ok(text) = std::fs::read_to_string(p) {
                    d.mtime = mtime;
                    if text != d.text {
                        let cursor = d.cursor_char;
                        d.set_text(text);
                        d.saved_hash = d.hash;
                        let c = cursor.min(d.index.total_chars);
                        d.pending_sel = Some((c, c));
                    }
                }
        }
    }
}
