//! Tree-sitter highlighting: a real parse tree per document, edited incrementally as the text
//! changes and (re)parsed in small time slices, with the grammar's highlight query run only
//! over the lines on screen.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{FontId, Stroke};
use tree_sitter::{InputEdit, Language, ParseOptions, ParseState, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

use crate::text::hash_str;
use crate::theme::Theme;

/// What a capture is painted as; the theme decides the colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Plain,
    Keyword,
    Operator,
    String,
    Escape,
    Number,
    Function,
    Macro,
    Type,
    Constructor,
    Module,
    Attribute,
    Tag,
    Property,
    Key,
    Parameter,
    Builtin,
    Label,
    Comment,
    Error,
}

pub struct Grammar {
    pub language: Language,
    pub query: Query,
    /// Slot per capture index of `query`.
    slots: Vec<Slot>,
}

/// The names of the bundled grammars.
#[cfg(test)]
pub const GRAMMARS: [&str; 14] = ["rust", "javascript", "typescript", "tsx", "python", "c", "cpp", "go", "css", "json", "toml", "bash", "html", "yaml"];

/// The bundled grammar for a syntect syntax name (what `Syntaxes::detect` returns), if any.
pub fn grammar_for(syntax: &str) -> Option<&'static Grammar> {
    let key = match syntax {
        "Rust" => "rust",
        "JavaScript (Babel)" | "JavaScript" => "javascript",
        "TypeScript" => "typescript",
        "TypeScriptReact" => "tsx",
        "Python" => "python",
        "C" | "Objective-C" => "c",
        "C++" => "cpp",
        "Go" => "go",
        "CSS" => "css",
        "JSON" => "json",
        "TOML" => "toml",
        "Bourne Again Shell (bash)" => "bash",
        "HTML" => "html",
        "YAML" => "yaml",
        _ => return None,
    };
    Some(grammar(key))
}

