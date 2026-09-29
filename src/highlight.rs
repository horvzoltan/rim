//! Syntax highlighting producing egui `LayoutJob`s: tree-sitter for the bundled grammars
//! (a real parse tree, see `ts`), syntect (regex grammars) for every other language.
//! Both are incremental and only colour the lines on screen.

pub mod ts;

use std::ops::Range;
use std::path::Path;

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{Color32, FontId, Stroke};
use syntect::highlighting::{
    Color as SColor, FontStyle, HighlightIterator, HighlightState, Highlighter, ScopeSelectors,
    StyleModifier, Theme as SynTheme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::text::hash_str;
use crate::theme::Theme;

/// Files larger than this are shown without highlighting.
const MAX_HIGHLIGHT_BYTES: usize = 16 * 1024 * 1024;

pub struct Syntaxes {
    pub set: SyntaxSet,
    pub theme: SynTheme,
    /// Bumped whenever the theme changes, to invalidate caches.
    pub generation: u64,
}

impl Syntaxes {
    pub fn new(theme: &Theme) -> Self {
        Self { set: two_face::syntax::extra_newlines(), theme: syntect_theme(theme), generation: 1 }
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme = syntect_theme(theme);
        self.generation += 1;
    }

    /// Pick a syntax name for a path, falling back to the first line (shebangs etc).
    pub fn detect(&self, path: Option<&Path>, text: &str) -> String {
        let by_path = path.and_then(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            let name = p.file_name().and_then(|e| e.to_str()).unwrap_or("");
            self.set.find_syntax_by_extension(ext).or_else(|| self.set.find_syntax_by_extension(name))
        });
        let first_line = text.lines().next().unwrap_or("");
        by_path
            .or_else(|| self.set.find_syntax_by_first_line(first_line))
            .unwrap_or_else(|| self.set.find_syntax_plain_text())
            .name
            .clone()
    }

    pub fn get(&self, name: &str) -> &SyntaxReference {
        self.set.find_syntax_by_name(name).unwrap_or_else(|| self.set.find_syntax_plain_text())
    }
}

fn syntect_theme(t: &Theme) -> SynTheme {
    let c = |c: Color32| SColor { r: c.r(), g: c.g(), b: c.b(), a: 255 };
    let item = |sel: &str, fg: Color32, fs: Option<FontStyle>| ThemeItem {
        scope: sel.parse::<ScopeSelectors>().expect("valid scope selector"),
        style: StyleModifier { foreground: Some(c(fg)), background: None, font_style: fs },
    };
    let italic = Some(FontStyle::ITALIC);
    let underline = Some(FontStyle::UNDERLINE);
    SynTheme {
        name: Some(t.name.to_string()),
        author: None,
        settings: ThemeSettings { foreground: Some(c(t.fg)), background: Some(c(t.bg)), ..Default::default() },
        scopes: vec![
            item("variable, meta.path, punctuation", t.fg, None),
            item("keyword, storage.type, storage.modifier, keyword.control, variable.language.this", t.purple, None),
            item("keyword.operator, punctuation.accessor, punctuation.separator.namespace", t.cyan, None),
            item("string, string.quoted, punctuation.definition.string, markup.raw", t.green, None),
            item("constant.character.escape, string.regexp", t.cyan, None),
            item("constant.numeric, constant.language, constant.other, constant.character, support.constant, variable.other.constant", t.orange, None),
            item("entity.name.function, support.function, variable.function, meta.function-call.identifier", t.blue, None),
            item("entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.interface, support.type, support.class, entity.other.inherited-class, storage.type.primitive", t.yellow, None),
            item("entity.name.namespace, entity.name.module, support.module", t.cyan, None),
            item("support.macro, entity.name.macro, meta.macro, meta.preprocessor, keyword.other.directive", t.cyan, None),
            item("entity.name.tag, meta.tag.sgml", t.red, None),
            item("entity.other.attribute-name, meta.attribute", t.yellow, None),
            item("support.type.property-name, meta.object-literal.key, meta.mapping.key string, entity.name.tag.yaml, meta.tag.table.toml, entity.name.label", t.blue, None),
            item("variable.parameter", t.orange, italic),
            item("variable.language, variable.other.readwrite.instance", t.red, None),
            item("storage.modifier.lifetime, entity.name.lifetime", t.orange, italic),
            item("markup.heading, entity.name.section, punctuation.definition.heading", t.blue, None),
            item("markup.italic", t.fg, italic),
            item("markup.underline.link, string.other.link", t.cyan, underline),
            item("markup.inserted, meta.diff.header.to-file, punctuation.definition.inserted", t.green, None),
            item("markup.deleted, meta.diff.header.from-file, punctuation.definition.deleted", t.red, None),
            item("markup.changed", t.yellow, None),
            item("meta.diff.range, meta.diff.index, meta.separator", t.purple, None),
            item("invalid", t.red, underline),
            item("comment, punctuation.definition.comment", t.comment, italic),
        ],
    }
}

