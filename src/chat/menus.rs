//! The panel's chrome: header with the account menu, the "⋯" settings menu, the sign-in
//! banner, the setup note and the Account & usage window.

use std::time::Instant;

use egui::{Align, CornerRadius, Frame, Key, Label, Layout, Margin, RichText, ScrollArea, Stroke, TextEdit, Ui};

use super::render::ago;
use super::{Chat, ChatAction, Env};
use crate::claude::{self, EFFORT_LEVELS, SessionInfo};

impl Chat {
    /// A note about where the CLI comes from, with a way to install a standalone copy when it
    /// is missing or borrowed from an editor extension (whose path changes on every update).
    pub(super) fn setup_note(&mut self, ui: &mut Ui, env: &Env) {
        let t = env.theme;
        let extension = self.bin.as_deref().is_some_and(claude::is_extension_copy);
        if self.bin.is_some() && !extension {
            return;
        }
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            if extension {
                ui.add(Label::new(RichText::new("Using the copy bundled with the VS Code extension; it moves on every extension update.").size(12.0).color(t.comment)).wrap());
            }
            if self.installing.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Installing Claude Code…").size(12.0).color(t.comment));
                });
            } else {
                let label = if extension { "Install a standalone copy" } else { "Install Claude Code" };
                if ui.button(label).on_hover_text("Runs the official installer (curl -fsSL https://claude.ai/install.sh | bash), which puts claude in ~/.local/bin").clicked() {
                    self.install_standalone(&ui.ctx().clone());
                }
            }
        });
    }


    /// Banner shown while signed out or signing in.
    pub(super) fn auth_ui(&mut self, ui: &mut Ui, env: &Env) {
        let t = env.theme;
        let signing_in = self.auth.login.is_some();
        let signed_out = self.auth.status.as_ref().is_some_and(|s| !s.logged_in);
        if !signing_in && !signed_out && self.auth.error.is_none() {
            return;
        }
        let ctx = ui.ctx().clone();
        Frame::new().fill(t.bg_highlight).stroke(Stroke::new(1.0, t.yellow)).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(e) = self.auth.error.clone() {
                ui.label(RichText::new(e).color(t.red));
                ui.add_space(4.0);
            }
            if let Some(login) = &self.auth.login {
                let url = login.url();
                match &url {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Starting sign-in…");
                        });
                    }
                    Some(url) => {
                        ui.label(RichText::new("Finish signing in").strong().color(t.fg));
                        ui.label(RichText::new("1. Your browser should have opened. If not:").size(12.5).color(t.fg_dim));
                        ui.horizontal(|ui| {
                            if ui.button("Open sign-in page").clicked() {
                                crate::format::open_external(url);
                            }
                            if ui.button("Copy link").clicked() {
                                ctx.copy_text(url.clone());
                            }
                        });
                        ui.label(RichText::new("2. Sign in, then copy the code the page shows.").size(12.5).color(t.fg_dim));
                        ui.label(RichText::new("3. Paste it here:").size(12.5).color(t.fg_dim));
                    }
                }
                if url.is_some() {
                    let mut submit = false;
                    ui.horizontal(|ui| {
                        let r = ui.add(TextEdit::singleline(&mut self.auth.code).hint_text("Paste code…").desired_width(ui.available_width() - 150.0));
                        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        submit |= ui.add_enabled(!self.auth.code.trim().is_empty(), egui::Button::new("Submit").fill(t.green.gamma_multiply(0.3))).clicked();
                    });
                    if submit && !self.auth.code.trim().is_empty() {
                        let code = std::mem::take(&mut self.auth.code);
                        if let Some(l) = self.auth.login.as_mut() {
                            l.submit_code(&code);
                        }
                    }
                }
                if ui.button("Cancel").clicked() {
                    self.auth.login = None;
                }
            } else if signed_out {
                ui.label(RichText::new("Claude isn't signed in").strong().color(t.fg));
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.add(egui::Button::new("Sign in with Claude account").fill(t.blue.gamma_multiply(0.35))).clicked() {
                        self.start_login(&ctx, false);
                    }
                    if ui.button("Use API billing (Console)").clicked() {
                        self.start_login(&ctx, true);
                    }
                });
            }
        });
        ui.add_space(4.0);
    }


    pub(super) fn info_window(&mut self, ctx: &egui::Context, env: &Env) {
        if !self.info_open {
            return;
        }
        let t = env.theme;
        let mut open = self.info_open;
        let mut refresh = false;
        egui::Window::new("Account & usage").open(&mut open).collapsible(false).resizable(true).default_width(420.0).show(ctx, |ui| {
            if let Some(a) = &self.account {
                ui.label(RichText::new(a.email.as_deref().unwrap_or("Signed in")).strong());
                let mut line = Vec::new();
                if let Some(p) = &a.plan {
                    line.push(p.clone());
                }
                if let Some(o) = &a.organization {
                    line.push(o.clone());
                }
                if !line.is_empty() {
                    ui.label(RichText::new(line.join(" · ")).size(12.0).color(t.comment));
                }
                ui.add_space(6.0);
            }
            match &self.usage {
                Some(u) => {
                    if u.rows.is_empty() {
                        ui.label(RichText::new("No usage limits reported (API-key accounts are billed per token instead).").color(t.fg_dim));
                    }
                    for r in &u.rows {
                        ui.horizontal(|ui| {
                            let color = match r.severity.as_str() {
                                "critical" => t.red,
                                "warning" => t.yellow,
                                _ => t.blue,
                            };
                            ui.add(egui::ProgressBar::new(r.percent / 100.0).desired_width(140.0).fill(color).text(format!("{:.0}%", r.percent)));
                            ui.label(&r.label);
                            if let Some(at) = &r.resets_at {
                                ui.label(RichText::new(format!("resets {}", at.replace('T', " ").chars().take(16).collect::<String>())).size(11.5).color(t.comment));
                            }
                        });
                    }
                    if let Some(c) = u.session_cost_usd {
                        let on_plan = u.plan.is_some();
                        let text = if on_plan {
                            format!("This session would cost ${c:.2} at API prices (informational: your plan covers it, and counts it against the limits above)")
                        } else {
                            format!("This session: ${c:.2} of API usage")
                        };
                        ui.add(Label::new(RichText::new(text).size(12.0).color(t.comment)).wrap());
                    }
                }
                None if self.info_requested => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Loading usage…").color(t.comment));
                    });
                }
                None => {
                    ui.label(RichText::new("Usage is read from Claude Code; start a conversation first.").color(t.fg_dim));
                }
            }
            if !self.status.is_empty() {
                ui.add_space(6.0);
                ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for (title, rows) in &self.status {
                        egui::CollapsingHeader::new(title).id_salt(("status", title)).default_open(title == "Session").show(ui, |ui| {
                            egui::Grid::new(("status-grid", title)).num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                                for (label, value) in rows {
                                    ui.label(RichText::new(label).color(t.comment));
                                    ui.add(Label::new(RichText::new(value).color(t.fg)).wrap());
                                    ui.end_row();
                                }
                            });
                        });
                    }
                });
            }
            ui.add_space(6.0);
            if let Some(b) = &self.bin {
                ui.label(RichText::new(format!("Claude Code: {}", b.display())).size(11.5).color(t.comment));
            }
            self.setup_note(ui, env);
            ui.add_space(6.0);
            if ui.button("Refresh").clicked() {
                refresh = true;
            }
        });
        self.info_open = open;
        if refresh {
            self.show_info(env.root, ctx);
        }
    }

    /// The "⋯" menu: model, effort, thinking, fast mode, output style, export.
    pub(super) fn settings_menu(&mut self, ui: &mut Ui, env: &Env, actions: &mut Vec<ChatAction>) {
        let t = env.theme;
        let ctx = ui.ctx().clone();
        let root = env.root.to_path_buf();
        ui.menu_button("⋯", |ui| {
            ui.set_min_width(240.0);
            let mut new = self.settings.clone();
            let model = self.current_model().cloned();
            // Effort
            let levels: Vec<String> = model.as_ref().filter(|m| !m.effort_levels.is_empty()).map(|m| m.effort_levels.clone()).unwrap_or_else(|| EFFORT_LEVELS.iter().map(|s| s.to_string()).collect());
            ui.menu_button(format!("Effort: {}", new.effort.as_deref().unwrap_or("default")), |ui| {
                if ui.radio(new.effort.is_none(), "Model default").clicked() {
                    new.effort = None;
                    ui.close();
                }
                for l in &levels {
                    if ui.radio(new.effort.as_deref() == Some(l.as_str()), l).clicked() {
                        new.effort = Some(l.clone());
                        ui.close();
                    }
                }
                if !new.thinking {
                    ui.label(RichText::new("Higher effort needs thinking on").size(11.5).color(t.comment));
                }
            });
            ui.checkbox(&mut new.thinking, "Thinking").on_hover_text("Extended thinking before answering");
            let fast_ok = model.as_ref().is_none_or(|m| m.fast_mode);
            ui.add_enabled(fast_ok, egui::Checkbox::new(&mut new.fast_mode, "Fast mode")).on_hover_text("Faster output from the same model, at a higher price per token").on_disabled_hover_text("This model doesn't support fast mode");
            let mut switch = new.switch_models_on_flag.unwrap_or(false);
            if ui.checkbox(&mut switch, "Switch models when a message is flagged").on_hover_text("Continue on another model when a safeguard flags a message, instead of asking each time").changed() {
                new.switch_models_on_flag = Some(switch);
            }
            let styles: Vec<String> = if self.output_styles.is_empty() { vec!["default".into()] } else { self.output_styles.clone() };
            ui.menu_button(format!("Output style: {}", new.output_style.as_deref().unwrap_or("default")), |ui| {
                for st in &styles {
                    let sel = new.output_style.as_deref().unwrap_or("default") == st;
                    if ui.radio(sel, st).clicked() {
                        new.output_style = (st != "default").then(|| st.clone());
                        ui.close();
                    }
                }
            });
            if new != self.settings {
                self.set_settings(new.clone());
                actions.push(ChatAction::SaveSettings(new));
            }
            ui.separator();
            if ui.button("Export conversation…").clicked() {
                self.export(&root, &ctx);
                ui.close();
            }
            if ui.button("Account & usage…").clicked() {
                self.show_info(&root, &ctx);
                ui.close();
            }
            if self.session.is_some() {
                ui.label(RichText::new("Changes restart Claude Code and resume this conversation.").size(11.0).color(t.comment));
            }
        })
        .response
        .on_hover_text("Effort, thinking, fast mode, output style, export, usage");
    }

    pub(super) fn header(&mut self, ui: &mut Ui, env: &Env, actions: &mut Vec<ChatAction>) {
        let t = env.theme;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Claude").strong().size(15.0).color(t.orange));
            if let Some(m) = &self.convo.model {
                ui.label(RichText::new(m).size(11.5).color(t.comment));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new("×").frame(false)).on_hover_text("Close panel (Ctrl+L)").clicked() {
                    actions.push(ChatAction::Close);
                }
                self.settings_menu(ui, env, actions);
                let root = env.root.to_path_buf();
                ui.menu_button("History", |ui| {
                    if self.history.as_ref().is_none_or(|(at, _)| at.elapsed().as_secs() > 30) {
                        self.history = Some((Instant::now(), claude::list_sessions(&root)));
                    }
                    let list = self.history.as_ref().map(|h| &h.1[..]).unwrap_or(&[]);
                    if list.is_empty() {
                        ui.label(RichText::new("No previous conversations").color(t.comment));
                    }
                    let mut pick = None;
                    ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (i, s) in list.iter().enumerate() {
                            let title: String = s.title.chars().take(60).collect();
                            if ui.button(format!("{title}   {}", ago(s.modified))).clicked() {
                                pick = Some(i);
                            }
                        }
                    });
                    if let Some(i) = pick {
                        let info = &self.history.as_ref().unwrap().1[i];
                        let (id, path, title, modified) = (info.id.clone(), info.path.clone(), info.title.clone(), info.modified);
                        self.load_session(&SessionInfo { id, path, title, modified });
                        ui.close();
                    }
                });
                if let Some(st) = self.auth.status.clone().filter(|s| s.logged_in) {
                    let ctx = ui.ctx().clone();
                    ui.menu_button("Account", |ui| {
                        ui.label(RichText::new(st.email.as_deref().unwrap_or("Signed in")).strong());
                        if let Some(p) = &st.plan {
                            ui.label(RichText::new(format!("{p} plan")).size(12.0).color(t.comment));
                        }
                        if ui.button("Usage & status…").clicked() {
                            self.show_info(&root, &ctx);
                            ui.close();
                        }
                        ui.separator();
                        ui.label(RichText::new("Signing out also signs out Claude Code\nin VS Code and terminals.").size(11.5).color(t.comment));
                        if ui.button("Sign out").clicked() {
                            self.logout(&ctx);
                            ui.close();
                        }
                    });
                }
                if ui.button("New").on_hover_text("Start a new conversation").clicked() {
                    self.reset();
                    self.focus_input = true;
                }
                if let Some(u) = self.convo.usage_5h {
                    let color = if u > 0.9 { t.red } else if u > 0.7 { t.yellow } else { t.comment };
                    ui.label(RichText::new(format!("{:.0}% of 5h limit", u * 100.0)).size(11.5).color(color)).on_hover_text(match self.convo.usage_7d {
                        Some(w) => format!("Weekly usage: {:.0}%", w * 100.0),
                        None => "Usage of your plan's 5-hour window".into(),
                    });
                }
            });
        });
    }
}