/// A bundled grammar by name; compiled once and kept for the program's lifetime.
pub fn grammar(name: &'static str) -> &'static Grammar {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static Grammar>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    cache.entry(name).or_insert_with(|| Box::leak(Box::new(build(name))))
}

/// The tree-sitter language of a bundled grammar (for parsing without highlighting).
pub fn language(name: &str) -> Option<Language> {
    use tree_sitter_typescript as ts;
    Some(Language::new(match name {
        "rust" => tree_sitter_rust::LANGUAGE,
        "javascript" => tree_sitter_javascript::LANGUAGE,
        "typescript" => ts::LANGUAGE_TYPESCRIPT,
        "tsx" => ts::LANGUAGE_TSX,
        "python" => tree_sitter_python::LANGUAGE,
        "c" => tree_sitter_c::LANGUAGE,
        "cpp" => tree_sitter_cpp::LANGUAGE,
        "go" => tree_sitter_go::LANGUAGE,
        "css" => tree_sitter_css::LANGUAGE,
        "json" => tree_sitter_json::LANGUAGE,
        "toml" => tree_sitter_toml_ng::LANGUAGE,
        "bash" => tree_sitter_bash::LANGUAGE,
        "html" => tree_sitter_html::LANGUAGE,
        "yaml" => tree_sitter_yaml::LANGUAGE,
        _ => return None,
    }))
}

fn build(name: &'static str) -> Grammar {
    use tree_sitter_javascript as js;
    use tree_sitter_typescript as ts;
    let language = language(name).expect("bundled grammar");
    // TypeScript's and C++'s queries build on the base language's, which comes first as in
    // their packages (later patterns win).
    let source = match name {
        "rust" => tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
        "javascript" => format!("{}\n{}", js::HIGHLIGHT_QUERY, js::JSX_HIGHLIGHT_QUERY),
        "typescript" => format!("{}\n{}", js::HIGHLIGHT_QUERY, ts::HIGHLIGHTS_QUERY),
        "tsx" => format!("{}\n{}\n{}", js::HIGHLIGHT_QUERY, js::JSX_HIGHLIGHT_QUERY, ts::HIGHLIGHTS_QUERY),
        "python" => tree_sitter_python::HIGHLIGHTS_QUERY.to_string(),
        "c" => tree_sitter_c::HIGHLIGHT_QUERY.to_string(),
        "cpp" => format!("{}\n{}", tree_sitter_c::HIGHLIGHT_QUERY, tree_sitter_cpp::HIGHLIGHT_QUERY),
        "go" => tree_sitter_go::HIGHLIGHTS_QUERY.to_string(),
        "css" => tree_sitter_css::HIGHLIGHTS_QUERY.to_string(),
        "json" => tree_sitter_json::HIGHLIGHTS_QUERY.to_string(),
        "toml" => tree_sitter_toml_ng::HIGHLIGHTS_QUERY.to_string(),
        "bash" => tree_sitter_bash::HIGHLIGHT_QUERY.to_string(),
        "html" => tree_sitter_html::HIGHLIGHTS_QUERY.to_string(),
        "yaml" => tree_sitter_yaml::HIGHLIGHTS_QUERY.to_string(),
        _ => unreachable!(),
    };
    let query = Query::new(&language, &source).unwrap_or_else(|e| panic!("highlight query for {name}: {e}"));
    let slots = query.capture_names().iter().map(|c| slot_for(name, c)).collect();
    Grammar { language, query, slots }
}

/// The paint slot of a capture name (the grammars' vocabulary, `keyword`, `function.method`, …).
fn slot_for(grammar: &str, capture: &str) -> Slot {
    match capture {
        "comment" | "comment.documentation" => Slot::Comment,
        "keyword" => Slot::Keyword,
        "operator" => Slot::Operator,
        "string" | "string.special" => Slot::String,
        "string.special.key" => Slot::Key,
        "escape" | "string.escape" => Slot::Escape,
        "number" | "boolean" | "constant" | "constant.builtin" => Slot::Number,
        "function" | "function.method" | "function.builtin" => Slot::Function,
        "function.macro" | "function.special" => Slot::Macro,
        "type" | "type.builtin" => Slot::Type,
        "constructor" => Slot::Constructor,
        "module" | "namespace" => Slot::Module,
        // A Rust attribute is the whole `#[...]`: keep it quiet like a macro.
        "attribute" if grammar == "rust" => Slot::Macro,
        "attribute" => Slot::Attribute,
        "tag" => Slot::Tag,
        "tag.error" => Slot::Error,
        // Keys of config files stand out; fields in code read like variables.
        "property" if matches!(grammar, "toml" | "yaml" | "css") => Slot::Key,
        "property" => Slot::Property,
        "variable.parameter" => Slot::Parameter,
        "variable.builtin" => Slot::Builtin,
        "label" => Slot::Label,
        _ => Slot::Plain,
    }
}

fn format(slot: Slot, theme: &Theme, font: &FontId) -> TextFormat {
    let (color, italics, underline) = match slot {
        Slot::Plain | Slot::Property => (theme.fg, false, false),
        Slot::Keyword => (theme.purple, false, false),
        Slot::Operator | Slot::Escape | Slot::Macro | Slot::Module => (theme.cyan, false, false),
        Slot::String => (theme.green, false, false),
        Slot::Number | Slot::Label => (theme.orange, false, false),
        Slot::Function | Slot::Key => (theme.blue, false, false),
        Slot::Type | Slot::Constructor | Slot::Attribute => (theme.yellow, false, false),
        Slot::Tag | Slot::Builtin => (theme.red, false, false),
        Slot::Parameter => (theme.orange, true, false),
        Slot::Comment => (theme.comment, true, false),
        Slot::Error => (theme.red, false, true),
    };
    let mut fmt = TextFormat { font_id: font.clone(), color, ..Default::default() };
    fmt.italics = italics;
    if underline {
        fmt.underline = Stroke::new(1.0, color);
    }
    fmt
}

/// Time given to parsing per frame while the document is shown.
const FRAME_BUDGET: Duration = Duration::from_millis(6);

type JobKey = (u64, u64, u32, usize, usize);

/// Per-document state: the tree for the current text, and the parse in progress when the
/// text changed or the file was just opened.
pub struct TsCache {
    grammar: &'static Grammar,
    parser: Parser,
    /// The text `tree` (or the parse in progress) is for.
    text: String,
    text_hash: u64,
    line_starts: Vec<usize>,
    tree: Option<Tree>,
    /// A parse in progress, resumed each call within a time budget, with the edited old tree
    /// when it is incremental.
    parsing: Option<Option<Tree>>,
    last: Option<(JobKey, LayoutJob)>,
}

impl TsCache {
    pub fn new(grammar: &'static Grammar) -> Self {
        let mut parser = Parser::new();
        parser.set_language(&grammar.language).expect("grammar matches the linked tree-sitter");
        Self { grammar, parser, text: String::new(), text_hash: 0, line_starts: vec![0], tree: None, parsing: None, last: None }
    }

    pub fn needs_more(&self) -> bool {
        self.parsing.is_some()
    }

    /// Whether the text is parsed and up to date (tests).
    #[cfg(test)]
    pub fn parsed(&self) -> bool {
        self.tree.is_some() && self.parsing.is_none()
    }

    pub fn layout_job_window(&mut self, text: &str, generation: u64, theme: &Theme, font: &FontId, visible: Option<(usize, usize)>) -> LayoutJob {
        let h = hash_str(text);
        if h != self.text_hash || (self.tree.is_none() && self.parsing.is_none()) {
            let old = match (self.tree.take(), self.parsing.take()) {
                // The previous parse finished: continue from its tree.
                (Some(mut t), None) => {
                    t.edit(&edit_between(&self.text, text));
                    Some(t)
                }
                // Mid-parse (or nothing yet): start over on the new text.
                _ => {
                    self.parser.reset();
                    None
                }
            };
            self.text.clear();
            self.text.push_str(text);
            self.text_hash = h;
            self.line_starts = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
            self.parsing = Some(old);
            self.last = None;
        }
        if let Some(old) = &self.parsing {
            let budget = if visible.is_some() { FRAME_BUDGET } else { Duration::MAX };
            let started = Instant::now();
            let mut progress = |_: &ParseState| if started.elapsed() > budget { ControlFlow::Break(()) } else { ControlFlow::Continue(()) };
            let bytes = self.text.as_bytes();
            let mut read = |offset: usize, _: Point| &bytes[offset.min(bytes.len())..];
            let opts = ParseOptions::new().progress_callback(&mut progress);
            if let Some(t) = self.parser.parse_with_options(&mut read, old.as_ref(), Some(opts)) {
                self.tree = Some(t);
                self.parsing = None;
            }
        }
        let lines = self.line_starts.len();
        let (ws, we) = match visible {
            Some((a, b)) => ((a / super::WINDOW_BLOCK).saturating_sub(1) * super::WINDOW_BLOCK, ((b / super::WINDOW_BLOCK + 2) * super::WINDOW_BLOCK).min(lines)),
            None => (0, lines),
        };
        let key = (h, generation, font.size.to_bits(), ws, we);
        if let Some((k, job)) = &self.last
            && *k == key
            && self.parsing.is_none()
        {
            return job.clone();
        }
        let job = self.build(theme, font, ws, we);
        if self.parsing.is_none() {
            self.last = Some((key, job.clone()));
        }
        job
    }

    fn build(&self, theme: &Theme, font: &FontId, ws: usize, we: usize) -> LayoutJob {
        let text = &self.text;
        let a = self.line_starts.get(ws).copied().unwrap_or(text.len());
        let b = self.line_starts.get(we).copied().unwrap_or(text.len());
        let mut slots = vec![Slot::Plain; b - a];
        if let Some(tree) = &self.tree {
            let mut cursor = QueryCursor::new();
            cursor.set_byte_range(a..b);
            let mut caps: Vec<(usize, usize, usize, u32)> = Vec::new();
            let mut it = cursor.captures(&self.grammar.query, tree.root_node(), text.as_bytes());
            while let Some((m, ci)) = it.next() {
                let c = m.captures()[*ci];
                let r = c.node.byte_range();
                if r.start < b && r.end > a {
                    caps.push((r.start, r.end, m.pattern_index, c.index));
                }
            }
            // Outer nodes first, so inner captures paint over them; for one node the
            // last pattern in the query wins, as in tree-sitter's own highlighter.
            caps.sort_by_key(|&(s, e, p, _)| (s, Reverse(e), Reverse(p)));
            let mut last = None;
            for (s, e, _, idx) in caps {
                if last == Some((s, e)) {
                    continue;
                }
                last = Some((s, e));
                let slot = self.grammar.slots[idx as usize];
                slots[s.max(a) - a..e.min(b) - a].fill(slot);
            }
        }
        let mut sections = Vec::new();
        let mut push = |start: usize, end: usize, slot: Slot| {
            if start < end || (start == 0 && end == 0 && sections.is_empty()) {
                sections.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(start)..ByteIndex(end), format: format(slot, theme, font) });
            }
        };
        push(0, a, Slot::Plain);
        let mut run = a;
        for i in a + 1..=b {
            if i == b || slots[i - a] != slots[run - a] {
                push(run, i, slots[run - a]);
                run = i;
            }
        }
        push(b, text.len(), Slot::Plain);
        if text.is_empty() {
            push(0, 0, Slot::Plain);
        }
        let mut job = LayoutJob { text: text.clone(), sections, ..Default::default() };
        job.wrap.max_width = f32::INFINITY;
        job
    }
}