/// Parsing is the expensive, theme-independent part; it is kept per line. Colours are
/// derived on demand from each line's starting scope stack, only for lines on screen.
struct Line {
    text: String,
    /// Parser state and scope stack at the *start* of this line.
    parse: ParseState,
    stack: ScopeStack,
    ops: Vec<(usize, ScopeStackOp)>,
}

/// Lines after an edit that were not re-parsed yet, kept to detect when the parser re-converges.
struct Tail {
    lines: Vec<Line>,
    /// New-text index of `lines[0]`.
    new_start: usize,
    /// Parser state after the old text's last line.
    end: (ParseState, ScopeStack),
}

/// Lines longer than this are shown uncoloured instead of stalling the parser (minified files).
const MAX_LINE_BYTES: usize = 10_000;
/// Lines are coloured around the visible range in blocks, so small scrolls reuse the same job.
const WINDOW_BLOCK: usize = 128;

type JobKey = (u64, u64, u32, usize, usize);

/// Per-document highlight cache: the engine for the document's syntax, chosen on first use.
#[derive(Default)]
pub struct HlCache {
    syntax: String,
    engine: Engine,
}

#[derive(Default)]
enum Engine {
    #[default]
    None,
    Syntect(SyntectCache),
    Tree(ts::TsCache),
}

impl HlCache {
    /// Highlight the whole text (tests, chat code blocks).
    pub fn layout_job(&mut self, text: &str, syn: &Syntaxes, syntax: &str, theme: &Theme, font: &FontId) -> LayoutJob {
        self.layout_job_window(text, syn, syntax, theme, font, None)
    }

    /// True while part of the file still has to be parsed in the background.
    pub fn needs_more(&self) -> bool {
        match &self.engine {
            Engine::None => false,
            Engine::Syntect(c) => c.needs_more(),
            Engine::Tree(c) => c.needs_more(),
        }
    }

    /// Highlight for display. `visible` is the (first, last) visible line from the previous
    /// frame; only lines around it are coloured, the rest uses the plain text colour.
    pub fn layout_job_window(&mut self, text: &str, syn: &Syntaxes, syntax: &str, theme: &Theme, font: &FontId, visible: Option<(usize, usize)>) -> LayoutJob {
        if text.len() > MAX_HIGHLIGHT_BYTES {
            return LayoutJob::simple(text.to_owned(), font.clone(), theme.fg, f32::INFINITY);
        }
        if syntax != self.syntax || matches!(self.engine, Engine::None) {
            self.syntax = syntax.to_string();
            self.engine = match ts::grammar_for(syntax) {
                Some(g) => Engine::Tree(ts::TsCache::new(g)),
                None => Engine::Syntect(SyntectCache { syntax: syntax.to_string(), ..Default::default() }),
            };
        }
        match &mut self.engine {
            Engine::Tree(c) => c.layout_job_window(text, syn.generation, theme, font, visible),
            Engine::Syntect(c) => c.layout_job_window(text, syn, theme, font, visible),
            Engine::None => unreachable!(),
        }
    }

    #[cfg(test)]
    fn syntect(&self) -> &SyntectCache {
        match &self.engine {
            Engine::Syntect(c) => c,
            _ => panic!("not a syntect document"),
        }
    }

    #[cfg(test)]
    pub fn tree(&self) -> &ts::TsCache {
        match &self.engine {
            Engine::Tree(c) => c,
            _ => panic!("not a tree-sitter document"),
        }
    }
}

