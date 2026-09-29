//! Modal dialogs: unsaved changes, save conflicts, trash, discard, inputs, settings.

use super::*;

impl App {
    /// A dialog went away: forget pending closes, flush settings, give the keyboard back.
    pub(super) fn dialog_closed(&mut self, dialog: &Dialog) {
        if let Dialog::SaveConflict { doc_id } | Dialog::CloseDirty { doc_id } = dialog {
            self.close_after_save.remove(doc_id);
        }
        if self.cfg_dirty {
            self.save_cfg();
        }
        self.refocus_frames = 2;
    }

    pub(super) fn dialogs(&mut self) {
        let Some(dialog) = self.dialog.take() else { return };
        // Esc cancels before the modal is drawn: a modal drawn this frame would still block
        // focusing the editor on the next one.
        if self.ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.dialog_closed(&dialog);
            return;
        }
        let t = self.theme.clone();
        let mut keep = true;
        let mut dialog = dialog;
        let ctx = self.ctx.clone();
        let resp = egui::Modal::new(Id::new("rim-dialog")).show(&ctx, |ui| {
            ui.set_width(380.0);
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            let esc = ui.input(|i| i.key_pressed(Key::Escape));
            match &mut dialog {
                Dialog::CloseDirty { doc_id } => {
                    let doc_id = *doc_id;
                    let title = self.doc_index(doc_id).map(|i| self.docs[i].title.clone()).unwrap_or_default();
                    ui.label(RichText::new(format!("Save changes to {title}?")).size(15.0));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() || enter {
                            if let Some(i) = self.doc_index(doc_id) {
                                self.save(i, false);
                                if !self.docs[i].is_dirty() {
                                    self.close_doc(i);
                                }
                            }
                            keep = false;
                        }
                        if ui.button("Don't Save").clicked() {
                            if let Some(i) = self.doc_index(doc_id) {
                                self.close_doc(i);
                            }
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() || esc {
                            keep = false;
                        }
                    });
                }
                Dialog::QuitDirty => {
                    let n = self.docs.iter().filter(|d| d.is_dirty()).count();
                    ui.label(RichText::new(format!("{n} file(s) have unsaved changes.")).size(15.0));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save All & Quit").clicked() || enter {
                            for i in 0..self.docs.len() {
                                if self.docs[i].is_dirty() {
                                    self.save(i, false);
                                }
                            }
                            if !self.docs.iter().any(|d| d.is_dirty()) {
                                self.quit_confirmed = true;
                                self.ctx.send_viewport_cmd(ViewportCommand::Close);
                            }
                            keep = false;
                        }
                        if ui.button("Quit Without Saving").clicked() {
                            self.quit_confirmed = true;
                            self.ctx.send_viewport_cmd(ViewportCommand::Close);
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() || esc {
                            keep = false;
                        }
                    });
                }
                Dialog::SaveConflict { doc_id } => {
                    let doc_id = *doc_id;
                    let title = self.doc_index(doc_id).map(|i| self.docs[i].title.clone()).unwrap_or_default();
                    ui.label(RichText::new(format!("{title} changed on disk")).size(15.0));
                    ui.label(RichText::new("Another program modified it since you opened it here.").color(t.comment));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Overwrite").clicked() {
                            if let Some(i) = self.doc_index(doc_id) {
                                self.write_doc_checked(i, true);
                            }
                            keep = false;
                        }
                        if ui.button("Reload from disk").on_hover_text("Discard your changes here").clicked() {
                            if let Some(i) = self.doc_index(doc_id) {
                                self.reload_from_disk(i);
                            }
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() || esc {
                            keep = false;
                        }
                    });
                }
                Dialog::Trash(p) => {
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    ui.label(RichText::new(format!("Move '{name}' to the trash?")).size(15.0));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Move to Trash").clicked() || enter {
                            match crate::format::move_to_trash(p) {
                                Ok(()) => {
                                    if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(p)) {
                                        self.close_doc(i);
                                    }
                                    self.tree.invalidate();
                                    self.refresh_git(false);
                                }
                                Err(e) => self.error(e),
                            }
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() || esc {
                            keep = false;
                        }
                    });
                }
                Dialog::Settings => {
                    ui.label(RichText::new("Settings").size(17.0).strong());
                    ui.add_space(6.0);
                    let mut cfg = self.cfg.clone();
                    ui.checkbox(&mut cfg.vim_mode, "Vim motions");
                    ui.label(RichText::new("Modal editing (normal / insert / visual). Off means regular editing.").size(12.0).color(t.comment));
                    ui.add_space(6.0);
                    ui.checkbox(&mut cfg.format_on_save, "Format on save");
                    ui.checkbox(&mut cfg.insert_spaces, "Indent with spaces");
                    ui.horizontal(|ui| {
                        ui.label("Tab size");
                        ui.add(egui::DragValue::new(&mut cfg.tab_size).range(1..=8));
                        ui.add_space(12.0);
                        ui.label("Font size");
                        ui.add(egui::DragValue::new(&mut cfg.font_size).range(8.0..=40.0).speed(0.25));
                    });
                    egui::ComboBox::from_label("Theme").selected_text(cfg.theme.clone()).show_ui(ui, |ui| {
                        for th in theme::all() {
                            ui.selectable_value(&mut cfg.theme, th.name.to_string(), th.name);
                        }
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    ui.label(RichText::new("Claude").strong());
                    egui::ComboBox::from_label("Default mode").selected_text(chat::MODES.iter().find(|m| m.1 == cfg.claude.permission_mode).map(|m| m.0).unwrap_or("Ask before edits")).show_ui(ui, |ui| {
                        for (label, mode) in chat::MODES {
                            ui.selectable_value(&mut cfg.claude.permission_mode, mode.to_string(), label);
                        }
                    });
                    ui.checkbox(&mut cfg.claude.include_context, "Share the open file and selection with prompts");
                    let found = match &self.chat.bin {
                        Some(p) => format!("Claude Code: {}", p.display()),
                        None => "Claude Code not found (set claude.path in config.toml)".to_string(),
                    };
                    ui.label(RichText::new(found).size(11.5).color(t.comment));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Open config.toml").clicked() {
                            if self.cfg_dirty {
                                self.save_cfg();
                            }
                            Config::ensure_exists();
                            self.open_path(&Config::path());
                            keep = false;
                        }
                        if ui.button("Close").clicked() || esc {
                            keep = false;
                        }
                    });
                    let (vim_changed, theme_changed) = (cfg.vim_mode != self.cfg.vim_mode, cfg.theme != self.cfg.theme);
                    let any_changed = vim_changed
                        || theme_changed
                        || cfg.format_on_save != self.cfg.format_on_save
                        || cfg.insert_spaces != self.cfg.insert_spaces
                        || cfg.tab_size != self.cfg.tab_size
                        || cfg.font_size != self.cfg.font_size
                        || cfg.claude.permission_mode != self.cfg.claude.permission_mode
                        || cfg.claude.include_context != self.cfg.claude.include_context;
                    if any_changed {
                        if cfg.claude.permission_mode != self.cfg.claude.permission_mode {
                            self.chat.set_mode(&cfg.claude.permission_mode);
                        }
                        self.chat.include_context = cfg.claude.include_context;
                        self.cfg = cfg;
                        if vim_changed {
                            self.reset_vim();
                        }
                        if theme_changed {
                            self.set_theme(&self.cfg.theme.clone(), false);
                        }
                        // Written when the dialog closes, not on every frame of a drag.
                        self.cfg_dirty = true;
                    }
                }
                Dialog::Discard(f) => {
                    let what = if f.unstaged == Some('?') { "delete this untracked file" } else { "discard changes to" };
                    ui.label(RichText::new(format!("Really {what} {}?", f.path)).size(15.0));
                    ui.label(RichText::new("This cannot be undone.").color(t.red));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Discard").clicked() {
                            let f = f.clone();
                            self.git_do("checkout", |r| r.discard(&f));
                            self.tree.invalidate();
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() || esc {
                            keep = false;
                        }
                    });
                }
                Dialog::Input { kind, value, focus } => {
                    let label = match kind {
                        InputKind::NewFile(_) => "New file name",
                        InputKind::NewFolder(_) => "New folder name",
                        InputKind::Rename(_) => "Rename to",
                        InputKind::NewBranch => "New branch name",
                    };
                    ui.label(RichText::new(label).size(15.0));
                    let r = ui.add(TextEdit::singleline(value).desired_width(f32::INFINITY));
                    if *focus {
                        r.request_focus();
                        *focus = false;
                    }
                    let submit = r.lost_focus() && enter;
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if (ui.button("OK").clicked() || submit) && !value.trim().is_empty() {
                            let (kind, value) = (kind.clone(), value.trim().to_string());
                            self.submit_input(kind, value);
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() || esc {
                            keep = false;
                        }
                    });
                }
            }
        });
        if resp.should_close() {
            keep = false;
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        } else if !keep && self.dialog.is_none() {
            self.dialog_closed(&dialog);
        }
    }

    pub(super) fn submit_input(&mut self, kind: InputKind, value: String) {
        let res: Result<(), String> = match kind {
            InputKind::NewFile(dir) => {
                let p = dir.join(&value);
                if p.exists() {
                    Err("file already exists".into())
                } else {
                    let r = p.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::write(&p, ""));
                    r.map_err(|e| e.to_string()).map(|_| {
                        self.tree.invalidate();
                        self.open_path(&p);
                    })
                }
            }
            InputKind::NewFolder(dir) => std::fs::create_dir_all(dir.join(&value)).map_err(|e| e.to_string()),
            InputKind::Rename(p) => {
                let to = p.with_file_name(&value);
                if to.exists() && to != p {
                    Err(format!("{value} already exists"))
                } else {
                    std::fs::rename(&p, &to).map_err(|e| e.to_string()).map(|_| {
                        for d in &mut self.docs {
                            let Some(dp) = d.path.clone() else { continue };
                            let Ok(rest) = dp.strip_prefix(&p) else { continue };
                            let np = if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
                            // The language server knew the old name; it gets the new one on next sync.
                            let old_uri = lsp::uri(&dp);
                            for c in self.lsp.values_mut() {
                                c.did_close(&old_uri);
                            }
                            d.set_path(np, &self.syn);
                        }
                    })
                }
            }
            InputKind::NewBranch => self.repo.clone().ok_or("not a git repository".to_string()).and_then(|r| {
                r.create_branch(&value).map(|_| ())
            }),
        };
        if let Err(e) = res {
            self.error(e);
        }
        self.tree.invalidate();
        self.refresh_git(true);
    }
}
