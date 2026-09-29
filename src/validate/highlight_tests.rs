//! Validation: the incremental highlight cache must always produce exactly what a
//! from-scratch highlight produces. Included from highlight.rs as a child module.

use super::*;
use std::time::Instant;

fn setup() -> (Syntaxes, Theme, FontId) {
    let theme = crate::theme::by_name("tokyonight");
    (Syntaxes::new(&theme), theme, FontId::monospace(14.0))
}

fn fresh(text: &str, syn: &Syntaxes, syntax: &str, theme: &Theme, font: &FontId) -> LayoutJob {
    HlCache::default().layout_job(text, syn, syntax, theme, font)
}

/// Apply `edits` one after another to one cache, checking each result against a fresh highlight.
fn check_sequence(initial: &str, syntax: &str, edits: &[&dyn Fn(&str) -> String]) {
    let (syn, theme, font) = setup();
    let mut cache = HlCache::default();
    let mut text = initial.to_string();
    let a = cache.layout_job(&text, &syn, syntax, &theme, &font);
    assert_eq!(a, fresh(&text, &syn, syntax, &theme, &font));
    for (i, e) in edits.iter().enumerate() {
        text = e(&text);
        let inc = cache.layout_job(&text, &syn, syntax, &theme, &font);
        let full = fresh(&text, &syn, syntax, &theme, &font);
        assert_eq!(inc.text, full.text, "edit {i}");
        if inc.sections != full.sections {
            let first = inc.sections.iter().zip(&full.sections).position(|(a, b)| a != b);
            panic!("edit {i}: incremental highlight differs from scratch at section {first:?} (inc {} vs full {} sections)", inc.sections.len(), full.sections.len());
        }
    }
}

const RUST: &str = r#"//! module doc
use std::io;

/// A struct
pub struct A {
    x: u32, // field
}

fn main() {
    let s = "string with // not a comment";
    let r = r"raw";
    /* block
       comment */
    println!("{}", s);
}

fn tail() -> i32 { 42 }
"#;

fn insert_at_line(t: &str, line: usize, ins: &str) -> String {
    let mut lines: Vec<&str> = t.split_inclusive('\n').collect();
    let owned = ins.to_string();
    let idx = line.min(lines.len());
    lines.insert(idx, &owned);
    lines.concat()
}

fn delete_line(t: &str, line: usize) -> String {
    let mut lines: Vec<&str> = t.split_inclusive('\n').collect();
    if line < lines.len() {
        lines.remove(line);
    }
    lines.concat()
}

fn replace_first(t: &str, a: &str, b: &str) -> String {
    t.replacen(a, b, 1)
}

#[test]
fn edit_in_the_middle() {
    check_sequence(RUST, "Rust", &[
        &|t| replace_first(t, "let s", "let mut s"),
        &|t| replace_first(t, "42", "43"),
        &|t| replace_first(t, "x: u32", "x: u64"),
    ]);
}

#[test]
fn edits_that_change_parser_state_downstream() {
    check_sequence(RUST, "Rust", &[
        // Open a block comment in the middle: everything below becomes comment.
        &|t| insert_at_line(t, 9, "/*\n"),
        // Close it again.
        &|t| delete_line(t, 9),
        // Open a string that swallows lines.
        &|t| replace_first(t, "let r = r\"raw\";", "let r = \"raw;"),
        &|t| replace_first(t, "let r = \"raw;", "let r = r\"raw\";"),
        // Break the existing block comment's start.
        &|t| replace_first(t, "/* block", "/ block"),
        &|t| replace_first(t, "/ block", "/* block"),
    ]);
}

#[test]
fn append_and_delete_lines() {
    check_sequence(RUST, "Rust", &[
        &|t| format!("{t}fn more() {{}}\n"),
        &|t| format!("{t}// trailing"),
        &|t| format!("{t} more"),
        &|t| format!("{t}\n"),
        &|t| delete_line(t, 0),
        &|t| delete_line(t, 0),
        &|t| { let n = t.split_inclusive('\n').count(); delete_line(t, n - 1) },
        &|t| { let n = t.split_inclusive('\n').count(); delete_line(t, n / 2) },
        &|t| t[..t.len() / 2].to_string(),
        &|_| String::new(),
        &|_| "fn a() {}".to_string(),
        &|t| format!("/* {t}"),
    ]);
}

#[test]
fn identical_lines_prefix_suffix_overlap() {
    // Repeated identical lines make the prefix/suffix scan ambiguous.
    let base = "a\na\na\na\n";
    check_sequence(base, "Rust", &[
        &|t| insert_at_line(t, 2, "a\n"),
        &|t| delete_line(t, 1),
        &|t| insert_at_line(t, 2, "/* a\n"),
        &|t| insert_at_line(t, 0, "*/\n"),
        &|t| delete_line(t, 3),
    ]);
}

