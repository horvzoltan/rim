//! Layout pieces: status bar, sidebar, tabs, find bar, editor area, palette and toasts.

use super::*;

impl App {
    pub(super) fn status_bar(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            if self.chat.is_busy() && !self.show_claude
                && ui.add(egui::Label::new(RichText::new("Claude is working…").color(t.orange)).sense(Sense::click())).clicked() {
                    self.show_claude = true;
                }
            if self.graph.scanning {
                ui.label(RichText::new("Scanning project graph…").color(t.comment));
            }
            if let Some((done, total)) = self.describe_progress() {
                let label = RichText::new(format!("Describing {done}/{total}…")).color(t.orange);
                if ui.add(egui::Label::new(label).sense(Sense::click())).on_hover_text("Open the project graph").clicked() && !self.show_graph {
                    self.toggle_graph();
                }
            }
            if let (true, Some(d)) = (self.cfg.vim_mode, self.active.and_then(|i| self.docs.get(i))) {
                let color = match d.vim.mode {
                    vim::Mode::Normal => t.blue,
                    vim::Mode::Insert => t.green,
                    vim::Mode::Visual | vim::Mode::VisualLine => t.purple,
                };
                ui.label(RichText::new(format!(" {} ", d.vim.label())).strong().color(t.bg_dark).background_color(color));
                let pending = d.vim.pending();
                if !pending.is_empty() {
                    ui.label(RichText::new(pending).monospace().color(t.fg_dim));
                }
            }
            if self.repo.is_some() {
                let mut label = self.git.branch.clone();
                if self.git.ahead > 0 || self.git.behind > 0 {
                    label += &format!("  +{} -{}", self.git.ahead, self.git.behind);
                }
                if ui.add(egui::Label::new(RichText::new(label).color(t.purple)).sense(Sense::click())).on_hover_text("Checkout branch").clicked() {
                    self.run(Cmd::GitCheckout);
                }
                if let Some(op) = &self.git_busy {
                    ui.label(RichText::new(format!("git {op}…")).color(t.comment));
                }
            }
            if let Some(i) = self.active {
                let main_server = if self.docs[i].path.is_some() { self.servers_for_doc(i).into_iter().next() } else { None };
                let suggest = if main_server.is_none() { self.suggested_package(i) } else { None };
                let d = &self.docs[i];
                if let Some(p) = &d.path {
                    let diags = self.diagnostics.get(&lsp::uri(p)).map(|v| v.as_slice()).unwrap_or(&[]);
                    let errs = diags.iter().filter(|d| d.severity == 1).count();
                    let warns = diags.iter().filter(|d| d.severity == 2).count();
                    ui.label(RichText::new(format!("✖ {errs}")).color(if errs > 0 { t.red } else { t.comment }));
                    ui.label(RichText::new(format!("⚠ {warns}")).color(if warns > 0 { t.yellow } else { t.comment }));
                    if let Some(p) = suggest {
                        let label = RichText::new(format!("Install {} support…", p.languages)).color(t.comment);
                        if ui.add(egui::Label::new(label).sense(Sense::click())).on_hover_text(format!("Set up {} in Settings → Language Servers", p.name)).clicked() {
                            self.open_language_servers(Some(p.id));
                        }
                    }
                    if let Some(s) = &main_server {
                        if let Some(c) = self.lsp.get(&s.name) {
                            let text = c.progress_text().map(|p| format!("{}: {p}", s.name)).unwrap_or(s.name.clone());
                            let text: String = text.chars().take(80).collect();
                            ui.label(RichText::new(text).color(if c.ready { t.fg_dim } else { t.comment }));
                        } else if self.lsp_failed.contains(&s.name) {
                            ui.label(RichText::new(format!("{} unavailable", s.name)).color(t.comment));
                        }
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Label::new(RichText::new(self.theme.name).color(t.comment)).sense(Sense::click())).clicked() {
                    self.run(Cmd::Theme);
                }
                if self.cfg.format_on_save {
                    ui.label(RichText::new("fmt on save").color(t.comment));
                }
                if let Some(i) = self.active {
                    let d = &self.docs[i];
                    ui.label(RichText::new(&d.syntax).color(t.fg_dim));
                    let indent = if self.cfg.insert_spaces { format!("Spaces: {}", self.cfg.tab_size) } else { "Tabs".into() };
                    ui.label(RichText::new(indent).color(t.fg_dim));
                    let (l, c) = d.line_col();
                    if ui.add(egui::Label::new(RichText::new(format!("Ln {}, Col {}", l + 1, c + 1)).color(t.fg_dim)).sense(Sense::click())).clicked() {
                        self.run(Cmd::GotoLine);
                    }
                }
            });
        });
    }

    pub(super) fn sidebar_ui(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let panels = [
                (Sidebar::Explorer, Icon::Files, "Explorer (Ctrl+Shift+E)", Cmd::Explorer),
                (Sidebar::Git, Icon::Git, "Source Control (Ctrl+Shift+G)", Cmd::SourceControl),
            ];
            for (s, icon, tip, cmd) in panels {
                let badge = if s == Sidebar::Git { self.git.files.len() } else { 0 };
                if icon_button(ui, &t, icon, self.sidebar == s, badge).on_hover_text(tip).clicked() {
                    self.run(cmd);
                }
            }
            let tip = if self.show_claude { "Close Claude" } else { "Claude (Ctrl+L)" };
            if icon_button(ui, &t, Icon::Claude, self.show_claude, 0).on_hover_text(tip).clicked() {
                if self.show_claude {
                    self.show_claude = false;
                    self.focus_editor = true;
                } else {
                    self.claude_toggle();
                }
            }
            let tip = if self.show_graph { "Back to the editor (Esc)" } else { "Project Graph (Ctrl+Shift+M)" };
            if icon_button(ui, &t, Icon::Graph, self.show_graph, 0).on_hover_text(tip).clicked() {
                self.run(Cmd::ProjectGraph);
            }
        });
        ui.add_space(2.0);
        ui.separator();
        match self.sidebar {
            Sidebar::Explorer => {
                let name = self.root.file_name().map(|n| n.to_string_lossy().to_uppercase()).unwrap_or_default();
                ui.label(RichText::new(name).size(11.5).color(t.comment).strong());
                if let Some(a) = self.tree.show(ui, &t, &self.file_colors) {
                    self.tree_action(a);
                }
            }
            Sidebar::Git => self.git_ui(ui),
        }
    }

    pub(super) fn tree_action(&mut self, a: TreeAction) {
        match a {
            TreeAction::Open(p) => {
                self.open_path(&p);
            }
            TreeAction::NewFile(d) => {
                self.dialog = Some(Dialog::Input { kind: InputKind::NewFile(d), value: String::new(), focus: true })
            }
            TreeAction::NewFolder(d) => {
                self.dialog = Some(Dialog::Input { kind: InputKind::NewFolder(d), value: String::new(), focus: true })
            }
            TreeAction::Rename(p) => {
                let value = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                self.dialog = Some(Dialog::Input { kind: InputKind::Rename(p), value, focus: true })
            }
            TreeAction::Delete(p) => self.dialog = Some(Dialog::Trash(p)),
        }
    }

    pub(super) fn tab_bar(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        let mut activate = None;
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("tabs").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (i, d) in self.docs.iter().enumerate() {
                    let active = self.active == Some(i);
                    let color = d
                        .path
                        .as_ref()
                        .and_then(|p| self.file_colors.get(p))
                        .copied()
                        .unwrap_or(if active { t.fg } else { t.comment });
                    let font = FontId::proportional(13.0);
                    let title_w = ui.ctx().fonts_mut(|f| f.layout_no_wrap(d.title.clone(), font.clone(), color).size().x);
                    let w = title_w + 44.0;
                    let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
                    ui.painter().rect_filled(rect, 0.0, if active { t.bg } else { t.bg_dark });
                    if active {
                        ui.painter().line_segment([rect.left_top(), rect.right_top()], Stroke::new(2.0, t.blue));
                    }
                    ui.painter().line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, t.border));
                    ui.painter().text(pos2(rect.left() + 12.0, rect.center().y), Align2::LEFT_CENTER, &d.title, font, color);
                    let cr = egui::Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(18.0, 18.0));
                    let cresp = ui.interact(cr, resp.id.with("close"), Sense::click());
                    if cresp.hovered() {
                        ui.painter().rect_filled(cr, 3.0, t.bg_highlight);
                    }
                    if d.is_dirty() && !cresp.hovered() {
                        ui.painter().circle_filled(cr.center(), 4.0, t.fg_dim);
                    } else if resp.hovered() || cresp.hovered() || active {
                        ui.painter().text(cr.center(), Align2::CENTER_CENTER, "×", FontId::proportional(15.0), t.fg_dim);
                    }
                    if cresp.clicked() || resp.middle_clicked() {
                        close = Some(i);
                    } else if resp.clicked() {
                        activate = Some(i);
                    }
                    if let Some(p) = &d.path {
                        resp.on_hover_text(p.to_string_lossy());
                    }
                }
            });
        });
        if let Some(i) = activate {
            self.activate(i);
        }
        if let Some(i) = close {
            self.request_close(i);
        }
    }

    pub(super) fn find_bar(&mut self, ui: &mut Ui) {
        let Some(i) = self.active else { return };
        let t = self.theme.clone();
        let mut nav: Option<bool> = None;
        // Esc closes the bar whether the find box or the editor has focus. Checked before the text
        // field runs, because it gives up focus on Esc.
        let mut close = !self.palette.open && !self.find_files.open && self.dialog.is_none() && ui.input(|i| i.key_pressed(Key::Escape));
        Frame::NONE.fill(t.bg_dark).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
            ui.horizontal(|ui| {
                let resp = ui.add(TextEdit::singleline(&mut self.find.query).hint_text("Find").desired_width(260.0));
                if self.find.focus {
                    resp.request_focus();
                    self.find.focus = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    nav = Some(!ui.input(|i| i.modifiers.shift));
                    resp.request_focus();
                }
                let n = self.find.matches.len();
                let label = match self.find.current {
                    Some(c) if n > 0 => format!("{} of {n}", c + 1),
                    _ if self.find.query.is_empty() => String::new(),
                    _ => format!("{n} results"),
                };
                ui.label(RichText::new(label).color(if n == 0 && !self.find.query.is_empty() { t.red } else { t.comment }));
                if ui.button("↑").clicked() {
                    nav = Some(false);
                }
                if ui.button("↓").clicked() {
                    nav = Some(true);
                }
                if ui.button("×").clicked() {
                    close = true;
                }
            });
        });
        // Recompute matches when text or query changes.
        let d = &self.docs[i];
        let key = (d.id, d.hash, self.find.query.clone());
        if key != self.find.cache {
            self.find.matches = editor::find_matches(&d.text, &self.find.query);
            self.find.cache = key;
            self.find.current = None;
        }
        if let Some(forward) = nav {
            let ms = &self.find.matches;
            if !ms.is_empty() {
                let cursor = self.docs[i].cursor_char;
                let idx = match (forward, self.find.current) {
                    (true, Some(c)) => (c + 1) % ms.len(),
                    (false, Some(c)) => (c + ms.len() - 1) % ms.len(),
                    (true, None) => ms.iter().position(|m| m.0 >= cursor).unwrap_or(0),
                    (false, None) => ms.iter().rposition(|m| m.0 < cursor).unwrap_or(ms.len() - 1),
                };
                self.find.current = Some(idx);
                self.docs[i].pending_sel = Some(ms[idx]);
            }
        }
        if close {
            self.find.open = false;
            self.focus_editor = true;
        }
    }

    pub(super) fn welcome(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.label(RichText::new("rim").size(56.0).color(t.blue));
            ui.label(RichText::new(self.root.to_string_lossy()).color(t.comment));
            ui.add_space(24.0);
            for (k, v) in [
                ("Ctrl+P", "Go to file"),
                ("Ctrl+Shift+P", "Command palette"),
                ("Ctrl+Shift+F", "Search in files"),
                ("Ctrl+Shift+G", "Source control"),
                ("Ctrl+O", "Open folder"),
                ("Ctrl+N", "New file"),
            ] {
                ui.label(RichText::new(format!("{k:>14}   {v:<18}")).monospace().color(t.fg_dim));
            }
        });
    }

    pub(super) fn central(&mut self, ui: &mut Ui) {
        if self.show_graph {
            self.graph_ui(ui);
            return;
        }
        if self.docs.is_empty() {
            self.welcome(ui);
            return;
        }
        self.tab_bar(ui);
        if self.find.open {
            self.find_bar(ui);
        }
        let Some(i) = self.active else { return };
        if self.debug_ui.open {
            let t = self.theme.clone();
            egui::Panel::bottom("rim_debug")
                .resizable(true)
                .default_size(240.0)
                .size_range(120.0..=700.0)
                .frame(Frame::NONE.fill(t.bg_dark).inner_margin(Margin::symmetric(8, 4)))
                .show(ui, |ui| self.debug_panel(ui));
        }
        let main_server = self.servers_for_doc(i).into_iter().next().map(|s| s.name);
        let uri = self.docs[i].path.as_ref().map(|p| lsp::uri(p));
        let diags: &[Diagnostic] = uri.as_ref().and_then(|u| self.diagnostics.get(u)).map(|v| v.as_slice()).unwrap_or(&[]);
        let triggers: &[String] = main_server
            .and_then(|s| self.lsp.get(&s))
            .map(|c| c.trigger_characters())
            .unwrap_or(&[]);
        let (matches, current) = if self.find.open && self.find.cache.0 == self.docs[i].id {
            (self.find.matches.as_slice(), self.find.current.map(|c| self.find.matches[c]))
        } else {
            (&[][..], None)
        };
        let doc_path = self.docs[i].path.clone();
        let breakpoints: Vec<usize> = doc_path.as_ref().map(|p| self.debug.lines_for(p)).unwrap_or_default();
        let exec_line = if self.debug.phase == crate::debug::Phase::Stopped {
            self.debug.frames.get(self.debug.frame_sel).filter(|f| f.path.is_some() && f.path == doc_path).map(|f| f.line)
        } else {
            None
        };
        let mut cx = EditorCtx {
            theme: &self.theme,
            syn: &self.syn,
            font: FontId::monospace(self.cfg.font_size),
            tab_size: self.cfg.tab_size.max(1),
            insert_spaces: self.cfg.insert_spaces,
            diagnostics: diags,
            completion: &mut self.completion,
            hover: &mut self.hover,
            matches,
            current_match: current,
            request_focus: std::mem::take(&mut self.focus_editor) && !self.palette.open && !self.find_files.open,
            triggers,
            vim: if self.cfg.vim_mode { Some(&mut self.vim_shared) } else { None },
            breakpoints: &breakpoints,
            exec_line,
        };
        let events = editor::show(ui, &mut self.docs[i], &mut cx);

        // Tell the server about edits before asking it anything.
        self.sync_lsp();
        let doc_id = self.docs[i].id;
        for e in events {
            match e {
                EditorEvent::Complete { anchor } => {
                    let c = self.docs[i].cursor_char;
                    self.lsp_request(i, "textDocument/completion", c, Pending::Completion { doc_id, anchor });
                }
                EditorEvent::Hover(ci) => {
                    self.lsp_request(i, "textDocument/hover", ci, Pending::Hover { doc_id, char_idx: ci })
                }
                EditorEvent::Definition(ci) => self.lsp_request(i, "textDocument/definition", ci, Pending::Definition),
                EditorEvent::CompletionAccepted { item, at } => self.resolve_completion(i, item, at),
                EditorEvent::Save => self.save(i, true),
                EditorEvent::SaveClose => {
                    // Closes once the file is really written (formatting on save is asynchronous).
                    self.close_after_save.insert(doc_id);
                    self.save(i, true);
                    if self.docs.get(i).is_some_and(|d| d.id == doc_id && !d.is_dirty() && !d.read_only) {
                        self.close_after_save.remove(&doc_id);
                        self.close_doc(i);
                    }
                    return;
                }
                EditorEvent::Close(force) => {
                    if force {
                        self.close_doc(i);
                    } else {
                        self.request_close(i);
                    }
                    return;
                }
                EditorEvent::ToggleBreakpoint(line) => match self.docs[i].path.clone() {
                    Some(p) => self.debug.toggle_breakpoint(&p, line),
                    None => self.info("Save the file before setting breakpoints"),
                },
            }
        }
    }

    pub(super) fn palette_ui(&mut self) {
        let t = self.theme.clone();
        match self.palette.show(&self.ctx, &t) {
            Outcome::None => {}
            Outcome::Preview(name) => {
                self.theme = theme::by_name(&name);
                self.theme.apply(&self.ctx);
                self.syn.set_theme(&self.theme);
            }
            Outcome::Cancel(Mode::Themes { original }) => self.set_theme(&original, false),
            Outcome::Cancel(Mode::Mention) => self.chat.focus_input = true,
            Outcome::Cancel(_) => self.focus_editor = true,
            Outcome::Accept(mode, item, query) => {
                self.focus_editor = mode != Mode::Mention;
                match (mode, item) {
                    (Mode::Mention, Some(it)) => self.chat.insert_mention(&it.id),
                    (Mode::Mention, None) => self.chat.focus_input = true,
                    (Mode::Commands, Some(it)) => self.run_command(&it.id),
                    (Mode::Files, Some(it)) => {
                        let p = self.root.join(&it.id);
                        self.open_path(&p);
                    }
                    (Mode::Themes { .. }, Some(it)) => self.set_theme(&it.id, true),
                    (Mode::Themes { original }, None) => self.set_theme(&original, false),
                    (Mode::Branches, Some(it)) => {
                        let b = it.id.clone();
                        self.git_do("checkout", |r| r.checkout(&b));
                        self.tree.invalidate();
                    }
                    (Mode::CodeActions, Some(it)) => {
                        if let Ok(k) = it.id.parse::<usize>() {
                            self.run_code_action(k);
                        }
                    }
                    (Mode::GotoLine, _) => {
                        if let (Ok(n), Some(d)) = (query.trim().parse::<usize>(), self.active_doc()) {
                            d.goto_line(n.saturating_sub(1));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub(super) fn toasts_ui(&mut self) {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return;
        }
        let t = self.theme.clone();
        let screen = self.ctx.content_rect();
        egui::Area::new(Id::new("rim-toasts"))
            .order(egui::Order::Tooltip)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-12.0, -32.0))
            .show(&self.ctx, |ui| {
                ui.set_max_width((screen.width() * 0.45).min(520.0));
                let mut dismiss = None;
                for (i, toast) in self.toasts.iter().enumerate() {
                    let accent = if toast.kind == ToastKind::Error { t.red } else { t.blue };
                    let r = Frame::popup(ui.style())
                        .fill(t.bg_float)
                        .stroke(Stroke::new(1.0, accent))
                        .show(ui, |ui| ui.label(RichText::new(&toast.text).color(t.fg)))
                        .response;
                    if r.interact(Sense::click()).clicked() {
                        dismiss = Some(i);
                    }
                }
                if let Some(i) = dismiss {
                    self.toasts.remove(i);
                }
            });
        self.ctx.request_repaint_after(Duration::from_millis(500));
    }

    pub(super) fn update_title(&mut self) {
        let folder = self.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let title = match self.active.and_then(|i| self.docs.get(i)) {
            Some(d) => format!("{}{} — {folder} — rim", if d.is_dirty() { "● " } else { "" }, d.title),
            None => format!("{folder} — rim"),
        };
        if title != self.title {
            self.ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Files,
    Git,
    Claude,
    Graph,
}

/// A square sidebar button with a painted icon; `active` gets a background and an accent bar underneath,
/// `badge` > 0 shows a count in the top-right corner.
fn icon_button(ui: &mut Ui, t: &Theme, icon: Icon, active: bool, badge: usize) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 30.0), Sense::click());
    let p = ui.painter();
    if active || resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(5), t.bg_highlight);
    }
    let accent = if matches!(icon, Icon::Claude) { t.orange } else { t.blue };
    if active {
        let bar = egui::Rect::from_center_size(pos2(rect.center().x, rect.bottom() - 1.0), vec2(16.0, 2.0));
        p.rect_filled(bar, CornerRadius::same(1), accent);
    }
    let color = if active {
        if matches!(icon, Icon::Claude) { t.orange } else { t.fg }
    } else if resp.hovered() {
        t.fg_dim
    } else {
        t.comment
    };
    let c = rect.center() - vec2(0.0, 1.0);
    let s = Stroke::new(1.5, color);
    match icon {
        Icon::Files => {
            // Two stacked pages, the front one with a folded corner.
            let back = egui::Rect::from_min_size(c + vec2(-4.0, -8.0), vec2(10.0, 13.0));
            p.line_segment([back.left_top(), back.right_top()], s);
            p.line_segment([back.right_top(), back.right_bottom()], s);
            let (l, tp, r, b) = (c.x - 7.0, c.y - 5.0, c.x + 3.0, c.y + 8.0);
            let front = vec![pos2(l, tp), pos2(r - 3.0, tp), pos2(r, tp + 3.0), pos2(r, b), pos2(l, b)];
            p.add(egui::Shape::closed_line(front, s));
        }
        Icon::Git => {
            // A branch: trunk on the left, a side branch curving in from the right.
            let (a, b, side) = (c + vec2(-4.0, -7.0), c + vec2(-4.0, 7.0), c + vec2(5.0, -4.0));
            p.line_segment([a + vec2(0.0, 2.5), b - vec2(0.0, 2.5)], s);
            let curve = egui::epaint::CubicBezierShape::from_points_stroke(
                [side + vec2(0.0, 2.5), side + vec2(0.0, 7.0), a + vec2(0.0, 9.0), a + vec2(0.0, 12.0)],
                false,
                Color32::TRANSPARENT,
                s,
            );
            p.add(curve);
            for pt in [a, b, side] {
                p.circle_stroke(pt, 2.3, s);
            }
        }
        Icon::Graph => {
            // Three linked nodes: a small network.
            let (a, b, d) = (c + vec2(-6.0, 5.0), c + vec2(6.0, 5.0), c + vec2(0.0, -6.0));
            for (x, y) in [(a, b), (b, d), (d, a)] {
                p.line_segment([x, y], s);
            }
            for pt in [a, b, d] {
                p.circle_filled(pt, 2.6, color);
            }
        }
        Icon::Claude => {
            // Claude's starburst: rays of alternating length around the centre.
            let color = if active || resp.hovered() { t.orange } else { color };
            for i in 0..10 {
                let ang = i as f32 * std::f32::consts::TAU / 10.0 - std::f32::consts::FRAC_PI_2;
                let len = if i % 2 == 0 { 8.0 } else { 6.0 };
                let dir = vec2(ang.cos(), ang.sin());
                p.line_segment([c + dir * 1.5, c + dir * len], Stroke::new(1.8, color));
            }
        }
    }
    if badge > 0 {
        let text = if badge > 99 { "99+".to_string() } else { badge.to_string() };
        let font = FontId::proportional(9.5);
        let galley = p.layout_no_wrap(text, font, t.bg_dark);
        let w = (galley.size().x + 6.0).max(14.0);
        let pill = egui::Rect::from_center_size(pos2(rect.right() - w / 2.0 + 1.0, rect.top() + 6.0), vec2(w, 13.0));
        p.rect_filled(pill, CornerRadius::same(7), t.blue);
        p.galley(pill.center() - galley.size() / 2.0, galley, t.bg_dark);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
