//! Git: background status, commits, remote operations and the source-control panel.

use super::*;

impl App {
    pub(super) fn git_base(&self, path: &Path) -> Option<String> {
        let repo = self.repo.as_ref()?;
        repo.head_content(&repo.rel(path)?)
    }

    /// Refresh git status in the background. `reload_bases` also re-reads the HEAD version of
    /// every open file (for the gutter markers); that happens anyway when HEAD moved.
    pub(super) fn refresh_git(&mut self, reload_bases: bool) {
        let Some(repo) = self.repo.clone() else {
            self.git = Default::default();
            self.file_colors.clear();
            return;
        };
        if self.git_status_running {
            self.git_status_queued = Some(self.git_status_queued.unwrap_or(false) || reload_bases);
            return;
        }
        self.git_status_running = true;
        let prev_head = self.git.head.clone();
        let paths: Vec<PathBuf> = self.docs.iter().filter_map(|d| d.path.clone()).collect();
        let (tx, ctx) = (self.tasks_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = repo.status();
            let head_moved = result.as_ref().is_ok_and(|st| st.head != prev_head);
            let bases = (head_moved || reload_bases)
                .then(|| paths.into_iter().map(|p| (p.clone(), repo.rel(&p).and_then(|r| repo.head_content(&r)))).collect());
            let _ = tx.send(Task::GitStatus { root: repo.root.clone(), result, bases });
            ctx.request_repaint();
        });
    }

    pub(super) fn apply_git_status(&mut self, result: Result<git::Status, String>, bases: Option<Vec<(PathBuf, Option<String>)>>) {
        match result {
            Ok(st) => {
                self.git = st;
                self.git_error = None;
            }
            Err(e) => self.git_error = Some(e),
        }
        for (path, base) in bases.into_iter().flatten() {
            if let Some(d) = self.docs.iter_mut().find(|d| d.path.as_ref() == Some(&path)) {
                d.set_git_base(base);
            }
        }
        self.file_colors.clear();
        let Some(root) = self.repo.as_ref().map(|r| r.root.clone()) else { return };
        let t = &self.theme;
        for f in &self.git.files {
            let color = status_color(t, f.unstaged.or(f.staged).unwrap_or(' '));
            let abs = root.join(&f.path);
            let mut dir = abs.parent();
            while let Some(d) = dir {
                if !d.starts_with(&root) || self.file_colors.contains_key(d) {
                    break;
                }
                self.file_colors.insert(d.to_path_buf(), t.yellow.gamma_multiply(0.8));
                dir = d.parent();
            }
            self.file_colors.insert(abs, color);
        }
    }

    pub(super) fn git_do(&mut self, what: &str, f: impl FnOnce(&Repo) -> Result<String, String>) {
        let Some(repo) = self.repo.clone() else {
            self.error("Not a git repository");
            return;
        };
        if let Err(e) = f(&repo) {
            self.error(format!("git {what}: {e}"));
        }
        self.refresh_git(false);
    }

    pub(super) fn git_remote(&mut self, op: &str) {
        let Some(repo) = self.repo.clone() else { return };
        if self.git_busy.is_some() {
            return;
        }
        self.git_busy = Some(op.to_string());
        let (tx, ctx, op) = (self.tasks_tx.clone(), self.ctx.clone(), op.to_string());
        std::thread::spawn(move || {
            let result = Repo::remote_op(repo.root, &op);
            let _ = tx.send(Task::Git { op, result });
            ctx.request_repaint();
        });
    }

    pub(super) fn commit(&mut self) {
        let msg = self.commit_msg.trim().to_string();
        if msg.is_empty() {
            self.error("Commit message is empty");
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        if self.git_busy.is_some() {
            return;
        }
        // Commit hooks can take a while; don't freeze the window.
        self.git_busy = Some("commit".into());
        let (tx, ctx) = (self.tasks_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            // Ask git itself (the status shown may still be refreshing after a stage).
            let result = if repo.has_staged() { repo.commit(&msg) } else { Err("Nothing staged. Stage changes with + first.".into()) };
            let _ = tx.send(Task::Commit { result });
            ctx.request_repaint();
        });
    }

    pub(super) fn open_diff(&mut self, f: &FileStatus, staged: bool) {
        let Some(repo) = self.repo.clone() else { return };
        let text = repo.diff(f, staged);
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let title = format!("{name} ({})", if staged { "staged" } else { "diff" });
        self.open_virtual(format!("diff:{staged}:{}", f.path), title, text, "diff");
    }

    pub(super) fn git_ui(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        let Some(repo) = self.repo.clone() else {
            ui.label(RichText::new("This folder is not a git repository.").color(t.comment));
            if ui.button("Initialize Repository").clicked() {
                self.run(Cmd::GitInit);
            }
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.git.branch).color(t.purple).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let busy = self.git_busy.is_some();
                if ui.add_enabled(!busy, egui::Button::new("⟳").frame(false)).on_hover_text("Refresh").clicked() {
                    self.refresh_git(true);
                }
                if ui.add_enabled(!busy, egui::Button::new(format!("Push {}", self.git.ahead)).frame(false)).on_hover_text("git push").clicked() {
                    self.git_remote("push");
                }
                if ui.add_enabled(!busy, egui::Button::new(format!("Pull {}", self.git.behind)).frame(false)).on_hover_text("git pull").clicked() {
                    self.git_remote("pull");
                }
                if ui.add_enabled(!busy, egui::Button::new("Fetch").frame(false)).clicked() {
                    self.git_remote("fetch");
                }
            });
        });
        let resp = ui.add(
            TextEdit::multiline(&mut self.commit_msg)
                .hint_text("Commit message (Ctrl+Enter)")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        if resp.has_focus() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
            self.commit();
        }
        let staged_n = self.git.files.iter().filter(|f| f.staged.is_some()).count();
        if ui.add_sized(vec2(ui.available_width(), 26.0), egui::Button::new(format!("Commit ({staged_n} staged)"))).clicked() {
            self.commit();
        }
        if let Some(e) = &self.git_error {
            ui.label(RichText::new(e).color(t.red));
        }
        ui.add_space(4.0);

        // Borrowed out and put back below, instead of cloning the list every frame.
        let files = std::mem::take(&mut self.git.files);
        let staged: Vec<&FileStatus> = files.iter().filter(|f| f.staged.is_some()).collect();
        let unstaged: Vec<&FileStatus> = files.iter().filter(|f| f.unstaged.is_some()).collect();
        let mut action: Option<(&str, FileStatus)> = None;
        let mut bulk = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (is_staged, list) in [(true, &staged), (false, &unstaged)] {
                if is_staged && list.is_empty() {
                    continue;
                }
                ui.horizontal(|ui| {
                    let title = if is_staged { "STAGED CHANGES" } else { "CHANGES" };
                    ui.label(RichText::new(format!("{title}  {}", list.len())).size(11.5).strong().color(t.comment));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (icon, tip) = if is_staged { ("−", "Unstage all") } else { ("+", "Stage all") };
                        if !list.is_empty() && ui.add(egui::Button::new(icon).frame(false)).on_hover_text(tip).clicked() {
                            bulk = Some(is_staged);
                        }
                    });
                });
                for f in list.iter() {
                    let code = if is_staged { f.staged } else { f.unstaged }.unwrap_or(' ');
                    let code_color = status_color(&t, code);
                    let (name, dir) = match f.path.rsplit_once('/') {
                        Some((d, n)) => (n, d),
                        None => (f.path.as_str(), ""),
                    };
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, 0.0, t.bg_highlight);
                    }
                    let p = ui.painter().with_clip_rect(rect);
                    let y = rect.center().y;
                    let r = p.text(pos2(rect.left() + 6.0, y), Align2::LEFT_CENTER, name, FontId::proportional(13.5), t.fg);
                    p.text(pos2(r.right() + 8.0, y), Align2::LEFT_CENTER, dir, FontId::proportional(11.5), t.comment);
                    p.text(pos2(rect.right() - 6.0, y), Align2::RIGHT_CENTER, code.to_string(), FontId::monospace(13.0), code_color);
                    if resp.hovered() {
                        // Inline actions on hover.
                        let mut x = rect.right() - 26.0;
                        let buttons: Vec<(&str, &str)> = if is_staged {
                            vec![("−", "unstage")]
                        } else {
                            vec![("+", "stage"), ("↺", "discard")]
                        };
                        for (icon, act) in buttons {
                            let br = egui::Rect::from_center_size(pos2(x - 8.0, y), vec2(20.0, 20.0));
                            let b = ui.interact(br, resp.id.with(act), Sense::click());
                            let bg = if b.hovered() { t.selection } else { t.bg_highlight };
                            ui.painter().rect_filled(br, 3.0, bg);
                            ui.painter().text(br.center(), Align2::CENTER_CENTER, icon, FontId::proportional(14.0), t.fg);
                            if b.clicked() {
                                action = Some((act, (*f).clone()));
                            }
                            x -= 24.0;
                        }
                    }
                    if resp.double_clicked() {
                        action = Some(("open", (*f).clone()));
                    } else if resp.clicked() && action.is_none() {
                        action = Some((if is_staged { "diff_staged" } else { "diff" }, (*f).clone()));
                    }
                }
                ui.add_space(6.0);
            }
            if files.is_empty() {
                ui.label(RichText::new("No changes").color(t.comment));
            }
        });
        drop(staged);
        drop(unstaged);
        self.git.files = files;
        if let Some(staged) = bulk {
            if staged {
                self.git_do("reset", |r| r.unstage_all());
            } else {
                self.git_do("add", |r| r.stage_all());
            }
        }
        if let Some((act, f)) = action {
            match act {
                "stage" => self.git_do("add", |r| r.stage(&f.path)),
                "unstage" => self.git_do("reset", |r| r.unstage(&f.path)),
                "discard" => self.dialog = Some(Dialog::Discard(f)),
                "open" => {
                    self.open_path(&repo.root.join(&f.path));
                }
                "diff" => self.open_diff(&f, false),
                "diff_staged" => self.open_diff(&f, true),
                _ => {}
            }
        }
    }
}
