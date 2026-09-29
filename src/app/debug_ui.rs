//! The debug panel (toolbar, call stack, variables, console), the configuration window, and
//! the glue between the debug session and the editor.

use egui::{Label, ScrollArea};

use super::*;
use crate::debug::{self, Effect, Kind, LaunchConfig, Phase, Var};

#[derive(Default)]
pub(super) struct DebugUi {
    pub open: bool,
    pub config_open: bool,
    draft: LaunchConfig,
    /// `command` of a custom adapter as one line.
    command_text: String,
    focus_eval: bool,
}

impl App {
    /// Apply what the session asked for (reveal the execution line, show the panel).
    pub(super) fn poll_debug(&mut self) {
        for fx in self.debug.poll() {
            match fx {
                Effect::Reveal { path, line } => {
                    if path.is_file() {
                        self.open_at(&path, line, 0, 0);
                    }
                }
                Effect::Attention => self.debug_ui.open = true,
            }
        }
    }

    /// F5: continue when stopped, otherwise start (after configuring, the first time).
    pub(super) fn debug_start(&mut self) {
        match self.debug.phase {
            Phase::Stopped => self.debug.continue_(),
            Phase::Building | Phase::Starting | Phase::Running => {}
            Phase::Idle | Phase::Ended => {
                if self.debug.needs_config() {
                    self.debug_configure();
                    return;
                }
                self.debug_ui.open = true;
                self.debug.start();
            }
        }
    }

    pub(super) fn debug_configure(&mut self) {
        let active = self.active.and_then(|i| self.docs[i].path.clone());
        self.debug_ui.draft = if self.debug.needs_config() { debug::detect(&self.root, active.as_deref()) } else { self.debug.config.clone() };
        if self.debug_ui.draft.kind == Kind::Custom && self.debug_ui.draft.command.is_empty() {
            self.debug_ui.draft.kind = Kind::Gdb;
        }
        self.debug_ui.command_text = self.debug_ui.draft.command.join(" ");
        self.debug_ui.config_open = true;
    }

    /// F9: a breakpoint on the cursor's line.
    pub(super) fn toggle_breakpoint_here(&mut self) {
        let Some(i) = self.active else { return };
        let Some(p) = self.docs[i].path.clone() else {
            self.info("Save the file before setting breakpoints");
            return;
        };
        let line = self.docs[i].index.line_of_char(self.docs[i].cursor_char.min(self.docs[i].index.total_chars));
        self.debug.toggle_breakpoint(&p, line);
    }

