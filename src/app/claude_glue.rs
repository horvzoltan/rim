//! Glue between the editor and the Claude panel: context, effects, focus.

use super::*;

impl App {
    /// The Claude context for the current frame, recomputed only when the document, its text
    /// or the selection changed (the panel needs it every frame for the context chip).
    pub(super) fn cached_context(&mut self) -> &ContextInfo {
        let key = self.active.and_then(|i| self.docs.get(i)).map(|d| (d.id, d.hash, d.sel));
        if self.ctx_cache.as_ref().is_none_or(|(k, _)| *k != key) {
            self.ctx_cache = Some((key, self.context_info()));
        }
        &self.ctx_cache.as_ref().unwrap().1
    }

    pub(super) fn context_info(&self) -> ContextInfo {
        let Some(d) = self.active.and_then(|i| self.docs.get(i)) else { return ContextInfo::default() };
        let Some(path) = d.path.clone() else { return ContextInfo::default() };
        let rel = path.strip_prefix(&self.root).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| path.to_string_lossy().to_string());
        let selection = d.selected_text().map(|text| {
            let (a, b) = d.sel;
            let (l1, _) = d.index.line_col(a);
            let (l2, _) = d.index.line_col(b);
            Selection { start_line: l1 + 1, end_line: l2 + 1, text: text.to_string() }
        });
        ContextInfo { path: Some(path), rel, selection }
    }

    pub(super) fn poll_claude(&mut self) {
        for e in self.chat.poll() {
            match e {
                claude::Effect::FileTouched(p) => {
                    let p = p.canonicalize().unwrap_or(p);
                    if let Some(d) = self.docs.iter().find(|d| d.path.as_ref() == Some(&p) && d.is_dirty()) {
                        let name = d.title.clone();
                        self.error(format!("Claude changed {name}, which has unsaved edits here. Save or reload to reconcile."));
                    }
                    self.tree.invalidate();
                    self.reload_clean_docs();
                    self.refresh_git(false);
                }
                claude::Effect::MaybeChanged => {
                    self.tree.invalidate();
                    self.reload_clean_docs();
                    self.refresh_git(false);
                }
                claude::Effect::PermissionNeeded => {
                    if !self.show_claude {
                        self.show_claude = true;
                        self.info("Claude is waiting for your approval");
                    }
                }
                claude::Effect::TurnDone => {
                    self.tree.invalidate();
                    self.reload_clean_docs();
                    self.refresh_git(false);
                }
            }
        }
    }

    /// Ctrl+L: open the panel, or move focus between chat and editor.
    pub(super) fn claude_toggle(&mut self) {
        let chat_focused = self.ctx.memory(|m| m.has_focus(Chat::input_id()));
        if !self.show_claude || !chat_focused {
            self.show_claude = true;
            self.chat.focus_input = true;
        } else {
            self.focus_editor = true;
        }
    }

    pub(super) fn open_mention_picker(&mut self) {
        self.open_file_picker(Mode::Mention);
    }

    /// Open a file picker right away with the cached file list, refreshing it in the background.
    pub(super) fn open_file_picker(&mut self, mode: Mode) {
        let cached = self.file_list.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
        let fresh = self.file_list.as_ref().is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(10));
        self.palette.open(mode, cached.into_iter().map(|f| Item::new(f.clone(), f, "")).collect(), 0);
        if !fresh && !self.file_list_running {
            self.file_list_running = true;
            let (tx, ctx, root) = (self.tasks_tx.clone(), self.ctx.clone(), self.root.clone());
            std::thread::spawn(move || {
                let files = search::list_files(&root, 50_000);
                let _ = tx.send(Task::Files { root, files });
                ctx.request_repaint();
            });
        }
    }
}
