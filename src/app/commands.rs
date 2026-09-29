//! The command palette's commands and the global keyboard shortcuts, from one table.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Cmd {
    Save,
    SaveAll,
    New,
    OpenFolder,
    Close,
    Format,
    FormatOnSave,
    GotoLine,
    QuickFix,
    Find,
    Sidebar,
    Explorer,
    Search,
    SourceControl,
    ProjectGraph,
    GraphDescribeAll,
    FontBigger,
    FontSmaller,
    Theme,
    SettingsDialog,
    ToggleVim,
    OpenConfig,
    GitCommit,
    GitStageAll,
    GitUnstageAll,
    GitPull,
    GitPush,
    GitFetch,
    GitCheckout,
    GitBranch,
    GitLog,
    GitDiff,
    GitInit,
    LanguageServers,
    ClaudeToggle,
    ClaudeNew,
    ClaudeStop,
    ClaudeMention,
    LspRestart,
    Quit,
    Palette,
    QuickOpen,
    NextTab,
    PrevTab,
    DebugStart,
    DebugStop,
    DebugStepOver,
    DebugStepInto,
    DebugStepOut,
    DebugPause,
    DebugToggleBreakpoint,
    DebugPanel,
    DebugConfigure,
}

const C: Modifiers = Modifiers::COMMAND;
const CS: Modifiers = Modifiers { shift: true, ..Modifiers::COMMAND };
const AS: Modifiers = Modifiers { alt: true, shift: true, ..Modifiers::NONE };
const N: Modifiers = Modifiers::NONE;
const SH: Modifiers = Modifiers::SHIFT;

struct CmdInfo {
    cmd: Cmd,
    /// Stable id (palette items, tests).
    id: &'static str,
    /// Shown in the command palette; empty for commands that are only keyboard shortcuts.
    label: &'static str,
    hint: &'static str,
    keys: &'static [(Modifiers, Key)],
}

macro_rules! cmds {
    ($(($cmd:ident, $id:literal, $label:literal, $hint:literal, [$($m:ident + $k:ident),*])),* $(,)?) => {
        const COMMANDS: &[CmdInfo] = &[$(CmdInfo { cmd: Cmd::$cmd, id: $id, label: $label, hint: $hint, keys: &[$(($m, Key::$k)),*] }),*];
    };
}