    pub(super) fn debug_config_window(&mut self) {
        if !self.debug_ui.config_open {
            return;
        }
        let t = self.theme.clone();
        let mut open = true;
        let mut action: Option<bool> = None; // Some(start)
        egui::Window::new("Debug configuration").open(&mut open).collapsible(false).resizable(false).default_width(460.0).show(&self.ctx.clone(), |ui| {
            let d = &mut self.debug_ui.draft;
            egui::Grid::new("debug-config").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Adapter");
                egui::ComboBox::from_id_salt("debug-kind").selected_text(d.kind.label()).width(220.0).show_ui(ui, |ui| {
                    for k in Kind::ALL {
                        ui.selectable_value(&mut d.kind, k, k.label());
                    }
                });
                ui.end_row();
                let program_label = match d.kind {
                    Kind::Gdb => "Program (executable)",
                    Kind::Debugpy => "Script",
                    Kind::Delve => "Package directory",
                    Kind::Custom => "Program",
                };
                ui.label(program_label);
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut d.program).desired_width(300.0));
                    if ui.button("…").clicked() {
                        let picked = if d.kind == Kind::Delve { rfd::FileDialog::new().set_directory(&self.root).pick_folder() } else { rfd::FileDialog::new().set_directory(&self.root).pick_file() };
                        if let Some(p) = picked {
                            d.program = p.strip_prefix(&self.root).unwrap_or(&p).display().to_string();
                        }
                    }
                });
                ui.end_row();
                ui.label("Arguments");
                ui.add(TextEdit::singleline(&mut d.args).desired_width(340.0).hint_text("--flag \"two words\""));
                ui.end_row();
                ui.label("Working directory");
                ui.add(TextEdit::singleline(&mut d.cwd).desired_width(340.0).hint_text("project root"));
                ui.end_row();
                ui.label("Build first");
                ui.add(TextEdit::singleline(&mut d.build).desired_width(340.0).hint_text("cargo build, make, …"));
                ui.end_row();
                if d.kind == Kind::Custom {
                    ui.label("Adapter command");
                    ui.add(TextEdit::singleline(&mut self.debug_ui.command_text).desired_width(340.0).hint_text("codelldb --port 0"));
                    ui.end_row();
                }
            });
            let (tool, how) = d.kind.requirement();
            if !tool.is_empty() && crate::format::find_in_path(tool).is_none() {
                ui.add(Label::new(RichText::new(format!("`{tool}` was not found on PATH: {how}.")).color(t.yellow).size(12.0)).wrap());
            } else if d.kind == Kind::Debugpy && !python_has_debugpy() {
                ui.add(Label::new(RichText::new("debugpy is not installed for python3: pip install debugpy").color(t.yellow).size(12.0)).wrap());
            } else if d.kind == Kind::Gdb {
                ui.label(RichText::new("Paths are relative to the project root. gdb 14+ is needed for its DAP mode.").size(12.0).color(t.comment));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Save & start").clicked() {
                    action = Some(true);
                }
                if ui.button("Save").clicked() {
                    action = Some(false);
                }
                if ui.button("Cancel").clicked() {
                    self.debug_ui.config_open = false;
                }
            });
        });
        if let Some(start) = action {
            let mut cfg = self.debug_ui.draft.clone();
            cfg.command = debug::split_args(&self.debug_ui.command_text);
            self.debug.config = cfg;
            if let Err(e) = self.debug.save() {
                self.error(format!("Couldn't save .rim/debug.toml: {e}"));
            }
            self.debug_ui.config_open = false;
            if start {
                self.debug_start();
            }
        }
        if !open {
            self.debug_ui.config_open = false;
        }
    }

    pub(super) fn debug_panel(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        let phase = self.debug.phase;
        let stopped = phase == Phase::Stopped;
        ui.horizontal(|ui| {
            let btn = |ui: &mut Ui, enabled: bool, text: &str, hint: &str| ui.add_enabled(enabled, egui::Button::new(text)).on_hover_text(hint).clicked();
            let start_label = match phase {
                Phase::Stopped => "▶ Continue",
                Phase::Building | Phase::Starting | Phase::Running => "▶ Running…",
                _ => "▶ Start",
            };
            if btn(ui, !matches!(phase, Phase::Building | Phase::Starting | Phase::Running), start_label, "F5") {
                self.debug_start();
            }
            if btn(ui, phase == Phase::Running, "⏸ Pause", "Interrupt the program") {
                self.debug.pause();
            }
            if btn(ui, stopped, "↷ Step over", "F10") {
                self.debug.step_over();
            }
            if btn(ui, stopped, "↓ Step into", "F11") {
                self.debug.step_into();
            }
            if btn(ui, stopped, "↑ Step out", "Shift+F11") {
                self.debug.step_out();
            }
            if btn(ui, self.debug.is_active(), "⏹ Stop", "Shift+F5") {
                self.debug.stop();
            }
            if ui.button("⚙").on_hover_text("Debug configuration (program, arguments, build command)").clicked() {
                self.debug_configure();
            }
            let status = match phase {
                Phase::Idle => "Set breakpoints in the gutter (or F9) and press F5.".to_string(),
                Phase::Building => "Building…".into(),
                Phase::Starting => "Starting…".into(),
                Phase::Running => "Running".into(),
                Phase::Stopped => format!("Stopped: {}", self.debug.reason),
                Phase::Ended => "Ended".into(),
            };
            let color = match phase {
                Phase::Stopped => t.yellow,
                Phase::Running | Phase::Building | Phase::Starting => t.green,
                _ => t.comment,
            };
            ui.label(RichText::new(status).color(color).size(12.5));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new("×").frame(false)).on_hover_text("Hide the panel").clicked() {
                    self.debug_ui.open = false;
                }
                let cfg = &self.debug.config;
                let summary = if self.debug.needs_config() { "not configured".to_string() } else { format!("{} · {}", cfg.kind.label().split(' ').next().unwrap_or(""), cfg.program) };
                ui.label(RichText::new(summary).color(t.comment).size(11.5));
            });
        });
        ui.separator();
        let total = ui.available_width();
        egui::Panel::left("dbg_stack").resizable(true).default_size(total * 0.26).frame(Frame::NONE).show(ui, |ui| {
            ui.label(RichText::new("CALL STACK").size(11.0).color(t.comment));
            ScrollArea::vertical().id_salt("dbg_stack_scroll").auto_shrink([false, false]).show(ui, |ui| {
                let mut pick = None;
                for (k, f) in self.debug.frames.iter().enumerate() {
                    let where_ = match &f.path {
                        Some(p) => format!("{}:{}", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), f.line + 1),
                        None => String::new(),
                    };
                    let text = if where_.is_empty() { f.name.clone() } else { format!("{}  {where_}", f.name) };
                    if ui.add(egui::Button::selectable(k == self.debug.frame_sel, RichText::new(text).size(12.5))).clicked() {
                        pick = Some(k);
                    }
                }
                if let Some(k) = pick {
                    self.debug.select_frame(k);
                }
                if self.debug.frames.is_empty() && stopped {
                    ui.label(RichText::new("Loading…").color(t.comment));
                }
            });
        });
        egui::Panel::right("dbg_console").resizable(true).default_size(total * 0.36).frame(Frame::NONE).show(ui, |ui| {
            ui.label(RichText::new("CONSOLE").size(11.0).color(t.comment));
            let input_h = 26.0;
            ScrollArea::vertical().id_salt("dbg_console_scroll").auto_shrink([false, false]).stick_to_bottom(true).max_height(ui.available_height() - input_h).show(ui, |ui| {
                ui.style_mut().interaction.selectable_labels = true;
                for l in &self.debug.console {
                    let color = match l.kind {
                        "stderr" | "error" => t.red,
                        "cmd" => t.blue,
                        "console" => t.comment,
                        _ => t.fg,
                    };
                    ui.add(Label::new(RichText::new(&l.text).monospace().size(12.0).color(color)).wrap());
                }
            });
            let resp = ui.add(TextEdit::singleline(&mut self.debug.eval).desired_width(f32::INFINITY).hint_text(if stopped { "Evaluate an expression (Enter)" } else { "Evaluate (needs a stopped program)" }));
            if self.debug_ui.focus_eval {
                resp.request_focus();
                self.debug_ui.focus_eval = false;
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let expr = std::mem::take(&mut self.debug.eval);
                self.debug.evaluate(&expr);
                self.debug_ui.focus_eval = true;
            }
        });
        ui.label(RichText::new("VARIABLES").size(11.0).color(t.comment));
        ScrollArea::vertical().id_salt("dbg_vars_scroll").auto_shrink([false, false]).show(ui, |ui| {
            let mut expand: Vec<(usize, Vec<usize>)> = Vec::new();
            let mut fetch: Vec<usize> = Vec::new();
            for (si, s) in self.debug.scopes.iter().enumerate() {
                let header = egui::CollapsingHeader::new(RichText::new(&s.name).strong()).id_salt(("dbg-scope", si)).default_open(si == 0).show(ui, |ui| match &s.vars {
                    Some(v) if v.is_empty() => {
                        ui.label(RichText::new("(none)").color(t.comment));
                    }
                    Some(v) => var_tree(ui, &t, v, si, &mut Vec::new(), &mut expand),
                    None => {
                        ui.label(RichText::new("…").color(t.comment));
                    }
                });
                if header.openness > 0.0 && s.vars.is_none() && !s.loading {
                    fetch.push(si);
                }
            }
            for si in fetch {
                self.debug.fetch_scope(si);
            }
            for (si, path) in expand {
                self.debug.expand(si, &path);
            }
            if self.debug.scopes.is_empty() {
                let msg = match phase {
                    Phase::Stopped => "Loading…",
                    Phase::Running | Phase::Starting | Phase::Building => "The program is running.",
                    _ => "Variables appear here while the program is stopped.",
                };
                ui.label(RichText::new(msg).color(t.comment));
            }
        });
    }
}

