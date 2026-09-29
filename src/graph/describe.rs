//! What a node does: cheap heuristics from the source itself (module doc comments, file
//! header comments, a directory's README), and prompts for Claude with a cache in
//! `<root>/.rim/graph.json` so each summary is generated once per file version.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{NodeKind, ProjectGraph};

const MAX_LEN: usize = 240;

fn tidy(lines: &[&str]) -> Option<String> {
    let text = lines.join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() < 4 {
        return None;
    }
    let mut out: String = text.chars().take(MAX_LEN).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    Some(out)
}

fn is_boilerplate(l: &str) -> bool {
    let l = l.to_lowercase();
    l.contains("copyright") || l.contains("spdx-") || l.contains("license") || l.contains("licence") || l.starts_with("#!") || l.contains("eslint") || l.contains("-*-")
}

/// A one-line description from the file's own comments, if it has any.
pub fn heuristic(lang: Option<&str>, text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().take(60).map(str::trim).collect();
    // Rust inner doc comments.
    let docs: Vec<&str> = lines.iter().filter_map(|l| l.strip_prefix("//!")).map(str::trim).take_while(|l| !l.is_empty()).collect();
    if !docs.is_empty() {
        return tidy(&docs);
    }
    // Python module docstring.
    if lang == Some("python") {
        let mut it = lines.iter().skip_while(|l| l.is_empty() || l.starts_with('#'));
        if let Some(first) = it.next() {
            for q in ["\"\"\"", "'''"] {
                if let Some(rest) = first.strip_prefix(q) {
                    if let Some(end) = rest.find(q) {
                        return tidy(&[&rest[..end]]);
                    }
                    let mut acc = vec![rest];
                    for l in it.by_ref() {
                        if let Some(end) = l.find(q) {
                            acc.push(&l[..end]);
                            break;
                        }
                        if l.is_empty() {
                            break;
                        }
                        acc.push(l);
                    }
                    return tidy(&acc);
                }
            }
        }
    }
    // A leading `/** … */` or `/* … */` block.
    if let Some(first) = lines.iter().find(|l| !l.is_empty()) {
        if first.starts_with("/*") {
            let mut acc = Vec::new();
            for raw in &lines {
                let (body, done) = match raw.strip_suffix("*/") {
                    Some(b) => (b, true),
                    None => (*raw, false),
                };
                let l = body.trim_start_matches("/**").trim_start_matches("/*").trim_start_matches('*').trim();
                if !l.is_empty() && !is_boilerplate(l) && !l.starts_with('@') {
                    acc.push(l);
                }
                if done || acc.len() >= 2 {
                    break;
                }
            }
            return tidy(&acc);
        }
    }
    // A leading `//` or `#` comment block (skipping shebangs and license headers). `#` only counts
    // for languages that comment with it, not for C preprocessor lines or CSS selectors.
    let hash_comments = matches!(
        lang,
        None | Some("python" | "shellscript" | "yaml" | "toml" | "ruby" | "perl" | "nix" | "r" | "cmake" | "makefile" | "dockerfile" | "ini" | "elixir" | "julia" | "powershell")
    );
    let mut acc = Vec::new();
    for l in &lines {
        if l.is_empty() && acc.is_empty() {
            continue;
        }
        let Some(c) = l.strip_prefix("///").or_else(|| l.strip_prefix("//")).or_else(|| if hash_comments { l.strip_prefix('#') } else { None }) else { break };
        let c = c.trim_start_matches(['/', '#', '!']).trim();
        if c.is_empty() || is_boilerplate(l) {
            if acc.is_empty() {
                continue;
            }
            break;
        }
        acc.push(c);
        if acc.len() >= 2 {
            break;
        }
    }
    if acc.is_empty() { None } else { tidy(&acc) }
}

/// The first paragraph of a README that is not a heading, badge or image.
pub fn readme_summary(text: &str) -> Option<String> {
    let mut acc = Vec::new();
    for l in text.lines().map(str::trim) {
        if l.is_empty() {
            if !acc.is_empty() {
                break;
            }
            continue;
        }
        if l.starts_with('#') || l.starts_with("![") || l.starts_with("[![") || l.starts_with('<') || l.starts_with("---") {
            if !acc.is_empty() {
                break;
            }
            continue;
        }
        acc.push(l);
    }
    tidy(&acc)
}

// ---------- cache ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub hash: u64,
    pub summary: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub entries: HashMap<String, Entry>,
}

fn one() -> u32 {
    1
}

impl Cache {
    pub fn path(root: &Path) -> PathBuf {
        root.join(".rim").join("graph.json")
    }