/// The syntect engine.
/// - Parsing is incremental: after an edit only lines from the change onwards are re-parsed,
///   stopping as soon as the parser state re-converges with the old parse.
/// - Parsing is lazy: only as far as needed to show the visible lines; the rest of the file is
///   parsed in small time slices while `needs_more()` is true.
/// - Colours are computed for the visible window only, so the job handed to egui each frame
///   has a few hundred sections instead of one per token of the whole file, and a theme
///   change needs no re-parse.
#[derive(Default)]
pub struct SyntectCache {
    syntax: String,
    text_hash: u64,
    lines: Vec<Line>,
    /// State after `lines.last()`, i.e. at the start of the next line to parse.
    next: Option<(ParseState, ScopeStack)>,
    total_lines: usize,
    tail: Option<Tail>,
    last: Option<(JobKey, LayoutJob)>,
}

impl SyntectCache {
    fn needs_more(&self) -> bool {
        self.lines.len() < self.total_lines
    }

    fn layout_job_window(&mut self, text: &str, syn: &Syntaxes, theme: &Theme, font: &FontId, visible: Option<(usize, usize)>) -> LayoutJob {
        let h = hash_str(text);
        let (ws, we) = match visible {
            Some((a, b)) => ((a / WINDOW_BLOCK).saturating_sub(1) * WINDOW_BLOCK, (b / WINDOW_BLOCK + 2) * WINDOW_BLOCK),
            None => (0, usize::MAX),
        };
        let key = (h, syn.generation, font.size.to_bits(), ws, we);
        let changed = h != self.text_hash || self.next.is_none();
        if !changed && !self.needs_more() && let Some((k, job)) = &self.last && *k == key {
            return job.clone();
        }
        let new: Vec<&str> = LinesWithEndings::from(text).collect();
        if changed {
            self.text_hash = h;
            self.apply_change(&new, syn);
        }
        // Lines in the window must be parsed now; beyond them, only a small time slice.
        let budget = if visible.is_some() { std::time::Duration::from_millis(3) } else { std::time::Duration::MAX };
        self.parse(&new, syn, we.min(new.len()), budget);
        if let Some((k, job)) = &self.last
            && *k == key
        {
            return job.clone();
        }
        let job = self.build(text, &new, syn, theme, font, ws, we);
        self.last = Some((key, job.clone()));
        job
    }

    /// Keep what is still valid of the previous parse for the new text.
    fn apply_change(&mut self, new: &[&str], syn: &Syntaxes) {
        let old_complete = self.next.is_some() && !self.needs_more();
        self.total_lines = new.len();
        let old = std::mem::take(&mut self.lines);
        let old_end = self.next.take();
        let prefix = old.iter().zip(new).take_while(|(o, n)| o.text == **n).count();
        // Lines after the change can be reused once the parser re-converges, but only when the
        // old parse reached the end (otherwise old and new tails can't be aligned).
        let suffix = if old_complete {
            let max = old.len().min(new.len()) - prefix;
            (0..max).take_while(|k| old[old.len() - 1 - k].text == new[new.len() - 1 - k]).count()
        } else {
            0
        };
        let mut old = old.into_iter();
        self.lines = old.by_ref().take(prefix).collect();
        let rest: Vec<Line> = old.collect();
        let next = match rest.first() {
            Some(l) => (l.parse.clone(), l.stack.clone()),
            // Only appended at the end: re-parse the last unchanged line to recover its end state.
            None => match self.lines.pop() {
                Some(l) => (l.parse, l.stack),
                None => (ParseState::new(syn.get(&self.syntax)), ScopeStack::new()),
            },
        };
        self.tail = match (suffix > 0, old_end) {
            (true, Some(end)) => {
                let skip = rest.len() - suffix;
                Some(Tail { lines: rest.into_iter().skip(skip).collect(), new_start: new.len() - suffix, end })
            }
            _ => None,
        };
        self.next = Some(next);
    }