fn var_tree(ui: &mut Ui, t: &Theme, vars: &[Var], scope: usize, path: &mut Vec<usize>, expand: &mut Vec<(usize, Vec<usize>)>) {
    for (k, v) in vars.iter().enumerate() {
        path.push(k);
        let mut text = egui::text::LayoutJob::default();
        let f = FontId::monospace(12.0);
        text.append(&v.name, 0.0, egui::TextFormat { font_id: f.clone(), color: t.fg, ..Default::default() });
        text.append(" = ", 0.0, egui::TextFormat { font_id: f.clone(), color: t.comment, ..Default::default() });
        text.append(&v.value, 0.0, egui::TextFormat { font_id: f.clone(), color: t.orange, ..Default::default() });
        if !v.ty.is_empty() {
            text.append(&format!("  {}", v.ty), 0.0, egui::TextFormat { font_id: FontId::monospace(11.0), color: t.comment, ..Default::default() });
        }
        if v.reference > 0 {
            let header = egui::CollapsingHeader::new(text).id_salt(("dbg-var", scope, path.clone())).show(ui, |ui| match &v.children {
                Some(c) if c.is_empty() => {
                    ui.label(RichText::new("(empty)").color(t.comment));
                }
                Some(c) => var_tree(ui, t, c, scope, path, expand),
                None => {
                    ui.label(RichText::new("…").color(t.comment));
                }
            });
            if header.openness > 0.0 && v.children.is_none() && !v.loading {
                expand.push((scope, path.clone()));
            }
        } else {
            ui.label(text);
        }
        path.pop();
    }
}

fn python_has_debugpy() -> bool {
    std::process::Command::new("python3").args(["-c", "import debugpy"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}
