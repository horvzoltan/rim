//! Force-directed placement, clustered by directory. Files repel each other, imports pull
//! them together, and each file is drawn toward its directory's centre. Directories sit at
//! the centroid of their contents and sibling clusters push each other apart as a whole.
//! Runs a few iterations per frame so the picture settles while it is shown.

use std::collections::HashMap;

use egui::{Pos2, Vec2, pos2, vec2};

use super::ProjectGraph;

/// Ideal distance between connected files, in scene units.
pub const K: f32 = 34.0;
const MAX_ITER: u32 = 700;

pub struct Layout {
    pub temp: f32,
    pub iter: u32,
    converged: bool,
    /// Per directory node: every file below it (recursively).
    members: HashMap<usize, Vec<usize>>,
    /// Per directory node: hull radius around its centroid, refreshed each step.
    pub hull: HashMap<usize, f32>,
}

impl Default for Layout {
    fn default() -> Self {
        Self { temp: K * 2.0, iter: 0, converged: false, members: HashMap::new(), hull: HashMap::new() }
    }
}

/// Small deterministic pseudo-random sequence (positions must not jump between runs).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32) / (u32::MAX >> 1) as f32
    }
}

impl Layout {
    /// Initial positions: each top-level cluster on a ring, its files jittered around the centre.
    pub fn seed(&mut self, g: &mut ProjectGraph) {
        self.members.clear();
        for &d in &g.dirs {
            let mut m = Vec::new();
            for (i, n) in g.nodes.iter().enumerate() {
                if n.is_dir() {
                    continue;
                }
                let mut cur = n.dir;
                while let Some(c) = cur {
                    if c == d {
                        m.push(i);
                        break;
                    }
                    cur = g.nodes[c].dir;
                }
            }
            self.members.insert(d, m);
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let groups: Vec<usize> = g.nodes.iter().enumerate().filter(|(_, n)| n.dir == Some(0) && n.is_dir()).map(|(i, _)| i).collect();
        let n = g.nodes.len().max(1) as f32;
        let ring = K * n.sqrt() * 0.9;
        let mut centres: HashMap<usize, Pos2> = HashMap::new();
        for (gi, &d) in groups.iter().enumerate() {
            let a = gi as f32 / groups.len() as f32 * std::f32::consts::TAU;
            centres.insert(d, pos2(a.cos() * ring, a.sin() * ring));
        }
        for i in 0..g.nodes.len() {
            let centre = g.nodes[i].top.and_then(|t| centres.get(&t)).copied().unwrap_or(Pos2::ZERO);
            let size = g.nodes[i].top.and_then(|t| self.members.get(&t)).map(|m| m.len()).unwrap_or(4) as f32;
            let spread = K * size.sqrt() * 0.6 + K;
            let a = rng.next() * std::f32::consts::TAU;
            let r = rng.next().sqrt() * spread;
            g.nodes[i].pos = centre + vec2(a.cos() * r, a.sin() * r);
            g.nodes[i].vel = Vec2::ZERO;
        }
        self.place_dirs(g);
        self.kick();
    }

    /// Files below a directory, recursively.
    pub fn member_count(&self, dir: usize) -> usize {
        self.members.get(&dir).map(Vec::len).unwrap_or(0)
    }

    pub fn kick(&mut self) {
        self.temp = K * 2.0;
        self.iter = 0;
        self.converged = false;
    }

    pub fn converged(&self) -> bool {
        self.converged
    }

    /// Directories at the centroid of their files; hull radii updated.
    fn place_dirs(&mut self, g: &mut ProjectGraph) {
        for &d in &g.dirs {
            let m = &self.members[&d];
            if m.is_empty() {
                continue;
            }
            let mut c = Vec2::ZERO;
            for &i in m {
                c += g.nodes[i].pos.to_vec2();
            }
            let c = (c / m.len() as f32).to_pos2();
            if !g.nodes[d].pinned {
                g.nodes[d].pos = c;
            }
            let r = m.iter().map(|&i| (g.nodes[i].pos - c).length() + g.nodes[i].radius).fold(0.0, f32::max);
            self.hull.insert(d, r + 10.0);
        }
    }

    /// Run up to `iters` iterations; returns true once the layout has settled.
    pub fn step(&mut self, g: &mut ProjectGraph, iters: usize) -> bool {
        if self.converged || g.nodes.is_empty() {
            self.converged = true;
            return true;
        }
        if self.members.is_empty() {
            self.seed(g);
        }
        let n = g.nodes.len();
        let mut disp = vec![Vec2::ZERO; n];
        let cell = K * 4.0;
        for _ in 0..iters {
            disp.iter_mut().for_each(|d| *d = Vec2::ZERO);

            // Repulsion between nearby files (uniform grid, neighbouring cells only).
            let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
            for (i, node) in g.nodes.iter().enumerate() {
                if node.is_dir() {
                    continue;
                }
                let key = ((node.pos.x / cell).floor() as i32, (node.pos.y / cell).floor() as i32);
                grid.entry(key).or_default().push(i);
            }
            for (&(cx, cy), ids) in &grid {
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        let Some(other) = grid.get(&(cx + dx, cy + dy)) else { continue };
                        for &i in ids {
                            for &j in other {
                                // Each pair is seen from both cells; count it once.
                                if j <= i {
                                    continue;
                                }
                                let a = g.nodes[i].pos;
                                let b = g.nodes[j].pos;
                                let mut delta = a - b;
                                let mut d = delta.length();
                                if d < 0.01 {
                                    delta = vec2(0.37 * (i as f32 + 1.0), 0.61 * (j as f32 + 1.0));
                                    d = delta.length();
                                }
                                if d > cell {
                                    continue;
                                }
                                let touch = g.nodes[i].radius + g.nodes[j].radius + 4.0;
                                let mut f = 1.6 * K * K / d;
                                if d < touch {
                                    f += (touch - d) * 2.0;
                                }
                                let push = delta / d * f;
                                disp[i] += push;
                                disp[j] -= push;
                            }
                        }
                    }
                }
            }

            // Imports pull files together.
            for e in &g.edges {
                let delta = g.nodes[e.to].pos - g.nodes[e.from].pos;
                let d = delta.length().max(0.01);
                let w = 1.0 + 0.25 * (e.weight.min(8) as f32 - 1.0);
                let f = d * d / K * 0.2 * w;
                let pull = delta / d * f.min(d * 0.5);
                if !g.nodes[e.from].is_dir() {
                    disp[e.from] += pull;
                }
                if !g.nodes[e.to].is_dir() {
                    disp[e.to] -= pull;
                }
            }

            // Files lean toward their directory's centre; root-level files toward the origin.
            for i in 0..n {
                if g.nodes[i].is_dir() {
                    continue;
                }
                let centre = g.nodes[i].dir.map(|d| g.nodes[d].pos).unwrap_or(Pos2::ZERO);
                let delta = centre - g.nodes[i].pos;
                let d = delta.length();
                if d > 0.01 {
                    let strength = if g.nodes[i].dir == Some(0) { 0.02 } else { 0.12 };
                    disp[i] += delta * strength;
                }
                // Everything drifts gently toward the middle so clusters stay in view.
                disp[i] -= g.nodes[i].pos.to_vec2() * 0.004;
            }

            // Sibling clusters (and loose files next to them) push each other apart as units.
            let dirs: Vec<usize> = g.dirs.clone();
            for a_idx in 0..dirs.len() {
                let a = dirs[a_idx];
                let (Some(pa), Some(&ra)) = (g.nodes[a].dir, self.hull.get(&a)) else { continue };
                for &b in &dirs[a_idx + 1..] {
                    if g.nodes[b].dir != Some(pa) {
                        continue;
                    }
                    let Some(&rb) = self.hull.get(&b) else { continue };
                    let delta = g.nodes[a].pos - g.nodes[b].pos;
                    let d = delta.length().max(0.01);
                    let overlap = ra + rb + K * 0.5 - d;
                    if overlap > 0.0 {
                        let push = delta / d * overlap * 0.3;
                        for &i in &self.members[&a] {
                            disp[i] += push;
                        }
                        for &i in &self.members[&b] {
                            disp[i] -= push;
                        }
                    }
                }
                // Files directly in the parent directory stay outside sibling clusters.
                for i in 0..n {
                    if g.nodes[i].is_dir() || g.nodes[i].dir != Some(pa) {
                        continue;
                    }
                    let delta = g.nodes[i].pos - g.nodes[a].pos;
                    let d = delta.length().max(0.01);
                    let overlap = ra + g.nodes[i].radius + 6.0 - d;
                    if overlap > 0.0 {
                        disp[i] += delta / d * overlap * 0.5;
                    }
                }
            }

            // Apply, limited by the temperature.
            let mut max_move: f32 = 0.0;
            for i in 0..n {
                if g.nodes[i].is_dir() || g.nodes[i].pinned {
                    continue;
                }
                let d = disp[i];
                let len = d.length();
                if len < 0.001 {
                    continue;
                }
                let step = d / len * len.min(self.temp);
                g.nodes[i].pos += step;
                max_move = max_move.max(step.length());
            }
            self.place_dirs(g);
            self.temp = (self.temp * 0.96).max(0.6);
            self.iter += 1;
            if max_move < 0.03 * K || self.iter >= MAX_ITER {
                self.converged = true;
                break;
            }
        }
        self.converged
    }
}

/// Bounding box of all nodes (including directory hulls), with padding.
pub fn bounds(g: &ProjectGraph, layout: &Layout) -> egui::Rect {
    let mut r = egui::Rect::NOTHING;
    for (i, n) in g.nodes.iter().enumerate() {
        let rad = if n.is_dir() { layout.hull.get(&i).copied().unwrap_or(0.0) } else { n.radius };
        r = r.union(egui::Rect::from_center_size(n.pos, Vec2::splat(rad * 2.0)));
    }
    if r.is_negative() || !r.is_finite() {
        egui::Rect::from_center_size(Pos2::ZERO, Vec2::splat(100.0))
    } else {
        r.expand(K * 2.0)
    }
}
