//! The project graph view: a zoomable map of directories and files joined by their imports,
//! with a card explaining the selected node. Replaces the editor area while shown.

use std::collections::VecDeque;

use egui::{Rect, Vec2};

use super::*;
use crate::file_icons;
use crate::graph::describe::{Cache, Request};
use crate::graph::layout::{self, Layout as ForceLayout};
use crate::graph::{self, DescSource, Node, NodeKind, ProjectGraph};

/// How long one Claude summary may take.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(120);
/// Layout work per frame while the picture settles.
const FRAME_BUDGET: Duration = Duration::from_millis(4);
const MAX_LABELS: usize = 400;

pub(super) struct GraphState {
    pub data: Option<ProjectGraph>,
    pub layout: ForceLayout,
    pub scene_rect: Rect,
    pub selected: Option<usize>,
    pub hovered: Option<usize>,
    pub search: String,
    pub scanning: bool,
    /// Files changed since the last scan; rescanned when the view is shown.
    pub dirty: bool,
    pub last_scan: Option<Instant>,
    pub cache: Cache,
    /// Nodes waiting for a Claude summary, and the one being summarized (by path).
    pub describe_queue: VecDeque<usize>,
    pub describing: Option<String>,
    pub describe_total: usize,
    pub describe_done: usize,
    pub describe_failures: u32,
    pub focus_search: bool,
    /// "Describe all" was asked for before the first scan finished.
    pub describe_after_scan: bool,
    /// Scene scale during the last frame (1 = one scene unit per point).
    pub zoom: f32,
}

impl Default for GraphState {
    fn default() -> Self {
        Self {
            data: None,
            layout: ForceLayout::default(),
            scene_rect: Rect::ZERO,
            selected: None,
            hovered: None,
            search: String::new(),
            scanning: false,
            dirty: false,
            last_scan: None,
            cache: Cache::default(),
            describe_queue: VecDeque::new(),
            describing: None,
            describe_total: 0,
            describe_done: 0,
            describe_failures: 0,
            focus_search: false,
            describe_after_scan: false,
            zoom: 1.0,
        }
    }
}

enum Action {
    Open(PathBuf),
    Center(usize),
    Describe(usize),
}

impl App {
    pub(super) fn toggle_graph(&mut self) {
        if self.show_graph {
            self.close_graph();
            return;
        }
        self.show_graph = true;
        if self.graph.data.is_none() || self.graph.dirty {
            self.start_graph_scan();
        }
    }

    fn close_graph(&mut self) {
        self.show_graph = false;
        self.focus_editor = true;
    }

    pub(super) fn start_graph_scan(&mut self) {
        if self.graph.scanning {
            return;
        }
        self.graph.scanning = true;
        self.graph.dirty = false;
        let (tx, ctx) = (self.tasks_tx.clone(), self.ctx.clone());
        let root = self.root.clone();
        let max_files = self.cfg.graph.max_files.max(10);
        std::thread::spawn(move || {
            let mut g = graph::scan::scan(&root, max_files);
            let cache = Cache::load(&root);
            cache.apply(&mut g);
            let _ = tx.send(Task::Graph { root, graph: g, cache });
            ctx.request_repaint();
        });
    }

    /// A finished scan: keep positions of nodes that already existed so the map doesn't reshuffle.
    pub(super) fn apply_graph(&mut self, root: PathBuf, mut g: ProjectGraph, cache: Cache) {
        if root != self.root {
            return;
        }
        self.graph.scanning = false;
        let mut layout = ForceLayout::default();
        layout.seed(&mut g);
        match &self.graph.data {
            Some(old) => {
                let by_rel: HashMap<&str, usize> = old.nodes.iter().enumerate().map(|(i, n)| (n.rel.as_str(), i)).collect();
                for n in &mut g.nodes {
                    if let Some(&i) = by_rel.get(n.rel.as_str()) {
                        n.pos = old.nodes[i].pos;
                        n.pinned = old.nodes[i].pinned;
                    }
                }
                self.graph.selected = self.graph.selected.and_then(|s| old.nodes.get(s)).and_then(|n| g.by_rel(&n.rel));
            }
            None => self.graph.selected = None,
        }
        self.graph.hovered = None;
        layout.kick();
        #[cfg(debug_assertions)]
        if let Some(rel) = std::env::var("RIM_GRAPH_DEMO").ok().filter(|v| v != "1") {
            if let Some(i) = g.by_rel(&rel) {
                for _ in 0..30 {
                    layout.step(&mut g, 20);
                }
                self.graph.selected = Some(i);
                self.graph.scene_rect = Rect::from_center_size(g.nodes[i].pos, Vec2::new(420.0, 300.0));
            }
        }
        self.graph.layout = layout;
        self.graph.cache = cache;
        self.graph.last_scan = Some(Instant::now());
        self.graph.describe_queue.clear();
        self.graph.data = Some(g);
        if std::mem::take(&mut self.graph.describe_after_scan) {
            self.describe_all();
        }
    }