    /// Parse until line `target`, then continue until `budget` runs out.
    fn parse(&mut self, new: &[&str], syn: &Syntaxes, target: usize, budget: std::time::Duration) {
        let Some((mut ps, mut stack)) = self.next.take() else { return };
        let started = std::time::Instant::now();
        while self.lines.len() < new.len() {
            let n = self.lines.len();
            if n >= target && n.is_multiple_of(16) && started.elapsed() >= budget {
                break;
            }
            if let Some(tail) = &self.tail
                && n >= tail.new_start
            {
                let k = n - tail.new_start;
                if k >= tail.lines.len() {
                    self.tail = None;
                } else if tail.lines[k].parse == ps && tail.lines[k].stack == stack {
                    // Re-converged with the old parse: the rest is still valid.
                    let tail = self.tail.take().unwrap();
                    self.lines.extend(tail.lines.into_iter().skip(k));
                    (ps, stack) = tail.end;
                    continue;
                }
            }
            let text = new[n];
            let start = (ps.clone(), stack.clone());
            let ops = if text.len() > MAX_LINE_BYTES { Vec::new() } else { ps.parse_line(text, &syn.set).unwrap_or_default() };
            for (_, op) in &ops {
                let _ = stack.apply(op);
            }
            self.lines.push(Line { text: text.to_string(), parse: start.0, stack: start.1, ops });
        }
        if self.lines.len() >= new.len() {
            self.tail = None;
        }
        self.next = Some((ps, stack));
    }