    pub fn load(root: &Path) -> Cache {
        std::fs::read_to_string(Self::path(root)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// The cached summary for this version of the node.
    pub fn get(&self, rel: &str, hash: u64) -> Option<&str> {
        self.entries.get(rel).filter(|e| e.hash == hash).map(|e| e.summary.as_str())
    }

    pub fn put(&mut self, rel: &str, hash: u64, summary: String, model: Option<String>) {
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        self.entries.insert(rel.to_string(), Entry { hash, summary, model, at });
    }

    pub fn save(&self, root: &Path) -> Result<(), String> {
        let path = Self::path(root);
        let dir = path.parent().ok_or("no parent")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut sorted: Vec<(&String, &Entry)> = self.entries.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut entries = serde_json::Map::new();
        for (k, e) in sorted {
            entries.insert(k.clone(), serde_json::to_value(e).map_err(|e| e.to_string())?);
        }
        let v = serde_json::json!({ "version": self.version.max(1), "entries": entries });
        let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".graph.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    /// Fill in Claude descriptions for nodes whose version matches.
    pub fn apply(&self, g: &mut ProjectGraph) {
        for n in &mut g.nodes {
            if let Some(s) = self.get(&n.rel, n.hash) {
                n.desc = Some(s.to_string());
                n.desc_src = super::DescSource::Claude;
            }
        }
    }
}

// ---------- prompts ----------

const FILE_TEXT_LIMIT: usize = 6000;

/// Everything needed to ask about one node, gathered on the UI thread and sent to a worker.
#[derive(Clone, Debug)]
pub struct Request {
    pub rel: String,
    pub hash: u64,
    pub kind: NodeKind,
    pub path: PathBuf,
    pub imports: Vec<String>,
    pub imported_by: Vec<String>,
    /// Directory children with what is known about them so far.
    pub children: Vec<(String, Option<String>)>,
    pub readme: Option<String>,
}

impl Request {
    pub fn for_node(g: &ProjectGraph, i: usize) -> Request {
        let n = &g.nodes[i];
        let (out, inc) = g.neighbors(i);
        let names = |v: Vec<usize>| v.into_iter().take(20).map(|j| g.nodes[j].rel.clone()).collect();
        let mut children: Vec<(String, Option<String>)> =
            g.children(i).map(|c| (g.nodes[c].name.clone() + if g.nodes[c].is_dir() { "/" } else { "" }, g.nodes[c].desc.clone())).collect();
        children.sort();
        children.truncate(60);
        let readme = g.children(i).find(|&c| g.nodes[c].name.eq_ignore_ascii_case("readme.md")).and_then(|c| {
            std::fs::read_to_string(&g.nodes[c].path).ok().map(|t| t.chars().take(1500).collect())
        });
        Request { rel: n.rel.clone(), hash: n.hash, kind: n.kind, path: n.path.clone(), imports: names(out), imported_by: names(inc), children, readme }
    }

    /// The prompt text; reads the file itself for file nodes.
    pub fn prompt(&self) -> String {
        let mut p = String::new();
        match self.kind {
            NodeKind::File => {
                p.push_str("Summarize what this source file does in 1-2 plain sentences (at most 40 words). ");
                p.push_str("Answer with the sentences only: no preamble, no markdown, no code.\n");
                p.push_str(&format!("Path: {}\n", self.rel));
                if !self.imports.is_empty() {
                    p.push_str(&format!("Imports: {}\n", self.imports.join(", ")));
                }
                if !self.imported_by.is_empty() {
                    p.push_str(&format!("Imported by: {}\n", self.imported_by.join(", ")));
                }
                p.push_str("---\n");
                let text = std::fs::read_to_string(&self.path).unwrap_or_default();
                let cut: String = text.chars().take(FILE_TEXT_LIMIT).collect();
                p.push_str(&cut);
                if cut.len() < text.len() {
                    p.push_str("\n…(truncated)");
                }
            }
            NodeKind::Dir => {
                p.push_str("Summarize what this directory of a software project is for in 1-2 plain sentences (at most 40 words). ");
                p.push_str("Answer with the sentences only: no preamble, no markdown.\n");
                p.push_str(&format!("Directory: {}\n", if self.rel.is_empty() { "(project root)" } else { &self.rel }));
                p.push_str("Contents:\n");
                for (name, desc) in &self.children {
                    match desc {
                        Some(d) => p.push_str(&format!("- {name}: {d}\n")),
                        None => p.push_str(&format!("- {name}\n")),
                    }
                }
                if let Some(r) = &self.readme {
                    p.push_str("README:\n");
                    p.push_str(r);
                    p.push('\n');
                }
            }
        }
        p
    }
}