    // ---------- descriptions ----------

    pub(super) fn describe_node(&mut self, i: usize) {
        let Some(bin) = self.chat.bin.clone() else {
            self.error("Claude Code CLI not found (see [claude] path in config.toml)");
            return;
        };
        let Some(g) = &self.graph.data else { return };
        if self.graph.describing.is_some() {
            if !self.graph.describe_queue.contains(&i) {
                self.graph.describe_queue.push_back(i);
                self.graph.describe_total += 1;
            }
            return;
        }
        let req = Request::for_node(g, i);
        self.graph.describing = Some(req.rel.clone());
        let model = self.cfg.graph.describe_model.clone().or_else(|| self.cfg.claude.model.clone());
        let (tx, ctx, root) = (self.tasks_tx.clone(), self.ctx.clone(), self.root.clone());
        std::thread::spawn(move || {
            let prompt = req.prompt();
            let result = claude::ask_once(&bin, &root, &prompt, model.as_deref(), DESCRIBE_TIMEOUT).map(|s| (s, model));
            let _ = tx.send(Task::Describe { root, rel: req.rel, hash: req.hash, result });
            ctx.request_repaint();
        });
    }

    /// Queue every node without a Claude summary: files first, then directories deepest
    /// first, so a directory's prompt can quote its children's summaries.
    pub(super) fn describe_all(&mut self) {
        let Some(g) = &self.graph.data else {
            self.graph.describe_after_scan = true;
            return;
        };
        let mut q: Vec<usize> = (0..g.nodes.len()).filter(|&i| !g.nodes[i].is_dir() && g.nodes[i].desc_src != DescSource::Claude).collect();
        q.extend(g.dirs.iter().rev().filter(|&&d| g.nodes[d].desc_src != DescSource::Claude));
        self.graph.describe_queue = q.into_iter().collect();
        self.graph.describe_total = self.graph.describe_queue.len();
        self.graph.describe_done = 0;
        self.graph.describe_failures = 0;
        self.pump_describe();
    }

    fn pump_describe(&mut self) {
        if self.graph.describing.is_some() {
            return;
        }
        while let Some(i) = self.graph.describe_queue.pop_front() {
            let pending = self.graph.data.as_ref().is_some_and(|g| g.nodes.get(i).is_some_and(|n| n.desc_src != DescSource::Claude));
            if pending {
                self.describe_node(i);
                if self.graph.describing.is_some() {
                    return;
                }
            } else {
                self.graph.describe_done += 1;
            }
        }
    }

    pub(super) fn describe_result(&mut self, root: PathBuf, rel: String, hash: u64, result: Result<(String, Option<String>), String>) {
        if self.graph.describing.as_deref() == Some(rel.as_str()) {
            self.graph.describing = None;
        }
        match result {
            Ok((text, model)) => {
                self.graph.describe_done += 1;
                self.graph.describe_failures = 0;
                if root == self.root {
                    if let Some(g) = &mut self.graph.data {
                        if let Some(i) = g.by_rel(&rel).filter(|&i| g.nodes[i].hash == hash) {
                            g.nodes[i].desc = Some(text.clone());
                            g.nodes[i].desc_src = DescSource::Claude;
                        }
                    }
                    self.graph.cache.put(&rel, hash, text, model);
                    if let Err(e) = self.graph.cache.save(&root) {
                        self.error(format!("Could not save .rim/graph.json: {e}"));
                    }
                }
            }
            Err(e) => {
                self.graph.describe_failures += 1;
                self.error(format!("Describe {}: {e}", if rel.is_empty() { "project" } else { &rel }));
                if self.graph.describe_failures >= 3 && !self.graph.describe_queue.is_empty() {
                    self.graph.describe_queue.clear();
                    self.error("Stopped after three failures in a row");
                }
            }
        }
        self.pump_describe();
    }