fn point_at(text: &str, byte: usize) -> Point {
    let before = &text[..byte];
    let row = before.bytes().filter(|b| *b == b'\n').count();
    let col = byte - before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    Point::new(row, col)
}

/// The single replacement that turns `old` into `new` (common prefix and suffix kept).
fn edit_between(old: &str, new: &str) -> InputEdit {
    let mut p = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(p) || !new.is_char_boundary(p) {
        p -= 1;
    }
    let max_s = old.len().min(new.len()) - p;
    let mut s = old.bytes().rev().zip(new.bytes().rev()).take(max_s).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(old.len() - s) || !new.is_char_boundary(new.len() - s) {
        s -= 1;
    }
    InputEdit {
        start_byte: p,
        old_end_byte: old.len() - s,
        new_end_byte: new.len() - s,
        start_position: point_at(old, p),
        old_end_position: point_at(old, old.len() - s),
        new_end_position: point_at(new, new.len() - s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_grammar_compiles_its_query() {
        for name in GRAMMARS {
            let g = grammar(name);
            assert!(!g.slots.is_empty(), "{name}");
        }
    }

    #[test]
    fn edits_are_located_on_char_boundaries() {
        let e = edit_between("aé😀b\nc", "aè😀b\nc");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (1, 3, 3));
        assert_eq!(e.start_position, Point::new(0, 1));
        let e = edit_between("ab\ncd", "ab\ncXd");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (4, 4, 5));
        assert_eq!(e.new_end_position, Point::new(1, 2));
    }
}