// Shortcuts are matched in this order, and egui ignores extra Shift/Alt when matching, so
// Ctrl+Shift+X must come before Ctrl+X.
cmds![
    (Palette, "palette", "", "Ctrl+Shift+P", [CS + P]),
    (Search, "search", "Find in Files", "Ctrl+Shift+F", [CS + F]),
    (Explorer, "explorer", "View: Explorer", "Ctrl+Shift+E", [CS + E]),
    (SourceControl, "scm", "View: Source Control", "Ctrl+Shift+G", [CS + G]),
    (ProjectGraph, "graph", "View: Project Graph", "Ctrl+Shift+M", [CS + M]),
    (PrevTab, "prev_tab", "", "Ctrl+Shift+Tab", [CS + Tab, C + PageUp]),
    (Format, "format", "Format Document", "Shift+Alt+F", [AS + F]),
    (QuickOpen, "quick_open", "", "Ctrl+P", [C + P]),
    // Shift+F5 / Shift+F11 before the plain keys (extra Shift is ignored when matching).
    (DebugStop, "debug_stop", "Debug: Stop", "Shift+F5", [SH + F5]),
    (DebugStepOut, "debug_step_out", "Debug: Step Out", "Shift+F11", [SH + F11]),
    (DebugStart, "debug_start", "Debug: Start / Continue", "F5", [N + F5]),
    (DebugStepOver, "debug_step_over", "Debug: Step Over", "F10", [N + F10]),
    (DebugStepInto, "debug_step_into", "Debug: Step Into", "F11", [N + F11]),
    (DebugToggleBreakpoint, "debug_breakpoint", "Debug: Toggle Breakpoint", "F9", [N + F9]),
    (DebugPause, "debug_pause", "Debug: Pause", "", []),
    (DebugPanel, "debug_panel", "View: Debug Panel", "", []),
    (DebugConfigure, "debug_configure", "Debug: Configure…", "", []),
    (Find, "find", "Find in File", "Ctrl+F", [C + F]),
    (GotoLine, "goto", "Go to Line…", "Ctrl+G", [C + G]),
    (QuickFix, "quick_fix", "Quick Fix… (Auto-Import, Fix Problem)", "Ctrl+.", [C + Period]),
    (Save, "save", "File: Save", "Ctrl+S", [C + S]),
    (SaveAll, "save_all", "File: Save All", "", []),
    (Close, "close", "File: Close Tab", "Ctrl+W", [C + W]),
    (New, "new", "File: New File", "Ctrl+N", [C + N]),
    (OpenFolder, "open_folder", "File: Open Folder…", "Ctrl+O", [C + O]),
    (Sidebar, "sidebar", "View: Toggle Sidebar", "Ctrl+B", [C + B]),
    (Quit, "quit", "Quit", "Ctrl+Q", [C + Q]),
    (NextTab, "next_tab", "", "Ctrl+Tab", [C + Tab, C + PageDown]),
    (SettingsDialog, "settings_ui", "Preferences: Settings", "Ctrl+,", [C + Comma]),
    (LanguageServers, "language_servers", "Preferences: Language Servers", "", []),
    (ClaudeToggle, "claude_toggle", "Claude: Focus Chat / Toggle Panel", "Ctrl+L", [C + L]),
    (FormatOnSave, "fos", "Toggle Format on Save", "", []),
    (FontBigger, "font_inc", "View: Increase Editor Font Size", "", []),
    (FontSmaller, "font_dec", "View: Decrease Editor Font Size", "", []),
    (Theme, "theme", "Preferences: Color Theme…", "", []),
    (ToggleVim, "vim", "Preferences: Toggle Vim Motions", "", []),
    (OpenConfig, "settings", "Preferences: Open config.toml", "", []),
    (GitCommit, "git_commit", "Git: Commit Staged", "Ctrl+Enter", []),
    (GitStageAll, "git_stage_all", "Git: Stage All Changes", "", []),
    (GitUnstageAll, "git_unstage_all", "Git: Unstage All Changes", "", []),
    (GitPull, "git_pull", "Git: Pull", "", []),
    (GitPush, "git_push", "Git: Push", "", []),
    (GitFetch, "git_fetch", "Git: Fetch", "", []),
    (GitCheckout, "git_checkout", "Git: Checkout Branch…", "", []),
    (GitBranch, "git_branch", "Git: Create Branch…", "", []),
    (GitLog, "git_log", "Git: Show Log", "", []),
    (GitDiff, "git_diff", "Git: Diff Current File", "", []),
    (GitInit, "git_init", "Git: Initialize Repository", "", []),
    (ClaudeNew, "claude_new", "Claude: New Conversation", "", []),
    (ClaudeStop, "claude_stop", "Claude: Stop", "", []),
    (ClaudeMention, "claude_mention", "Claude: Mention Current File", "", []),
    (LspRestart, "lsp_restart", "LSP: Restart Language Servers", "", []),
    (GraphDescribeAll, "graph_describe_all", "Graph: Describe All with Claude", "", []),
];

impl App {
    /// Command palette entries.
    pub(super) fn commands() -> Vec<Item> {
        COMMANDS.iter().filter(|c| !c.label.is_empty()).map(|c| Item::new(c.id, c.label, c.hint)).collect()
    }

    /// Run a command by its id (palette items, tests). An unknown id is a bug, so it's reported.
    pub(super) fn run_command(&mut self, id: &str) {
        match COMMANDS.iter().find(|c| c.id == id) {
            Some(c) => self.run(c.cmd),
            None => self.error(format!("Unknown command: {id}")),
        }
    }

