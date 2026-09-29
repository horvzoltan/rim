//! Settings → Language Servers: install, update, remove and enable language servers, and the
//! per-file choice of which servers run (see `lsp_registry`).

use std::cell::RefCell;
use std::sync::Arc;

use super::*;
use crate::config::ServerConfig;
use crate::file_icons;
use crate::lsp_install::{self, InstallInfo, Tools};
use crate::lsp_registry::{self, Group, Package, Source};

pub(super) enum InstallEvent {
    Log(String),
    Done(Result<InstallInfo, String>),
    /// Newest available version of a package (by id), from "Check for updates".
    Latest { id: String, result: Result<Option<String>, String> },
}

#[derive(Clone, Copy, PartialEq, Default)]
pub(super) enum SettingsTab {
    #[default]
    General,
    LanguageServers,
}

#[derive(Default)]
pub(super) struct Job {
    pub log: Vec<String>,
    pub running: bool,
    pub error: Option<String>,
}

pub(super) struct LspManager {
    pub servers_dir: PathBuf,
    /// Installed packages by install folder.
    pub installed: HashMap<String, InstallInfo>,
    /// Why a server failed to start, by name.
    pub errors: HashMap<String, String>,
    /// Installs in progress or finished this session, by install folder.
    pub jobs: HashMap<String, Job>,
    /// Newest published version per install folder, from "Check for updates".
    pub latest: HashMap<String, String>,
    pub checking: usize,
    pub tab: SettingsTab,
    pub search: String,
    pub expanded: Option<&'static str>,
    /// Scroll the expanded row into view on the next frame.
    pub scroll_to_expanded: bool,
    /// Set by a button that leaves the dialog (e.g. to open config.toml).
    pub close_dialog: bool,
    /// Servers per (extension, file name), and PATH lookups: both are asked for every frame.
    cache: HashMap<(String, String), Vec<ServerConfig>>,
    on_path: RefCell<HashMap<String, Option<PathBuf>>>,
    /// Finds programs (servers, and installers like `npm` or `go`) on `PATH`. The same lookup
    /// decides whether an Install button is enabled and runs the install; tests swap it out.
    pub which: Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>,
}

impl LspManager {
    pub fn new() -> Self {
        let servers_dir = config::servers_dir();
        Self {
            installed: lsp_install::load_installed(&servers_dir),
            servers_dir,
            errors: HashMap::new(),
            jobs: HashMap::new(),
            latest: HashMap::new(),
            checking: 0,
            tab: SettingsTab::General,
            search: String::new(),
            expanded: None,
            close_dialog: false,
            scroll_to_expanded: false,
            cache: HashMap::new(),
            on_path: RefCell::new(HashMap::new()),
            which: Arc::new(crate::format::find_in_path),
        }
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.on_path.borrow_mut().clear();
    }

    fn find(&self, program: &str) -> Option<PathBuf> {
        self.on_path.borrow_mut().entry(program.to_string()).or_insert_with(|| (self.which)(program)).clone()
    }

    /// Installer tools that use the same program lookup as the UI.
    fn tools(&self) -> Tools {
        let which = self.which.clone();
        Tools { find: Box::new(move |n| which(n)), ..Default::default() }
    }

    fn env<'a>(&'a self, cfg: &'a Config, root: &'a Path, on_path: &'a dyn Fn(&str) -> Option<PathBuf>) -> lsp_registry::Env<'a> {
        lsp_registry::Env { cfg, installed: &self.installed, servers_dir: &self.servers_dir, root, on_path }
    }
}

/// What the manager shows for a package.
enum Status {
    Installed(String),
    OnPath(PathBuf),
    NotInstalled,
    /// No download for this OS / CPU, or it can only be found (External).
    Unavailable(String),
}

impl App {
    /// The servers for document `i`: main server first, then add-ons.
    pub(super) fn servers_for_doc(&mut self, i: usize) -> Vec<ServerConfig> {
        let d = &self.docs[i];
        let file_name = d.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let key = (d.ext.clone(), file_name);
        if let Some(s) = self.lsp_mgr.cache.get(&key) {
            return s.clone();
        }
        let m = &self.lsp_mgr;
        let find = |p: &str| m.find(p);
        let servers = lsp_registry::servers_for(&m.env(&self.cfg, &self.root, &find), &key.0, &key.1);
        self.lsp_mgr.cache.insert(key, servers.clone());
        servers
    }