#[test]
fn markdown_and_python_with_embedded_state() {
    let md = "# T\n\n```rust\nfn a() {}\n```\n\ntext *em*\n";
    check_sequence(md, "Markdown", &[
        &|t| replace_first(t, "```rust\n", "```rust\nlet x = 1;\n"),
        &|t| delete_line(t, 2), // remove the fence opener
        &|t| insert_at_line(t, 2, "```python\n"),
    ]);
    let py = "def f():\n    '''doc\n    more'''\n    return 1\n";
    check_sequence(py, "Python", &[
        &|t| replace_first(t, "'''doc", "doc"),
        &|t| replace_first(t, "    doc", "    '''doc"),
    ]);
}

#[test]
fn pseudo_random_edits_match_scratch() {
    // Deterministic LCG so the sequence is reproducible.
    let mut seed: u64 = 0x1234_5678;
    let mut rnd = move |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % n.max(1)
    };
    let pieces = ["/*", "*/", "\"", "\n", "//", "fn x() {", "}", "r#\"", "\"#", "'a'", "é😀", " ", "\r\n"];
    let (syn, theme, font) = setup();
    let mut cache = HlCache::default();
    let mut text: String = RUST.repeat(3);
    for step in 0..300 {
        let chars: Vec<char> = text.chars().collect();
        let pos = rnd(chars.len() + 1);
        if rnd(3) == 0 && !chars.is_empty() {
            let end = (pos + rnd(12)).min(chars.len());
            text = chars[..pos].iter().chain(&chars[end..]).collect();
        } else {
            let p = pieces[rnd(pieces.len())];
            text = chars[..pos].iter().collect::<String>() + p + &chars[pos..].iter().collect::<String>();
        }
        let inc = cache.layout_job(&text, &syn, "Rust", &theme, &font);
        let full = fresh(&text, &syn, "Rust", &theme, &font);
        assert!(inc == full, "step {step}: incremental != scratch for text {text:?}");
    }
}

#[test]
fn theme_change_and_font_change_invalidate() {
    let (mut syn, theme, font) = setup();
    let mut cache = HlCache::default();
    let a = cache.layout_job(RUST, &syn, "Rust", &theme, &font);
    let other = crate::theme::by_name("gruvbox-light");
    syn.set_theme(&other);
    let b = cache.layout_job(RUST, &syn, "Rust", &other, &font);
    assert_ne!(a.sections, b.sections, "theme change must recolor");
    assert_eq!(b, fresh(RUST, &syn, "Rust", &other, &font));
    let big = FontId::monospace(20.0);
    let c = cache.layout_job(RUST, &syn, "Rust", &other, &big);
    assert!(c.sections.iter().all(|s| s.format.font_id.size == 20.0));
}

#[test]
fn sections_cover_the_text_exactly() {
    let (syn, theme, font) = setup();
    for text in ["", "\n", "a", "😀\r\n漢字", RUST] {
        let job = fresh(text, &syn, "Rust", &theme, &font);
        let mut pos = 0;
        for s in &job.sections {
            assert_eq!(s.byte_range.start.0, pos, "gap in sections for {text:?}");
            assert!(text.is_char_boundary(s.byte_range.end.0));
            pos = s.byte_range.end.0;
        }
        assert_eq!(pos, text.len());
    }
}

#[test]
fn timing_breakdown_20k_lines() {
    let (syn, theme, font) = setup();
    let base = RUST.repeat(20_000 / RUST.lines().count());
    let mut cache = HlCache::default();
    let t = Instant::now();
    cache.layout_job(&base, &syn, "Rust", &theme, &font);
    let first = t.elapsed();
    let mid = base.len() / 2;
    let mid = (mid..).find(|i| base.is_char_boundary(*i)).unwrap();
    let edited = format!("{}x{}", &base[..mid], &base[mid..]);
    let t = Instant::now();
    cache.layout_job(&edited, &syn, "Rust", &theme, &font);
    let inc = t.elapsed();
    let top = format!("/*{edited}");
    let t = Instant::now();
    cache.layout_job(&top, &syn, "Rust", &theme, &font);
    let top_t = t.elapsed();
    // egui text layout of the whole job, which the editor does on every change.
    let ctx = egui::Context::default();
    let mut lay = std::time::Duration::ZERO;
    let mut out = ctx.run_ui(Default::default(), |ui| {
        let job = cache.layout_job(&top, &syn, "Rust", &theme, &font);
        let t = Instant::now();
        ui.ctx().fonts_mut(|f| f.layout_job(job));
        lay = t.elapsed();
    });
    out.textures_delta.clear();
    eprintln!("highlight {} lines: initial {first:?}, 1-char edit mid-file {inc:?}, '/*' at top {top_t:?}, egui galley layout {lay:?}", base.lines().count());
}