    #[allow(clippy::too_many_arguments)]
    fn build(&self, text: &str, new: &[&str], syn: &Syntaxes, theme: &Theme, font: &FontId, ws: usize, we: usize) -> LayoutJob {
        let mut job = LayoutJob { text: text.to_owned(), ..Default::default() };
        job.wrap.max_width = f32::INFINITY;
        let plain = TextFormat { font_id: font.clone(), color: theme.fg, ..Default::default() };
        let sections = &mut job.sections;
        let mut push = |range: Range<usize>, fmt: &TextFormat| {
            if range.is_empty() {
                return;
            }
            if let Some(last) = sections.last_mut()
                && last.format == *fmt
                && last.byte_range.end.0 == range.start
            {
                last.byte_range.end = ByteIndex(range.end);
                return;
            }
            sections.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(range.start)..ByteIndex(range.end), format: fmt.clone() });
        };
        let highlighter = Highlighter::new(&syn.theme);
        let colored_end = we.min(self.lines.len());
        let mut base = 0;
        for (n, line_text) in new.iter().enumerate() {
            let len = line_text.len();
            if n < ws || n >= colored_end {
                push(base..base + len, &plain);
                base += len;
                continue;
            }
            let line = &self.lines[n];
            let mut hs = HighlightState::new(&highlighter, line.stack.clone());
            let mut off = 0;
            for (style, piece) in HighlightIterator::new(&mut hs, &line.ops, line_text, &highlighter) {
                let fg = style.foreground;
                let color = if fg.a == 0 { theme.fg } else { Color32::from_rgb(fg.r, fg.g, fg.b) };
                let mut fmt = TextFormat { font_id: font.clone(), color, ..Default::default() };
                fmt.italics = style.font_style.contains(FontStyle::ITALIC);
                if style.font_style.contains(FontStyle::UNDERLINE) {
                    fmt.underline = Stroke::new(1.0, color);
                }
                push(base + off..base + off + piece.len(), &fmt);
                off += piece.len();
            }
            push(base + off..base + len, &plain);
            base += len;
        }
        if job.sections.is_empty() {
            job.sections.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(0)..ByteIndex(text.len()), format: plain });
        }
        job
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    fn setup() -> (Syntaxes, Theme, FontId) {
        let theme = crate::theme::by_name("tokyonight");
        (Syntaxes::new(&theme), theme, FontId::monospace(14.0))
    }

    fn big(lines: usize) -> String {
        (0..lines).map(|i| if i % 50 == 0 { format!("/* block {i}\n still comment */\n") } else { format!("fn f{i}() -> u32 {{ \"s{i}\".len() as u32 }} // c\n") }).collect()
    }

    /// Sections restricted to a byte range, for comparing a window against a full highlight.
    fn in_range(job: &LayoutJob, r: Range<usize>) -> Vec<(usize, usize, Color32, bool)> {
        let mut out = vec![];
        for s in &job.sections {
            let (a, b) = (s.byte_range.start.0.max(r.start), s.byte_range.end.0.min(r.end));
            if a < b {
                out.push((a, b, s.format.color, s.format.italics));
            }
        }
        // Normalize: merge neighbours with the same look.
        let mut merged: Vec<(usize, usize, Color32, bool)> = vec![];
        for x in out {
            match merged.last_mut() {
                Some(l) if l.1 == x.0 && l.2 == x.2 && l.3 == x.3 => l.1 = x.1,
                _ => merged.push(x),
            }
        }
        merged
    }

    fn line_bytes(text: &str, a: usize, b: usize) -> Range<usize> {
        let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        starts[a]..starts[b.min(starts.len() - 1)]
    }

    #[test]
    fn window_colours_match_a_full_highlight_and_parsing_is_lazy() {
        let (syn, theme, font) = setup();
        let text = big(20_000);
        let full = HlCache::default().layout_job(&text, &syn, "Java", &theme, &font);
        let mut c = HlCache::default();
        let job = c.layout_job_window(&text, &syn, "Java", &theme, &font, Some((0, 40)));
        assert!(c.needs_more(), "only the start of the file should be parsed on first show");
        assert!(c.syntect().lines.len() < 20_000);
        let r = line_bytes(&text, 0, 200);
        assert_eq!(in_range(&job, r.clone()), in_range(&full, r));
        assert!(job.sections.len() < 8_000, "window job has {} sections", job.sections.len());

        // Jump deep into the file: parsed on demand, and correct.
        let job = c.layout_job_window(&text, &syn, "Java", &theme, &font, Some((15_000, 15_040)));
        let r = line_bytes(&text, 14_950, 15_100);
        assert_eq!(in_range(&job, r.clone()), in_range(&full, r));

        // Background slices finish the parse.
        for _ in 0..10_000 {
            if !c.needs_more() {
                break;
            }
            c.layout_job_window(&text, &syn, "Java", &theme, &font, Some((15_000, 15_040)));
        }
        assert!(!c.needs_more());
    }

    #[test]
    fn edits_during_and_after_lazy_parsing_stay_correct() {
        let (syn, theme, font) = setup();
        let mut text = big(3_000);
        let mut c = HlCache::default();
        c.layout_job_window(&text, &syn, "Java", &theme, &font, Some((0, 40)));
        // Edit while most of the file is unparsed, then open a comment that changes everything after.
        text.insert(10, 'x');
        c.layout_job_window(&text, &syn, "Java", &theme, &font, Some((0, 40)));
        let at = text.find("fn f5(").unwrap();
        text.insert_str(at, "/* ");
        let inc = c.layout_job(&text, &syn, "Java", &theme, &font);
        let full = HlCache::default().layout_job(&text, &syn, "Java", &theme, &font);
        assert_eq!(inc.sections, full.sections);
        // Close it again after a complete parse: re-convergence path.
        text = text.replacen("/* fn f5(", "fn f5(", 1);
        let inc = c.layout_job(&text, &syn, "Java", &theme, &font);
        let full = HlCache::default().layout_job(&text, &syn, "Java", &theme, &font);
        assert_eq!(inc.sections, full.sections);
    }

    #[test]
    fn theme_change_recolours_without_reparsing() {
        let (mut syn, theme, font) = setup();
        let text = big(2_000);
        let mut c = HlCache::default();
        let a = c.layout_job(&text, &syn, "Java", &theme, &font);
        let parsed_ptr = c.syntect().lines.as_ptr();
        let other = crate::theme::by_name("gruvbox");
        syn.set_theme(&other);
        let b = c.layout_job(&text, &syn, "Java", &other, &font);
        assert_ne!(a.sections, b.sections);
        assert_eq!(parsed_ptr, c.syntect().lines.as_ptr(), "theme change must not re-parse");
        assert_eq!(b.sections, HlCache::default().layout_job(&text, &syn, "Java", &other, &font).sections);
    }

    #[test]
    fn tree_sitter_window_matches_a_full_highlight_and_parsing_is_sliced() {
        let (syn, theme, font) = setup();
        let text = big(20_000);
        let full = HlCache::default().layout_job(&text, &syn, "Rust", &theme, &font);
        let mut c = HlCache::default();
        let mut job = c.layout_job_window(&text, &syn, "Rust", &theme, &font, Some((0, 40)));
        // A big file is parsed in slices across frames; the text shows plain meanwhile.
        for _ in 0..10_000 {
            if !c.needs_more() {
                break;
            }
            job = c.layout_job_window(&text, &syn, "Rust", &theme, &font, Some((0, 40)));
        }
        assert!(c.tree().parsed());
        let r = line_bytes(&text, 0, 200);
        assert_eq!(in_range(&job, r.clone()), in_range(&full, r));
        assert!(job.sections.len() < 8_000, "window job has {} sections", job.sections.len());
        let job = c.layout_job_window(&text, &syn, "Rust", &theme, &font, Some((15_000, 15_040)));
        let r = line_bytes(&text, 14_950, 15_100);
        assert_eq!(in_range(&job, r.clone()), in_range(&full, r));
    }

    #[test]
    fn tree_sitter_edits_are_incremental_and_correct() {
        let (syn, theme, font) = setup();
        let mut text = big(3_000);
        let mut c = HlCache::default();
        c.layout_job(&text, &syn, "Rust", &theme, &font);
        assert!(c.tree().parsed());
        // Open a comment that changes everything after it, then close it again.
        let at = text.find("fn f5(").unwrap();
        text.insert_str(at, "/* ");
        let inc = c.layout_job(&text, &syn, "Rust", &theme, &font);
        let full = HlCache::default().layout_job(&text, &syn, "Rust", &theme, &font);
        assert_eq!(inc.sections, full.sections);
        text = text.replacen("/* fn f5(", "fn f5(", 1);
        let inc = c.layout_job(&text, &syn, "Rust", &theme, &font);
        let full = HlCache::default().layout_job(&text, &syn, "Rust", &theme, &font);
        assert_eq!(inc.sections, full.sections);
        // Theme change: new colours, same parse.
        let other = crate::theme::by_name("gruvbox");
        let mut syn2 = Syntaxes::new(&theme);
        syn2.set_theme(&other);
        let b = c.layout_job(&text, &syn2, "Rust", &other, &font);
        assert_ne!(b.sections, inc.sections);
        assert_eq!(b.sections, HlCache::default().layout_job(&text, &syn2, "Rust", &other, &font).sections);
    }

    #[test]
    fn tree_sitter_colours_by_syntax_not_by_regex() {
        let (syn, theme, font) = setup();
        let colour_of = |lang: &str, text: &str, needle: &str| {
            let job = HlCache::default().layout_job(text, &syn, lang, &theme, &font);
            let at = text.find(needle).unwrap();
            job.sections.iter().find(|s| s.byte_range.start.0 <= at && at < s.byte_range.end.0).map(|s| s.format.color).unwrap()
        };
        let rust = "fn main() { let s = \"x\"; foo(s); } // c\nstruct T;\n";
        assert_eq!(colour_of("Rust", rust, "fn"), theme.purple);
        assert_eq!(colour_of("Rust", rust, "\"x\""), theme.green);
        assert_eq!(colour_of("Rust", rust, "// c"), theme.comment);
        assert_eq!(colour_of("Rust", rust, "main"), theme.blue);
        assert_eq!(colour_of("Rust", rust, "T;"), theme.yellow);
        // A keyword inside a string is a string; a call is a function even without a regex hint.
        let ts = "const fn2 = 'let x';\nfoo.bar(1);\nclass A {}\n";
        assert_eq!(colour_of("TypeScript", ts, "let x"), theme.green);
        assert_eq!(colour_of("TypeScript", ts, "bar"), theme.blue);
        assert_eq!(colour_of("TypeScript", ts, "A {}"), theme.yellow);
        assert_eq!(colour_of("TypeScript", ts, "const"), theme.purple);
        assert_eq!(colour_of("TypeScript", "let v: string;", "string"), theme.yellow);
        let py = "def f(a):\n    return a  # note\n";
        assert_eq!(colour_of("Python", py, "def"), theme.purple);
        assert_eq!(colour_of("Python", py, "# note"), theme.comment);
        // Languages without a bundled grammar still get syntect.
        let java = "public class A { String s = \"q\"; }";
        assert_eq!(colour_of("Java", java, "\"q\""), theme.green);
    }

    #[test]
    fn very_long_lines_are_not_parsed() {
        let (syn, theme, font) = setup();
        let text = format!("fn a() {{}}\nlet x = \"{}\";\nfn b() {{}}\n", "y".repeat(50_000));
        let mut c = HlCache::default();
        let job = c.layout_job(&text, &syn, "Java", &theme, &font);
        assert!(c.syntect().lines[1].ops.is_empty());
        let end = job.sections.last().unwrap().byte_range.end.0;
        assert_eq!(end, text.len());
    }
}

#[cfg(test)]
#[path = "validate/highlight_tests.rs"]
mod validate_tests;