    /// A package that could serve document `i` when nothing does yet (for the status bar hint).
    pub(super) fn suggested_package(&mut self, i: usize) -> Option<&'static Package> {
        let d = &self.docs[i];
        d.path.as_ref()?;
        let file_name = d.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let ext = d.ext.clone();
        if !self.servers_for_doc(i).is_empty() {
            return None;
        }
        lsp_registry::all().iter().find(|p| {
            !p.addon
                && !matches!(p.source, Source::External { .. })
                && ((!ext.is_empty() && p.extensions.contains(&ext.as_str())) || p.file_names.contains(&file_name.as_str()))
                && !self.cfg.server_disabled(p.id)
        })
    }

    pub(super) fn open_language_servers(&mut self, focus: Option<&'static str>) {
        self.lsp_mgr.tab = SettingsTab::LanguageServers;
        if focus.is_some() {
            self.lsp_mgr.expanded = focus;
            self.lsp_mgr.search.clear();
            self.lsp_mgr.scroll_to_expanded = true;
        }
        self.dialog = Some(Dialog::Settings);
    }

    pub(super) fn lsp_install(&mut self, p: &'static Package) {
        let folder = p.folder.to_string();
        if self.lsp_mgr.jobs.get(&folder).is_some_and(|j| j.running) {
            return;
        }
        // An update replaces the folder a running server may be executing from.
        if self.lsp_mgr.installed.contains_key(p.folder) {
            for other in lsp_registry::all().iter().filter(|o| o.folder == p.folder) {
                self.stop_server(other.id);
            }
        }
        self.lsp_mgr.jobs.insert(folder.clone(), Job { running: true, ..Default::default() });
        let (tx, ctx, dir) = (self.tasks_tx.clone(), self.ctx.clone(), self.lsp_mgr.servers_dir.clone());
        let tools = self.lsp_mgr.tools();
        std::thread::spawn(move || {
            let send = |event| {
                let _ = tx.send(Task::LspInstall { folder: folder.clone(), event });
                ctx.request_repaint();
            };
            let result = lsp_install::install(p, &dir, &tools, &|line| send(InstallEvent::Log(line)));
            send(InstallEvent::Done(result));
        });
    }

    pub(super) fn lsp_uninstall(&mut self, p: &'static Package) {
        for other in lsp_registry::all().iter().filter(|o| o.folder == p.folder) {
            self.stop_server(other.id);
        }
        match lsp_install::uninstall(&self.lsp_mgr.servers_dir, p.folder) {
            Ok(()) => {
                self.lsp_mgr.installed.remove(p.folder);
                self.lsp_mgr.jobs.remove(p.folder);
                self.lsp_mgr.clear_cache();
                self.info(format!("Removed {}", p.name));
            }
            Err(e) => self.error(format!("Couldn't remove {}: {e}", p.name)),
        }
    }

    pub(super) fn lsp_set_enabled(&mut self, id: &str, enabled: bool) {
        let list = &mut self.cfg.disabled_language_servers;
        list.retain(|d| d != id);
        if !enabled {
            list.push(id.to_string());
        }
        self.cfg_dirty = true;
        self.stop_server(id);
    }

    pub(super) fn lsp_check_updates(&mut self) {
        let mut seen = HashSet::new();
        for p in lsp_registry::all() {
            if !self.lsp_mgr.installed.contains_key(p.folder) || !seen.insert(p.folder) {
                continue;
            }
            self.lsp_mgr.checking += 1;
            let (tx, ctx, folder) = (self.tasks_tx.clone(), self.ctx.clone(), p.folder.to_string());
            let tools = self.lsp_mgr.tools();
            std::thread::spawn(move || {
                let result = lsp_install::latest_version(p, &tools);
                let _ = tx.send(Task::LspInstall { folder, event: InstallEvent::Latest { id: p.id.to_string(), result } });
                ctx.request_repaint();
            });
        }
        if self.lsp_mgr.checking == 0 {
            self.info("No installed servers to check");
        }
    }

    pub(super) fn lsp_install_event(&mut self, folder: String, event: InstallEvent) {
        match event {
            InstallEvent::Log(line) => {
                let job = self.lsp_mgr.jobs.entry(folder).or_default();
                job.log.push(line);
                if job.log.len() > 200 {
                    job.log.remove(0);
                }
            }
            InstallEvent::Done(result) => {
                let name = lsp_registry::all().iter().find(|p| p.folder == folder).map(|p| p.name).unwrap_or("server");
                let job = self.lsp_mgr.jobs.entry(folder.clone()).or_default();
                job.running = false;
                match result {
                    Ok(info) => {
                        job.error = None;
                        let version = info.version.clone();
                        self.lsp_mgr.installed.insert(folder.clone(), info);
                        // Restart servers from this folder so open files use the new copy.
                        self.lsp_mgr.latest.remove(&folder);
                        for p in lsp_registry::all().iter().filter(|p| p.folder == folder) {
                            self.stop_server(p.id);
                        }
                        self.info(format!("Installed {name} {version}"));
                    }
                    Err(e) => {
                        job.error = Some(e.clone());
                        self.error(format!("Installing {name} failed: {}", e.lines().next().unwrap_or("")));
                    }
                }
            }
            InstallEvent::Latest { id, result } => {
                self.lsp_mgr.checking = self.lsp_mgr.checking.saturating_sub(1);
                match result {
                    Ok(Some(v)) => {
                        // Keyed by install folder: packages sharing one install share the version.
                        self.lsp_mgr.latest.insert(folder.clone(), v);
                    }
                    Ok(None) => {}
                    Err(e) => self.error(format!("Checking {id} for updates failed: {e}")),
                }
                if self.lsp_mgr.checking == 0 {
                    let n = self.updates_available();
                    self.info(if n == 0 { "All language servers are up to date".to_string() } else { format!("{n} update(s) available") });
                }
            }
        }
    }

    fn updates_available(&self) -> usize {
        lsp_registry::all().iter().filter(|p| self.newer_version(p).is_some()).count()
    }

    fn newer_version(&self, p: &Package) -> Option<&String> {
        let installed = &self.lsp_mgr.installed.get(p.folder)?.version;
        self.lsp_mgr.latest.get(p.folder).filter(|v| v.trim_start_matches('v') != installed.trim_start_matches('v'))
    }

    fn package_status(&self, p: &Package) -> Status {
        if let Some(info) = self.lsp_mgr.installed.get(p.folder) {
            return Status::Installed(info.version.clone());
        }
        if let Some(path) = self.lsp_mgr.find(p.program()) {
            return Status::OnPath(path);
        }
        match &p.source {
            Source::External { hint } => Status::Unavailable(hint.to_string()),
            Source::GithubRelease { asset, .. } if asset(std::env::consts::OS, std::env::consts::ARCH).is_none() => {
                Status::Unavailable(format!("No download for this system ({} {})", std::env::consts::OS, std::env::consts::ARCH))
            }
            _ => Status::NotInstalled,
        }
    }

    /// The Language Servers page of the Settings dialog.
    pub(super) fn language_servers_ui(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        ui.horizontal(|ui| {
            ui.add(TextEdit::singleline(&mut self.lsp_mgr.search).hint_text("Search languages").desired_width(260.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.lsp_mgr.checking > 0 {
                    ui.spinner();
                } else if ui.button("Check for updates").clicked() {
                    self.lsp_check_updates();
                }
                if ui.button("Restart servers").on_hover_text("Stop all language servers; they start again as needed").clicked() {
                    self.restart_lsp();
                }
            });
        });
        ui.label(
            RichText::new(format!("Installed in {}", self.lsp_mgr.servers_dir.display())).size(11.5).color(t.comment),
        );
        ui.add_space(4.0);
        let query = self.lsp_mgr.search.to_lowercase();
        let hit = |p: &Package| {
            query.is_empty()
                || p.languages.to_lowercase().contains(&query)
                || p.name.to_lowercase().contains(&query)
                || p.extensions.iter().any(|e| *e == query.trim_start_matches('.'))
        };
        egui::ScrollArea::vertical().max_height(480.0).auto_shrink([false, true]).show(ui, |ui| {
            for group in Group::ALL {
                let pkgs: Vec<&'static Package> = lsp_registry::all().iter().filter(|p| p.group == group && hit(p)).collect();
                if pkgs.is_empty() {
                    continue;
                }
                ui.add_space(6.0);
                ui.label(RichText::new(group.label().to_uppercase()).size(11.0).strong().color(t.comment));
                for p in pkgs {
                    self.package_row(ui, &t, p);
                }
            }
        });
    }

    fn package_row(&mut self, ui: &mut Ui, t: &Theme, p: &'static Package) {
        let status = self.package_status(p);
        let enabled = !self.cfg.server_disabled(p.id);
        let running = self.lsp.get(p.id).map(|c| c.ready);
        let failed = self.lsp_mgr.errors.get(p.id).cloned();
        let job_running = self.lsp_mgr.jobs.get(p.folder).is_some_and(|j| j.running);
        let expanded = self.lsp_mgr.expanded == Some(p.id);
        let (status_text, status_color) = match (&status, enabled) {
            (_, false) => ("Disabled".to_string(), t.comment),
            (Status::Installed(v), _) => (if v.is_empty() { "Installed".into() } else { format!("Installed {v}") }, t.green),
            (Status::OnPath(path), _) => (format!("Found on PATH: {}", path.display()), t.blue),
            (Status::NotInstalled, _) => ("Not installed".into(), t.comment),
            (Status::Unavailable(why), _) => (why.clone(), t.comment),
        };
        // Reserve a slot behind the row for the hover highlight.
        let bg = ui.painter().add(egui::Shape::Noop);
        // The row's click sense is registered before its contents, so the buttons inside sit on top
        // and receive their own clicks (a sensor added afterwards would cover them).
        let row = ui.scope_builder(egui::UiBuilder::new().sense(Sense::click()).id_salt(("lsp-row", p.id)), |ui| ui.horizontal(|ui| {
            // Plain text in the row is not selectable, so clicking it toggles the row instead.
            ui.style_mut().interaction.selectable_labels = false;
            ui.set_min_height(36.0);
            let (glyph, color) = file_icons::for_file(&p.sample_file(), t.dark);
            let icon = RichText::new(glyph.to_string()).family(file_icons::family()).size(18.0).color(color);
            ui.add_sized(vec2(26.0, 26.0), egui::Label::new(icon));
            let text = ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(p.languages).strong().color(t.fg));
                    ui.label(RichText::new(p.name).size(12.0).color(t.comment));
                    if p.addon {
                        ui.label(RichText::new("add-on").size(10.5).color(t.purple))
                            .on_hover_text("Runs next to the file's main language server and adds its own problems");
                    }
                });
                ui.horizontal(|ui| {
                    if let Some(ready) = running {
                        let c = if ready { t.green } else { t.yellow };
                        ui.label(RichText::new("●").size(10.0).color(c)).on_hover_text(if ready { "Running" } else { "Starting" });
                    } else if let Some(e) = &failed {
                        ui.label(RichText::new("●").size(10.0).color(t.red)).on_hover_text(format!("Failed to start: {e}"));
                    }
                    ui.add(egui::Label::new(RichText::new(status_text).size(11.5).color(status_color)).truncate());
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if job_running {
                    ui.label(RichText::new("Installing…").size(12.0).color(t.comment));
                    ui.spinner();
                    return;
                }
                match &status {
                    Status::Installed(_) => {
                        if let Some(v) = self.newer_version(p).cloned() {
                            if ui.button(format!("Update to {v}")).clicked() {
                                self.lsp_install(p);
                            }
                        }
                    }
                    Status::NotInstalled | Status::OnPath(_) => {
                        let missing = p.source.installer().filter(|(prog, _)| self.lsp_mgr.find(prog).is_none());
                        let label = if matches!(status, Status::OnPath(_)) { "Install rim's copy" } else { "Install" };
                        let resp = ui.add_enabled(missing.is_none(), egui::Button::new(label));
                        let resp = match missing {
                            Some((_, hint)) => resp.on_disabled_hover_text(hint),
                            None => resp.on_hover_text(format!("Installs from the {}", p.source.describe())),
                        };
                        if resp.clicked() {
                            self.lsp_install(p);
                        }
                    }
                    Status::Unavailable(_) => {}
                }
            });
            text.response
        }));
        let click = row.response;
        if click.hovered() {
            ui.painter().set(bg, egui::Shape::rect_filled(click.rect.expand(2.0), 4.0, t.bg_highlight));
        }
        if expanded && std::mem::take(&mut self.lsp_mgr.scroll_to_expanded) {
            click.scroll_to_me(Some(Align::Center));
        }
        if click.clicked() || row.inner.inner.clicked() {
            self.lsp_mgr.expanded = if expanded { None } else { Some(p.id) };
        }
        if expanded {
            self.package_details(ui, t, p, &status, enabled);
        }
    }

    fn package_details(&mut self, ui: &mut Ui, t: &Theme, p: &'static Package, status: &Status, enabled: bool) {
        Frame::NONE.fill(t.bg_dark).corner_radius(6).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(p.description).color(t.fg_dim));
            ui.hyperlink_to(RichText::new(p.homepage).size(12.0), p.homepage);
            let mut files: Vec<String> = p.extensions.iter().map(|e| format!(".{e}")).collect();
            files.extend(p.file_names.iter().map(|f| f.to_string()));
            let small = |s: String| RichText::new(s).size(12.0).color(t.comment);
            ui.label(small(format!("Files: {}", files.join(" "))));
            ui.label(small(format!("Installs from: {}", p.source.describe())));
            if let Some(m) = p.marker {
                ui.label(small(format!("Only in projects with a {m}")));
            }
            if !p.requires.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(small("Needs:".into()));
                    for (prog, what) in p.requires {
                        let ok = self.lsp_mgr.find(prog).is_some();
                        let text = format!("{} {what}", if ok { "✔" } else { "✖" });
                        ui.label(RichText::new(text).size(12.0).color(if ok { t.green } else { t.red }))
                            .on_hover_text(if ok { format!("`{prog}` found") } else { format!("`{prog}` is not on PATH") });
                    }
                });
            }
            if let Status::Installed(_) = status {
                ui.label(small(format!("Folder: {}", self.lsp_mgr.servers_dir.join(p.folder).display())));
            }
            if let Some(e) = self.lsp_mgr.errors.get(p.id) {
                ui.label(RichText::new(format!("Failed to start: {e}")).size(12.0).color(t.red));
            }
            if let Some(job) = self.lsp_mgr.jobs.get(p.folder) {
                if let Some(e) = &job.error {
                    ui.label(RichText::new(e).size(12.0).color(t.red));
                }
                if !job.log.is_empty() {
                    let tail: Vec<&str> = job.log.iter().rev().take(8).rev().map(String::as_str).collect();
                    ui.label(RichText::new(tail.join("\n")).monospace().size(11.0).color(t.comment));
                }
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let mut on = enabled;
                if ui.checkbox(&mut on, "Enabled").on_hover_text("Off: never start this server").changed() {
                    self.lsp_set_enabled(p.id, on);
                }
                let shared = lsp_registry::all().iter().filter(|o| o.folder == p.folder && o.id != p.id).map(|o| o.languages).collect::<Vec<_>>();
                if let Status::Installed(_) = status {
                    let running = self.lsp_mgr.jobs.get(p.folder).is_some_and(|j| j.running);
                    if ui.add_enabled(!running, egui::Button::new("Reinstall")).on_hover_text("Install the latest version again").clicked() {
                        self.lsp_install(p);
                    }
                    let mut btn = ui.add_enabled(!running, egui::Button::new("Uninstall"));
                    if !shared.is_empty() {
                        btn = btn.on_hover_text(format!("Also removes {} (same install)", shared.join(", ")));
                    }
                    if btn.clicked() {
                        self.lsp_uninstall(p);
                    }
                }
                if ui.button("Customize in config.toml").on_hover_text("Add a [[language_servers]] entry to use your own command").clicked() {
                    Config::ensure_exists();
                    self.open_path(&Config::path());
                    self.lsp_mgr.close_dialog = true;
                }
            });
        });
        ui.add_space(4.0);
    }
}