    pub(super) fn run(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::ProjectGraph => self.toggle_graph(),
            Cmd::QuickFix => self.quick_fix(),
            Cmd::DebugStart => self.debug_start(),
            Cmd::DebugStop => self.debug.stop(),
            Cmd::DebugStepOver => self.debug.step_over(),
            Cmd::DebugStepInto => self.debug.step_into(),
            Cmd::DebugStepOut => self.debug.step_out(),
            Cmd::DebugPause => self.debug.pause(),
            Cmd::DebugToggleBreakpoint => self.toggle_breakpoint_here(),
            Cmd::DebugPanel => self.debug_ui.open = !self.debug_ui.open,
            Cmd::DebugConfigure => self.debug_configure(),
            Cmd::GraphDescribeAll => {
                if !self.show_graph {
                    self.toggle_graph();
                }
                self.describe_all();
            }
            Cmd::Save => {
                if let Some(i) = self.active {
                    self.save(i, true);
                }
            }
            Cmd::SaveAll => {
                for i in 0..self.docs.len() {
                    if self.docs[i].is_dirty() {
                        self.save(i, true);
                    }
                }
            }
            Cmd::New => self.new_file(),
            Cmd::OpenFolder => self.open_folder_dialog(),
            Cmd::Close => {
                if let Some(i) = self.active {
                    self.request_close(i);
                }
            }
            Cmd::Format => {
                if let Some(i) = self.active {
                    self.format(i, false);
                }
            }
            Cmd::FormatOnSave => {
                self.cfg.format_on_save = !self.cfg.format_on_save;
                self.save_cfg();
                self.info(format!("Format on save: {}", if self.cfg.format_on_save { "on" } else { "off" }));
            }
            Cmd::GotoLine => self.palette.open(Mode::GotoLine, vec![], 0),
            Cmd::Find => {
                self.find.open = true;
                self.find.focus = true;
                let sel = self.active.and_then(|i| self.docs[i].selected_text()).filter(|s| !s.contains('\n')).map(String::from);
                if let Some(s) = sel {
                    self.find.query = s;
                }
            }
            Cmd::Sidebar => self.show_sidebar = !self.show_sidebar,
            Cmd::Explorer => self.show_panel(Sidebar::Explorer),
            Cmd::Search => self.open_find_files(),
            Cmd::SourceControl => self.show_panel(Sidebar::Git),
            Cmd::FontBigger | Cmd::FontSmaller => {
                let delta = if cmd == Cmd::FontBigger { 1.0 } else { -1.0 };
                self.cfg.font_size = (self.cfg.font_size + delta).clamp(8.0, 40.0);
                self.save_cfg();
            }
            Cmd::Theme => {
                let items: Vec<Item> = theme::all()
                    .iter()
                    .map(|t| Item::new(t.name, t.name, if t.dark { "dark" } else { "light" }))
                    .collect();
                let sel = items.iter().position(|i| i.id == self.theme.name).unwrap_or(0);
                self.palette.open(Mode::Themes { original: self.theme.name.to_string() }, items, sel);
            }
            Cmd::SettingsDialog => self.dialog = Some(Dialog::Settings),
            Cmd::ToggleVim => self.set_vim(!self.cfg.vim_mode),
            Cmd::ClaudeToggle => self.claude_toggle(),
            Cmd::LanguageServers => self.open_language_servers(None),
            Cmd::ClaudeNew => {
                self.show_claude = true;
                self.chat.reset();
                self.chat.focus_input = true;
            }
            Cmd::ClaudeStop => self.chat.stop(),
            Cmd::ClaudeMention => {
                let rel = self.context_info().rel;
                if !rel.is_empty() {
                    self.show_claude = true;
                    self.chat.insert_mention(&rel);
                }
            }
            Cmd::OpenConfig => {
                Config::ensure_exists();
                self.open_path(&Config::path());
            }
            Cmd::GitCommit => self.commit(),
            Cmd::GitStageAll => self.git_do("add", |r| r.stage_all()),
            Cmd::GitUnstageAll => self.git_do("reset", |r| r.unstage_all()),
            Cmd::GitPull => self.git_remote("pull"),
            Cmd::GitPush => self.git_remote("push"),
            Cmd::GitFetch => self.git_remote("fetch"),
            Cmd::GitCheckout => {
                let Some(repo) = &self.repo else { return };
                let items = repo.branches().into_iter().map(|b| Item::new(b.clone(), b, "")).collect();
                self.palette.open(Mode::Branches, items, 0);
            }
            Cmd::GitBranch => {
                self.dialog = Some(Dialog::Input { kind: InputKind::NewBranch, value: String::new(), focus: true })
            }
            Cmd::GitLog => {
                if let Some(repo) = &self.repo {
                    let log = repo.log();
                    self.open_virtual("git-log".into(), "git log".into(), log, "txt");
                }
            }
            Cmd::GitDiff => {
                let rel = self.active.and_then(|i| self.docs[i].path.clone()).and_then(|p| self.repo.as_ref()?.rel(&p));
                if let Some(rel) = rel {
                    let f = FileStatus { path: rel, staged: None, unstaged: Some('M') };
                    self.open_diff(&f, false);
                }
            }
            Cmd::GitInit => match Repo::init(&self.root) {
                Ok(r) => {
                    self.repo = Some(r);
                    self.refresh_git(true);
                    self.info("Initialized empty git repository");
                }
                Err(e) => self.error(e),
            },
            Cmd::LspRestart => self.restart_lsp(),
            Cmd::Quit => self.ctx.send_viewport_cmd(ViewportCommand::Close),
            Cmd::Palette => self.palette.open(Mode::Commands, Self::commands(), 0),
            Cmd::QuickOpen => self.open_quick_open(),
            Cmd::NextTab | Cmd::PrevTab => {
                if !self.docs.is_empty() {
                    let n = self.docs.len();
                    let a = self.active.unwrap_or(0);
                    self.activate(if cmd == Cmd::NextTab { (a + 1) % n } else { (a + n - 1) % n });
                }
            }
        }
    }

    pub(super) fn show_panel(&mut self, s: Sidebar) {
        self.show_sidebar = true;
        self.sidebar = s;
    }

    pub(super) fn open_quick_open(&mut self) {
        self.open_file_picker(Mode::Files);
    }

    pub(super) fn shortcuts(&mut self) {
        // In vim normal/visual mode Ctrl+F / Ctrl+B page down/up, so don't take them for Find and
        // the sidebar while the editor has focus.
        let vim_owns_ctrl = self.cfg.vim_mode
            && self.active.and_then(|i| self.docs.get(i)).is_some_and(|d| {
                d.vim.mode != vim::Mode::Insert && self.ctx.memory(|m| m.has_focus(d.editor_id()))
            });
        let mut run = Vec::new();
        self.ctx.input_mut(|i| {
            for c in COMMANDS {
                if vim_owns_ctrl && matches!(c.cmd, Cmd::Find | Cmd::Sidebar) {
                    continue;
                }
                if c.keys.iter().any(|&(m, k)| i.consume_key(m, k)) {
                    run.push(c.cmd);
                }
            }
        });
        for cmd in run {
            self.run(cmd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_table_is_consistent() {
        let mut ids = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(ids.insert(c.id), "duplicate id {}", c.id);
            assert_eq!(COMMANDS.iter().filter(|o| o.cmd == c.cmd).count(), 1, "{:?} listed twice", c.cmd);
        }
        // A Ctrl+X binding listed before a Ctrl+Shift+X one would steal it.
        for (i, a) in COMMANDS.iter().enumerate() {
            for b in &COMMANDS[i + 1..] {
                for &(ma, ka) in a.keys {
                    for &(mb, kb) in b.keys {
                        assert!(!(ka == kb && ma == C && mb == CS), "{} must come after {}", a.id, b.id);
                    }
                }
            }
        }
    }
}