    /// `(done, total)` while summaries are being generated.
    pub(super) fn describe_progress(&self) -> Option<(usize, usize)> {
        if self.graph.describing.is_none() && self.graph.describe_queue.is_empty() {
            return None;
        }
        Some((self.graph.describe_done, self.graph.describe_total.max(1)))
    }

    // ---------- view ----------

    pub(super) fn graph_ui(&mut self, ui: &mut Ui) {
        let t = self.theme.clone();
        if !self.graph.scanning && (self.graph.data.is_none() || self.graph.dirty && self.graph.last_scan.is_none_or(|s| s.elapsed() > Duration::from_secs(10))) {
            self.start_graph_scan();
        }
        let esc = ui.input(|i| i.key_pressed(Key::Escape));
        if esc && !self.palette.open && !self.find_files.open && ui.memory(|m| m.focused().is_none()) {
            self.close_graph();
            return;
        }

        let mut actions: Vec<Action> = Vec::new();
        self.graph_toolbar(ui, &t, &mut actions);

        egui::Panel::right("graph_info")
            .default_size(300.0)
            .size_range(220.0..=520.0)
            .frame(Frame::NONE.fill(t.bg_dark).inner_margin(Margin::same(10)))
            .show(ui, |ui| self.graph_info(ui, &t, &mut actions));

        if self.graph.data.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Scanning project…").color(t.fg_dim));
            });
            return;
        }

        // Let the layout settle a little each frame.
        {
            let st = &mut self.graph;
            let g = st.data.as_mut().unwrap();
            let start = Instant::now();
            while !st.layout.converged() && start.elapsed() < FRAME_BUDGET {
                st.layout.step(g, 3);
            }
            if !st.layout.converged() {
                ui.ctx().request_repaint();
            }
        }

        let mut scene_rect = self.graph.scene_rect;
        let resp = egui::Scene::new().zoom_range(0.02..=12.0).show(ui, &mut scene_rect, |ui| self.paint_graph(ui, &t, &mut actions));
        if resp.response.clicked() {
            self.graph.selected = None;
        }
        self.graph.scene_rect = scene_rect;

        for a in actions {
            match a {
                Action::Open(p) => {
                    self.open_path(&p);
                    self.close_graph();
                }
                Action::Center(i) => {
                    self.graph.selected = Some(i);
                    if let Some(g) = &self.graph.data {
                        let size = self.graph.scene_rect.size();
                        let size = if size.x > 1.0 && size.y > 1.0 { size } else { Vec2::splat(400.0) };
                        self.graph.scene_rect = Rect::from_center_size(g.nodes[i].pos, size);
                    }
                }
                Action::Describe(i) => self.describe_node(i),
            }
        }
    }

    fn graph_toolbar(&mut self, ui: &mut Ui, t: &Theme, actions: &mut Vec<Action>) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Project Graph").strong().color(t.fg));
            let edit = TextEdit::singleline(&mut self.graph.search).hint_text("Filter by path…").desired_width(180.0);
            let r = ui.add(edit);
            if std::mem::take(&mut self.graph.focus_search) {
                r.request_focus();
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let q = self.graph.search.trim().to_lowercase();
                if let Some(g) = &self.graph.data {
                    if let Some(i) = (!q.is_empty()).then(|| g.nodes.iter().position(|n| !n.is_dir() && n.rel.to_lowercase().contains(&q))).flatten() {
                        actions.push(Action::Center(i));
                    }
                }
            }
            if let Some(g) = &self.graph.data {
                let what = if g.collapsed { format!("{} folders ({} files, folded)", g.dirs.len(), g.file_count) } else { format!("{} files", g.file_count) };
                ui.label(RichText::new(format!("{what}, {} imports · {:.0}%", g.edges.len(), self.graph.zoom * 100.0)).color(t.comment));
            }
            if self.graph.scanning {
                ui.label(RichText::new("scanning…").color(t.orange));
            } else if ui.small_button("Rescan").on_hover_text("Read the project again").clicked() {
                self.start_graph_scan();
            }
            if ui.small_button("Fit").on_hover_text("Fit the whole graph in view").clicked() {
                self.graph.scene_rect = Rect::ZERO;
            }
            match self.describe_progress() {
                Some((done, total)) => {
                    ui.label(RichText::new(format!("Describing {done}/{total}…")).color(t.orange));
                    if ui.small_button("Stop").clicked() {
                        self.graph.describe_queue.clear();
                    }
                }
                None => {
                    let has_bin = self.chat.bin.is_some();
                    let b = ui.add_enabled(has_bin && self.graph.data.is_some(), egui::Button::new("Describe all").small());
                    let b = b.on_hover_text("Ask Claude for a one-line summary of every file and folder (cached in .rim/graph.json)");
                    if b.clicked() {
                        self.describe_all();
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Back to the editor (Esc)").clicked() {
                    self.close_graph();
                }
                ui.label(RichText::new("scroll: zoom · drag: pan · double-click: open").color(t.comment));
            });
        });
        ui.add_space(2.0);
    }

    fn graph_info(&mut self, ui: &mut Ui, t: &Theme, actions: &mut Vec<Action>) {
        let Some(g) = &self.graph.data else {
            return;
        };
        let Some(i) = self.graph.selected.filter(|&i| i < g.nodes.len()) else {
            ui.label(RichText::new("Nothing selected").strong().color(t.fg_dim));
            ui.add_space(6.0);
            ui.label(RichText::new("Click a file or a folder name to see what it does. Double-click a file to open it.").color(t.fg_dim));
            ui.add_space(6.0);
            ui.label(RichText::new("Bigger circles are longer files; lines are imports; colours are top-level folders.").color(t.comment));
            if g.collapsed {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("This project has more than {} files, so only folders are shown ([graph] max_files in config.toml).", self.cfg.graph.max_files)).color(t.comment));
            }
            return;
        };
        let n = &g.nodes[i];
        let colors = top_colors(g, t);
        let color = color_of(n, &colors, t);
        ui.horizontal(|ui| {
            let glyph = if n.is_dir() { (file_icons::folder(true), color) } else { file_icons::for_file(&n.name, t.dark) };
            ui.label(RichText::new(glyph.0.to_string()).font(FontId::new(20.0, file_icons::family())).color(glyph.1));
            let title = if n.rel.is_empty() { self.root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default() } else { n.name.clone() };
            ui.label(RichText::new(title).size(16.0).strong().color(t.fg));
        });
        if !n.rel.is_empty() {
            ui.label(RichText::new(&n.rel).color(t.fg_dim).small());
        }
        let mut facts = vec![format!("{} lines", n.loc)];
        if n.is_dir() {
            let files = g.nodes.iter().filter(|m| !m.is_dir() && m.rel.starts_with(&format!("{}/", n.rel)) || n.rel.is_empty() && !m.is_dir()).count();
            facts.insert(0, format!("{files} files"));
        } else if let Some(l) = n.lang {
            facts.push(l.to_string());
        }
        if n.external > 0 {
            facts.push(format!("{} external imports", n.external));
        }
        ui.label(RichText::new(facts.join(" · ")).color(t.comment).small());
        ui.add_space(8.0);

        match &n.desc {
            Some(d) => {
                ui.label(RichText::new(d).color(t.fg));
                let src = match n.desc_src {
                    DescSource::Claude => "summary by Claude",
                    DescSource::Heuristic if n.is_dir() => "from README",
                    DescSource::Heuristic => "from the file's comments",
                    DescSource::None => "",
                };
                ui.label(RichText::new(src).color(t.comment).small());
            }
            None => {
                ui.label(RichText::new("No description yet.").color(t.fg_dim));
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let busy = self.graph.describing.as_deref() == Some(n.rel.as_str()) || self.graph.describe_queue.contains(&i);
            let label = if busy { "Describing…" } else if n.desc_src == DescSource::Claude { "Describe again" } else { "Describe with Claude" };
            let b = ui.add_enabled(self.chat.bin.is_some() && !busy, egui::Button::new(label));
            if b.on_hover_text("Ask Claude for a 1-2 sentence summary (cached in .rim/graph.json)").clicked() {
                actions.push(Action::Describe(i));
            }
            if !n.is_dir() && ui.button("Open").clicked() {
                actions.push(Action::Open(n.path.clone()));
            }
        });

        let (out, inc) = g.neighbors(i);
        let mut list = |ui: &mut Ui, title: &str, ids: Vec<usize>| {
            if ids.is_empty() {
                return;
            }
            ui.add_space(8.0);
            ui.label(RichText::new(format!("{title} ({})", ids.len())).strong().color(t.fg_dim).small());
            for j in ids {
                let m = &g.nodes[j];
                let r = ui.add(egui::Label::new(RichText::new(&m.rel).color(t.blue)).sense(Sense::click()).truncate());
                if r.clicked() {
                    actions.push(Action::Center(j));
                }
                if r.double_clicked() && !m.is_dir() {
                    actions.push(Action::Open(m.path.clone()));
                }
            }
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            list(ui, "Imports", out);
            list(ui, "Imported by", inc);
            if n.is_dir() {
                let mut kids: Vec<usize> = g.children(i).collect();
                kids.sort_by(|&a, &b| g.nodes[b].is_dir().cmp(&g.nodes[a].is_dir()).then(g.nodes[a].name.cmp(&g.nodes[b].name)));
                list(ui, "Contains", kids);
            }
        });
    }

    /// Paint hulls, edges, nodes and labels in scene coordinates; handle hover, click and drag.
    fn paint_graph(&mut self, ui: &mut Ui, t: &Theme, actions: &mut Vec<Action>) {
        let st = &mut self.graph;
        let g = st.data.as_mut().unwrap();
        let zoom = ui.ctx().layer_transform_to_global(ui.layer_id()).map(|t| t.scaling).unwrap_or(1.0).max(1e-3);
        st.zoom = zoom;
        let clip = ui.clip_rect();
        ui.expand_to_include_rect(layout::bounds(g, &st.layout));
        let painter = ui.painter().clone();
        let colors = top_colors(g, t);
        let query = st.search.trim().to_lowercase();
        let matches = |n: &Node| query.is_empty() || n.rel.to_lowercase().contains(&query);
        let (sel, hov) = (st.selected, st.hovered);
        // Screen-constant sizes: divide by zoom when zoomed in, keep text growing when zoomed in.
        let px = |v: f32| v / zoom;
        let text_size = |v: f32| v / zoom.min(1.0);

        // A directory holding nothing but one subdirectory shares its hull with it: draw only the outer one.
        let redundant: std::collections::HashSet<usize> = g
            .dirs
            .iter()
            .copied()
            .filter(|&d| g.nodes[d].dir.is_some_and(|p| p != 0 && st.layout.member_count(p) == st.layout.member_count(d)))
            .collect();
        // Directory hulls, parents first so children draw on top.
        for &d in &g.dirs {
            if d == 0 || redundant.contains(&d) {
                continue;
            }
            let Some(&r) = st.layout.hull.get(&d) else { continue };
            let c = g.nodes[d].pos;
            if !clip.intersects(Rect::from_center_size(c, Vec2::splat(r * 2.0))) {
                continue;
            }
            let col = color_of(&g.nodes[d], &colors, t);
            let (fill, stroke) = if g.nodes[d].depth == 1 { (0.14, 0.4) } else { (0.08, 0.25) };
            let stroke = if sel == Some(d) { Stroke::new(px(2.0), t.fg) } else { Stroke::new(px(1.0), col.gamma_multiply(stroke)) };
            painter.circle(c, r, col.gamma_multiply(fill), stroke);
        }

        // Edges.
        for e in &g.edges {
            let (a, b) = (g.nodes[e.from].pos, g.nodes[e.to].pos);
            if !clip.intersects(Rect::from_two_pos(a, b)) {
                continue;
            }
            let hi = sel == Some(e.from) || sel == Some(e.to) || hov == Some(e.from) || hov == Some(e.to);
            let dimmed = !query.is_empty() && !matches(&g.nodes[e.from]) && !matches(&g.nodes[e.to]);
            let (color, width) = if hi {
                (t.yellow, px(1.8))
            } else {
                let alpha = if dimmed { 0.08 } else if zoom < 0.35 { 0.22 } else { 0.38 };
                (t.fg_dim.gamma_multiply(alpha), px(0.7 + 0.15 * e.weight.min(6) as f32))
            };
            painter.line_segment([a, b], Stroke::new(width, color));
            if hi {
                // Arrow head at the imported file.
                let dir = (b - a).normalized();
                let tip = b - dir * (g.nodes[e.to].radius + px(1.0));
                let side = Vec2::new(-dir.y, dir.x) * px(3.5);
                let back = tip - dir * px(8.0);
                painter.add(egui::Shape::convex_polygon(vec![tip, back + side, back - side], color, Stroke::NONE));
            }
        }

        // File nodes.
        let show_names = zoom >= 1.2;
        let show_icons = zoom >= 1.0;
        let mut labels = 0;
        let mut new_hov = None;
        let mut new_sel = None;
        let mut kicked = false;
        for i in 0..g.nodes.len() {
            if g.nodes[i].is_dir() {
                continue;
            }
            let r = g.nodes[i].radius;
            let pos = g.nodes[i].pos;
            let rect = Rect::from_center_size(pos, Vec2::splat(r * 2.0 + px(4.0)));
            if !clip.intersects(rect) {
                continue;
            }
            let resp = ui.interact(rect, ui.id().with(("node", i)), Sense::click_and_drag());
            if resp.hovered() {
                new_hov = Some(i);
            }
            if resp.clicked() {
                new_sel = Some(i);
            }
            if resp.double_clicked() {
                actions.push(Action::Open(g.nodes[i].path.clone()));
            }
            if resp.dragged() {
                g.nodes[i].pos += resp.drag_delta();
                g.nodes[i].pinned = true;
                kicked = true;
            }
            if resp.drag_stopped() {
                g.nodes[i].pinned = false;
            }
            let n = &g.nodes[i];
            let base = color_of(n, &colors, t);
            let matched = matches(n);
            let fill = if matched { base } else { base.gamma_multiply(0.25) };
            let stroke = if sel == Some(i) {
                Stroke::new(px(2.0), t.fg)
            } else if hov == Some(i) || !query.is_empty() && matched {
                Stroke::new(px(1.5), if matched && !query.is_empty() { t.yellow } else { t.fg })
            } else {
                Stroke::new(px(0.8), t.bg.gamma_multiply(0.8))
            };
            painter.circle(pos, r, fill, stroke);
            if show_icons && r * zoom > 9.0 {
                let (glyph, _) = file_icons::for_file(&n.name, t.dark);
                painter.text(pos, Align2::CENTER_CENTER, glyph.to_string(), FontId::new(r * 1.15, file_icons::family()), t.bg_dark);
            }
            let want_label = show_names && r * zoom > 5.0 && labels < MAX_LABELS && matched || sel == Some(i) || hov == Some(i);
            if want_label {
                labels += 1;
                let color = if matched { t.fg } else { t.fg_dim };
                let size = text_size(if sel == Some(i) || hov == Some(i) { 12.0 } else { 10.5 });
                painter.text(pos + Vec2::new(0.0, r + px(2.0)), Align2::CENTER_TOP, &n.name, FontId::proportional(size), color);
            }
        }

        // Directory labels sit on top of their hull and select the directory when clicked.
        for &d in &g.dirs {
            if d == 0 || redundant.contains(&d) {
                continue;
            }
            let Some(&r) = st.layout.hull.get(&d) else { continue };
            let n = &g.nodes[d];
            let big_enough = r * zoom > 40.0 || n.depth == 1;
            if !big_enough {
                continue;
            }
            let size = text_size(if n.depth == 1 { 14.0 } else { 11.5 });
            let anchor = n.pos - Vec2::new(0.0, r + px(2.0));
            let col = color_of(n, &colors, t);
            let label = if n.depth == 1 { n.name.clone() + "/" } else { n.name.clone() };
            let galley = painter.layout_no_wrap(label, FontId::proportional(size), if matches(n) { col } else { col.gamma_multiply(0.4) });
            let rect = Align2::CENTER_BOTTOM.anchor_size(anchor, galley.size());
            if !clip.intersects(rect) {
                continue;
            }
            let resp = ui.interact(rect, ui.id().with(("dir", d)), Sense::click());
            if resp.clicked() {
                new_sel = Some(d);
            }
            if resp.hovered() {
                new_hov = Some(d);
            }
            painter.galley(rect.min, galley, col);
        }

        if kicked {
            st.layout.kick();
        }
        st.hovered = new_hov;
        if let Some(s) = new_sel {
            st.selected = Some(s);
        }
    }
}

/// One accent colour per top-level directory.
fn top_colors(g: &ProjectGraph, t: &Theme) -> HashMap<usize, Color32> {
    let palette = [t.blue, t.purple, t.cyan, t.green, t.yellow, t.orange, t.red];
    g.nodes.iter().enumerate().filter(|(_, n)| n.dir == Some(0) && n.is_dir()).enumerate().map(|(k, (i, _))| (i, palette[k % palette.len()])).collect()
}

fn color_of(n: &Node, colors: &HashMap<usize, Color32>, t: &Theme) -> Color32 {
    match n.kind {
        NodeKind::Dir if n.dir == Some(0) => colors.get(&n.top.unwrap_or(usize::MAX)).copied().unwrap_or(t.fg_dim),
        _ => n.top.and_then(|top| colors.get(&top)).copied().unwrap_or(t.fg_dim),
    }
}
