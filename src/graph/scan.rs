//! Builds the project graph: walks the workspace (honoring .gitignore), makes a node per
//! directory and file, and turns import lines into edges. Language-agnostic line matching,
//! no parsers: fast and approximate is the point.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use egui::{Pos2, Vec2};

use super::{DescSource, Node, NodeKind, ProjectGraph, describe, radius_for};
use crate::config;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// One import line, before it is resolved to a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportRef {
    /// A path relative to the importing file's directory (`./x`, `../y/z.h`), or to the
    /// project root when it starts with `/`, `@/` or `~/`.
    Relative(String),
    /// `mod x;`
    RustMod(String),
    /// `use crate::a::b`, `use super::a`, `use self::a`, `use a::b` (bare).
    RustPath { kind: RustKind, segs: Vec<String> },
    /// `from .a.b import c` (levels = leading dots), `import a.b` (levels = 0).
    Py { levels: usize, segs: Vec<String>, names: Vec<String> },
    /// A Go import path (matched against `go.mod` module paths).
    GoPkg(String),
    /// A package or system header: never a project file, counted as external.
    External(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RustKind {
    Crate,
    /// `super::` repeated n times.
    Super(usize),
    SelfMod,
    Bare,
}

use super::ts_imports;

// ---------- extraction ----------

fn quoted(s: &str) -> Option<&str> {
    let (q, rest) = s.split_at(s.find(['"', '\''])?);
    let _ = q;
    let quote = rest.chars().next()?;
    let inner = &rest[1..];
    let end = inner.find(quote)?;
    Some(&inner[..end])
}

pub(super) fn is_js_relative(spec: &str) -> bool {
    spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') || spec.starts_with("@/") || spec.starts_with("~/")
}

/// Rust `use` tree → list of paths (`a::{b, c::d}` → `[a::b, a::c::d]`); aliases and `*` dropped.
pub(super) fn rust_use_paths(tree: &str) -> Vec<Vec<String>> {
    fn walk(s: &str, prefix: &[String], out: &mut Vec<Vec<String>>) {
        let s = s.trim();
        let s = s.split(" as ").next().unwrap_or(s).trim();
        if s.is_empty() {
            return;
        }
        if let Some(brace) = s.find('{') {
            let head = s[..brace].trim().trim_end_matches("::");
            let mut p = prefix.to_vec();
            p.extend(head.split("::").map(str::trim).filter(|x| !x.is_empty()).map(String::from));
            let inner = s[brace + 1..].trim_end_matches('}');
            // Split on commas at brace depth 0.
            let (mut depth, mut start) = (0, 0);
            let bytes = inner.as_bytes();
            for (i, &b) in bytes.iter().enumerate() {
                match b {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    b',' if depth == 0 => {
                        walk(&inner[start..i], &p, out);
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            walk(&inner[start..], &p, out);
        } else {
            let mut p = prefix.to_vec();
            p.extend(s.split("::").map(str::trim).filter(|x| !x.is_empty() && *x != "*").map(String::from));
            if !p.is_empty() {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(tree, &[], &mut out);
    out
}

pub(super) fn rust_path(mut segs: Vec<String>) -> Option<ImportRef> {
    let first = segs.first()?.as_str();
    let kind = match first {
        "crate" => {
            segs.remove(0);
            RustKind::Crate
        }
        "self" => {
            segs.remove(0);
            RustKind::SelfMod
        }
        "super" => {
            let n = segs.iter().take_while(|s| *s == "super").count();
            segs.drain(..n);
            RustKind::Super(n)
        }
        _ => RustKind::Bare,
    };
    // `self` at the end of a group (`use a::{self, b}`) names the module itself.
    segs.retain(|s| s != "self");
    Some(ImportRef::RustPath { kind, segs })
}

fn extract_css(text: &str, out: &mut Vec<ImportRef>) {
    for raw in text.lines() {
        let line = raw.trim();
        for key in ["@import ", "@use ", "@forward "] {
            if let Some(rest) = line.strip_prefix(key) {
                let rest = rest.trim_start().trim_start_matches("url(");
                if let Some(q) = quoted(rest) {
                    if !q.starts_with("http") {
                        out.push(ImportRef::Relative(q.to_string()));
                    }
                }
            }
        }
    }
}

/// Import lines of a file, by language id (see `config::language_id`).
pub fn extract_imports(lang: &str, text: &str) -> Vec<ImportRef> {
    let mut out = Vec::new();
    match lang {
        l if ts_imports::supports(l) => ts_imports::extract(l, text, &mut out),
        "vue" | "svelte" => ts_imports::extract_script_blocks(text, &mut out),
        "css" | "scss" | "less" => extract_css(text, &mut out),
        _ => {}
    }
    out
}

// ---------- resolution ----------

/// Lexically normalize `a/b/../c` → `a/c` (no filesystem access).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

struct Resolver<'a> {
    g: &'a ProjectGraph,
    /// Directories containing a `Cargo.toml`, `go.mod` module paths → their directory.
    crate_roots: Vec<PathBuf>,
    go_modules: Vec<(String, PathBuf)>,
}

impl Resolver<'_> {
    fn file(&self, p: &Path) -> Option<usize> {
        self.g.node_at(&normalize(p)).filter(|&i| !self.g.nodes[i].is_dir())
    }

    fn dir(&self, p: &Path) -> Option<usize> {
        self.g.node_at(&normalize(p)).filter(|&i| self.g.nodes[i].is_dir())
    }

    fn any(&self, p: &Path) -> Option<usize> {
        self.g.node_at(&normalize(p))
    }

    fn relative(&self, from: &Path, spec: &str) -> Option<usize> {
        let base: PathBuf = if let Some(rest) = spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/")) {
            let src = self.g.root.join("src");
            return self.relative_in(if self.dir(&src).is_some() { &src } else { &self.g.root }, rest);
        } else if let Some(rest) = spec.strip_prefix('/') {
            return self.relative_in(&self.g.root, rest);
        } else {
            from.parent()?.to_path_buf()
        };
        self.relative_in(&base, spec)
    }

    fn relative_in(&self, base: &Path, spec: &str) -> Option<usize> {
        let p = base.join(spec);
        if let Some(i) = self.file(&p) {
            return Some(i);
        }
        let stem = p.to_string_lossy().to_string();
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "d.ts", "py", "h", "hpp", "scss", "css", "rs"] {
            if let Some(i) = self.file(Path::new(&format!("{stem}.{ext}"))) {
                return Some(i);
            }
        }
        // `./x.js` written for the compiled output of `x.ts`.
        for (from, to) in [(".js", ".ts"), (".js", ".tsx"), (".jsx", ".tsx"), (".mjs", ".mts"), (".cjs", ".cts")] {
            if let Some(s) = stem.strip_suffix(from) {
                if let Some(i) = self.file(Path::new(&format!("{s}{to}"))) {
                    return Some(i);
                }
            }
        }
        // Sass partials: `_name.scss`.
        if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            let n = name.to_string_lossy();
            for ext in ["scss", "css"] {
                if let Some(i) = self.file(&parent.join(format!("_{n}.{ext}"))) {
                    return Some(i);
                }
            }
        }
        for idx in ["index.ts", "index.tsx", "index.js", "index.jsx", "mod.rs", "__init__.py"] {
            if let Some(i) = self.file(&p.join(idx)) {
                return Some(i);
            }
        }
        if let Some(i) = self.dir(&p) {
            return Some(i);
        }
        // C headers: project-wide include folders.
        if spec.ends_with(".h") || spec.ends_with(".hpp") {
            for d in ["include", "src"] {
                if let Some(i) = self.file(&self.g.root.join(d).join(spec)) {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Where `mod x;` children of this file live.
    fn mod_dir(&self, file: &Path) -> PathBuf {
        let dir = file.parent().unwrap_or(Path::new("")).to_path_buf();
        match file.file_name().and_then(|n| n.to_str()) {
            Some("mod.rs" | "lib.rs" | "main.rs") => dir,
            _ => dir.join(file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
        }
    }

    fn crate_src(&self, file: &Path) -> PathBuf {
        let mut best: Option<&PathBuf> = None;
        for r in &self.crate_roots {
            if file.starts_with(r) && best.is_none_or(|b| r.components().count() > b.components().count()) {
                best = Some(r);
            }
        }
        let root = best.cloned().unwrap_or_else(|| self.g.root.clone());
        let src = root.join("src");
        if self.dir(&src).is_some() { src } else { root }
    }

    /// The file that declares the module whose children live in `dir`.
    fn owner_of_dir(&self, dir: &Path) -> Option<usize> {
        if let Some(i) = self.file(&dir.join("mod.rs")) {
            return Some(i);
        }
        if let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) {
            if let Some(i) = self.file(&parent.join(format!("{}.rs", name.to_string_lossy()))) {
                return Some(i);
            }
        }
        self.file(&dir.join("lib.rs")).or_else(|| self.file(&dir.join("main.rs")))
    }

    /// Follow module path segments from `dir`, returning the deepest file (or directory) found.
    fn walk_rust(&self, dir: &Path, segs: &[String]) -> Option<usize> {
        let mut cur = dir.to_path_buf();
        let mut found = None;
        for s in segs {
            if let Some(i) = self.file(&cur.join(format!("{s}.rs"))) {
                found = Some(i);
                cur = self.mod_dir(&self.g.nodes[i].path);
            } else if let Some(i) = self.file(&cur.join(s).join("mod.rs")) {
                found = Some(i);
                cur = cur.join(s);
            } else if self.dir(&cur.join(s)).is_some() {
                cur = cur.join(s);
            } else {
                break;
            }
        }
        found
    }

    fn rust(&self, from: &Path, kind: RustKind, segs: &[String]) -> Option<usize> {
        match kind {
            RustKind::Crate => {
                let src = self.crate_src(from);
                self.walk_rust(&src, segs).or_else(|| if segs.is_empty() { self.owner_of_dir(&src) } else { None })
            }
            RustKind::SelfMod => self.walk_rust(&self.mod_dir(from), segs),
            RustKind::Super(n) => {
                let is_mod = matches!(from.file_name().and_then(|n| n.to_str()), Some("mod.rs"));
                let mut d = from.parent()?.to_path_buf();
                if is_mod {
                    d = d.parent()?.to_path_buf();
                }
                for _ in 1..n {
                    d = d.parent()?.to_path_buf();
                }
                if !d.starts_with(&self.g.root) {
                    return None;
                }
                self.walk_rust(&d, segs).or_else(|| self.owner_of_dir(&d))
            }
            RustKind::Bare => {
                let src = self.crate_src(from);
                let Some(first) = segs.first() else { return None };
                if self.file(&src.join(format!("{first}.rs"))).is_some() || self.dir(&src.join(first)).is_some() {
                    self.walk_rust(&src, segs)
                } else {
                    // Rust 2015 style: a child of the current module.
                    self.walk_rust(&self.mod_dir(from), segs).filter(|_| self.file(&self.mod_dir(from).join(format!("{first}.rs"))).is_some())
                }
            }
        }
    }

    fn walk_py(&self, dir: &Path, segs: &[String]) -> Option<usize> {
        let mut cur = dir.to_path_buf();
        let mut found = None;
        for s in segs {
            if let Some(i) = self.file(&cur.join(format!("{s}.py"))) {
                return Some(i);
            } else if let Some(i) = self.file(&cur.join(s).join("__init__.py")) {
                found = Some(i);
                cur = cur.join(s);
            } else if let Some(i) = self.dir(&cur.join(s)) {
                found = Some(i);
                cur = cur.join(s);
            } else {
                return found;
            }
        }
        found
    }

    fn python(&self, from: &Path, levels: usize, segs: &[String], names: &[String]) -> Option<usize> {
        let mut starts: Vec<PathBuf> = Vec::new();
        if levels > 0 {
            let mut d = from.parent()?.to_path_buf();
            for _ in 1..levels {
                d = d.parent()?.to_path_buf();
            }
            starts.push(d);
        } else {
            starts.push(self.g.root.clone());
            starts.push(self.g.root.join("src"));
            // The top of the package the importing file belongs to.
            let mut d = from.parent()?.to_path_buf();
            while d.parent().is_some_and(|p| self.file(&p.join("__init__.py")).is_some()) {
                d = d.parent()?.to_path_buf();
            }
            if let Some(p) = d.parent() {
                starts.push(p.to_path_buf());
            }
        }
        for start in &starts {
            if !start.starts_with(&self.g.root) {
                continue;
            }
            // `from pkg import module` names a file too.
            for n in names {
                let mut s = segs.to_vec();
                s.push(n.clone());
                if let Some(i) = self.walk_py(start, &s) {
                    if self.g.nodes[i].rel.ends_with(&format!("{n}.py")) {
                        return Some(i);
                    }
                }
            }
            if let Some(i) = self.walk_py(start, segs) {
                return Some(i);
            }
        }
        None
    }

    fn go(&self, pkg: &str) -> Option<usize> {
        for (module, dir) in &self.go_modules {
            if let Some(rest) = pkg.strip_prefix(module.as_str()) {
                let rest = rest.trim_start_matches('/');
                return self.any(&dir.join(rest));
            }
        }
        None
    }

    fn resolve(&self, from: &Path, r: &ImportRef) -> Option<usize> {
        match r {
            ImportRef::Relative(s) => self.relative(from, s),
            ImportRef::RustMod(name) => {
                let d = self.mod_dir(from);
                self.file(&d.join(format!("{name}.rs"))).or_else(|| self.file(&d.join(name).join("mod.rs")))
            }
            ImportRef::RustPath { kind, segs } => self.rust(from, *kind, segs),
            ImportRef::Py { levels, segs, names } => self.python(from, *levels, segs, names),
            ImportRef::GoPkg(p) => self.go(p),
            ImportRef::External(_) => None,
        }
    }
}

// ---------- scanning ----------

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

struct Scanned {
    rel: String,
    text: Option<String>,
    hash: u64,
    loc: u32,
}

fn read_files(root: &Path) -> Vec<Scanned> {
    let mut out = Vec::new();
    for e in crate::search::walker(root).flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = e.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.starts_with(".rim/") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if meta.len() > MAX_FILE_BYTES {
            out.push(Scanned { rel, text: None, hash: meta.len(), loc: 0 });
            continue;
        }
        let Ok(bytes) = std::fs::read(e.path()) else { continue };
        let hash = hash_bytes(&bytes);
        match String::from_utf8(bytes) {
            Ok(text) => {
                let loc = text.lines().count() as u32;
                out.push(Scanned { rel, text: Some(text), hash, loc });
            }
            Err(_) => out.push(Scanned { rel, text: None, hash, loc: 0 }),
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn node(root: &Path, rel: &str, kind: NodeKind, dir: Option<usize>, depth: usize) -> Node {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    Node {
        path: if rel.is_empty() { root.to_path_buf() } else { root.join(rel) },
        rel: rel.to_string(),
        name,
        kind,
        loc: 0,
        dir,
        top: None,
        depth,
        lang: None,
        pos: Pos2::ZERO,
        vel: Vec2::ZERO,
        radius: 3.0,
        pinned: false,
        desc: None,
        desc_src: DescSource::None,
        hash: 0,
        external: 0,
    }
}

/// Build the graph of `root`. With more than `max_files` files only directories are kept.
pub fn scan(root: &Path, max_files: usize) -> ProjectGraph {
    let root = root.canonicalize().unwrap_or(root.to_path_buf());
    let files = read_files(&root);
    let mut g = ProjectGraph { root: root.clone(), ..Default::default() };
    g.nodes.push(node(&root, "", NodeKind::Dir, None, 0));
    g.index.insert(root.clone(), 0);
    g.dirs.push(0);

    let mut texts: HashMap<usize, String> = HashMap::new();
    for f in files {
        // Ensure every ancestor directory has a node.
        let mut dir = 0;
        let mut depth = 0;
        let parts: Vec<&str> = f.rel.split('/').collect();
        let mut prefix = String::new();
        for part in &parts[..parts.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            depth += 1;
            let p = root.join(&prefix);
            dir = match g.index.get(&p) {
                Some(&i) => i,
                None => {
                    let i = g.nodes.len();
                    g.nodes.push(node(&root, &prefix, NodeKind::Dir, Some(dir), depth));
                    g.index.insert(p, i);
                    g.dirs.push(i);
                    i
                }
            };
        }
        let i = g.nodes.len();
        let mut n = node(&root, &f.rel, NodeKind::File, Some(dir), depth + 1);
        let ext = Path::new(&f.rel).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let lang = config::language_id_for(&ext, &n.name);
        n.lang = if lang == "plaintext" { None } else { Some(lang) };
        n.loc = f.loc;
        n.hash = f.hash;
        n.radius = radius_for(f.loc);
        if let Some(t) = &f.text {
            if let Some(d) = describe::heuristic(n.lang, t) {
                n.desc = Some(d);
                n.desc_src = DescSource::Heuristic;
            }
        }
        g.index.insert(n.path.clone(), i);
        g.nodes.push(n);
        if let Some(t) = f.text {
            texts.insert(i, t);
        }
        g.file_count += 1;
    }

    // Top-level directory of every node.
    for i in 0..g.nodes.len() {
        let mut cur = i;
        let mut top = None;
        while let Some(d) = g.nodes[cur].dir {
            if d == 0 {
                top = if g.nodes[cur].is_dir() { Some(cur) } else { None };
                break;
            }
            cur = d;
        }
        g.nodes[i].top = top;
    }

    // Edges.
    let crate_roots: Vec<PathBuf> = g
        .nodes
        .iter()
        .filter(|n| n.name == "Cargo.toml")
        .filter_map(|n| n.path.parent().map(Path::to_path_buf))
        .collect();
    let go_modules: Vec<(String, PathBuf)> = g
        .nodes
        .iter()
        .filter(|n| n.name == "go.mod")
        .filter_map(|n| {
            let t = std::fs::read_to_string(&n.path).ok()?;
            let m = t.lines().find_map(|l| l.strip_prefix("module "))?.trim().to_string();
            Some((m, n.path.parent()?.to_path_buf()))
        })
        .collect();
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut external: Vec<(usize, u32)> = Vec::new();
    {
        let r = Resolver { g: &g, crate_roots, go_modules };
        for (&i, text) in &texts {
            let Some(lang) = g.nodes[i].lang else { continue };
            let mut ext = 0;
            for imp in extract_imports(lang, text) {
                match r.resolve(&g.nodes[i].path, &imp) {
                    Some(j) => edges.push((i, j)),
                    None => ext += 1,
                }
            }
            if ext > 0 {
                external.push((i, ext));
            }
        }
    }
    g.add_edges(edges);
    for (i, n) in external {
        g.nodes[i].external = n;
    }

    // Directory sizes, hashes and README descriptions, deepest first.
    for &d in g.dirs.iter().rev() {
        let kids: Vec<usize> = g.children(d).collect();
        let loc = kids.iter().map(|&k| g.nodes[k].loc).sum();
        let mut pairs: Vec<(String, u64)> = kids.iter().map(|&k| (g.nodes[k].rel.clone(), g.nodes[k].hash)).collect();
        pairs.sort();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        pairs.hash(&mut h);
        g.nodes[d].loc = loc;
        g.nodes[d].hash = h.finish();
        g.nodes[d].radius = radius_for(loc);
        if let Some(&readme) = kids.iter().find(|&&k| g.nodes[k].name.eq_ignore_ascii_case("readme.md")) {
            if let Some(desc) = texts.get(&readme).and_then(|t| describe::readme_summary(t)) {
                g.nodes[d].desc = Some(desc);
                g.nodes[d].desc_src = DescSource::Heuristic;
            }
        }
    }

    if g.file_count > max_files {
        g = collapse(g);
    }
    g.recompute_degrees();
    g
}

/// Keep only the directory nodes, with file edges aggregated between their directories.
fn collapse(g: ProjectGraph) -> ProjectGraph {
    let mut out = ProjectGraph { root: g.root.clone(), collapsed: true, file_count: g.file_count, ..Default::default() };
    let map: HashMap<usize, usize> = g.dirs.iter().enumerate().map(|(i, &d)| (d, i)).collect();
    for &d in &g.dirs {
        let mut n = g.nodes[d].clone();
        n.dir = n.dir.map(|p| map[&p]);
        n.top = n.top.map(|p| map[&p]);
        let i = out.nodes.len();
        out.index.insert(n.path.clone(), i);
        out.nodes.push(n);
        out.dirs.push(i);
    }
    let dir_of = |i: usize| if g.nodes[i].is_dir() { map[&i] } else { map[&g.nodes[i].dir.unwrap_or(0)] };
    let pairs = g.edges.iter().flat_map(|e| std::iter::repeat_n((dir_of(e.from), dir_of(e.to)), e.weight as usize)).collect();
    out.add_edges(pairs);
    out
}
