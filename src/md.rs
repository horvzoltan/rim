//! A small markdown parser for chat messages: enough for what Claude writes
//! (headings, lists, fenced code, tables, quotes, inline code/bold/italic/links).
//! It is forgiving about unfinished input because text arrives token by token.

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Span>),
    Para(Vec<Span>),
    Code { lang: String, text: String },
    /// One list item; nested lists are flattened with an indent level.
    Item { indent: usize, marker: String, spans: Vec<Span> },
    Quote(Vec<Span>),
    Rule,
    Table { header: Vec<Vec<Span>>, rows: Vec<Vec<Vec<Span>>> },
}

pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && ["-", "*", "_"].iter().any(|c| t.chars().all(|x| x == c.chars().next().unwrap() || x == ' '))
        && t.chars().filter(|c| !c.is_whitespace()).count() >= 3
}

fn is_table_sep(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.contains('|') && t.chars().all(|c| matches!(c, '-' | ':' | '|' | ' '))
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// (indent level, marker, text) if the line starts a list item.
fn list_item(line: &str) -> Option<(usize, String, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    for m in ["- ", "* ", "+ "] {
        if let Some(t) = rest.strip_prefix(m) {
            return Some((indent / 2, "\u{2022}".to_string(), t));
        }
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits <= 9 {
        let after = &rest[digits..];
        if let Some(t) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return Some((indent / 2, format!("{}.", &rest[..digits]), t));
        }
    }
    None
}

pub fn parse(src: &str) -> Vec<Block> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut i = 0;

    fn flush(para: &mut Vec<&str>, out: &mut Vec<Block>) {
        if !para.is_empty() {
            out.push(Block::Para(inline(&para.join(" "))));
            para.clear();
        }
    }

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let lead = line.len() - trimmed.len();

        if lead < 4 && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            flush(&mut para, &mut out);
            let fence = &trimmed[..3];
            let lang = trimmed[3..].split_whitespace().next().unwrap_or("").to_string();
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
                // Strip the fence's own indentation from the content.
                body.push(lines[i].get(lead.min(lines[i].len() - lines[i].trim_start().len())..).unwrap_or(lines[i]));
                i += 1;
            }
            i += 1;
            out.push(Block::Code { lang, text: body.join("\n") });
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut para, &mut out);
            i += 1;
            continue;
        }
        if lead < 4 && trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
                flush(&mut para, &mut out);
                out.push(Block::Heading(level as u8, inline(trimmed[level..].trim().trim_end_matches('#').trim())));
                i += 1;
                continue;
            }
        }
        if is_rule(line) && para.is_empty() {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if line.contains('|') && i + 1 < lines.len() && is_table_sep(lines[i + 1]) && !is_table_sep(line) {
            flush(&mut para, &mut out);
            let header = split_row(line).iter().map(|c| inline(c)).collect();
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                rows.push(split_row(lines[i]).iter().map(|c| inline(c)).collect());
                i += 1;
            }
            out.push(Block::Table { header, rows });
            continue;
        }
        if let Some((indent, marker, text)) = list_item(line) {
            flush(&mut para, &mut out);
            let mut body = text.to_string();
            i += 1;
            // Continuation lines: indented, not a new item, not blank.
            while i < lines.len() && !lines[i].trim().is_empty() && list_item(lines[i]).is_none() && lines[i].starts_with("  ") && !lines[i].trim_start().starts_with("```") {
                body.push(' ');
                body.push_str(lines[i].trim());
                i += 1;
            }
            out.push(Block::Item { indent, marker, spans: inline(&body) });
            continue;
        }
        if let Some(q) = trimmed.strip_prefix('>') {
            flush(&mut para, &mut out);
            let mut body = q.trim().to_string();
            i += 1;
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                body.push(' ');
                body.push_str(lines[i].trim_start().trim_start_matches('>').trim());
                i += 1;
            }
            out.push(Block::Quote(inline(&body)));
            continue;
        }
        para.push(line.trim());
        i += 1;
    }
    flush(&mut para, &mut out);
    out
}

#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
}

pub fn inline(src: &str) -> Vec<Span> {
    let mut out = Vec::new();
    inline_into(src, Style::default(), None, &mut out);
    // Merge neighbours with the same look.
    let mut merged: Vec<Span> = Vec::new();
    for s in out {
        match merged.last_mut() {
            Some(l) if l.bold == s.bold && l.italic == s.italic && l.code == s.code && l.link == s.link => l.text.push_str(&s.text),
            _ => merged.push(s),
        }
    }
    merged
}

fn find_seq(chars: &[char], from: usize, pat: &[char]) -> Option<usize> {
    if pat.is_empty() || chars.len() < pat.len() {
        return None;
    }
    (from..=chars.len() - pat.len()).find(|&i| chars[i..i + pat.len()] == *pat)
}

fn inline_into(src: &str, st: Style, link: Option<&str>, out: &mut Vec<Span>) {
    let c: Vec<char> = src.chars().collect();
    let n = c.len();
    let mut buf = String::new();
    let mut i = 0;
    let mk = |text: String, code: bool| Span { text, bold: st.bold, italic: st.italic, code, link: link.map(String::from) };
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                out.push(mk(std::mem::take(&mut buf), false));
            }
        };
    }
    let word = |ch: char| ch.is_alphanumeric();
    while i < n {
        let ch = c[i];
        if ch == '\\' && i + 1 < n && c[i + 1].is_ascii_punctuation() {
            buf.push(c[i + 1]);
            i += 2;
            continue;
        }
        if ch == '`' {
            let run = c[i..].iter().take_while(|x| **x == '`').count();
            let fence = vec!['`'; run];
            if let Some(end) = find_seq(&c, i + run, &fence) {
                flush!();
                let inner: String = c[i + run..end].iter().collect();
                let inner = if inner.len() > 1 && inner.starts_with(' ') && inner.ends_with(' ') { inner[1..inner.len() - 1].to_string() } else { inner };
                out.push(mk(inner, true));
                i = end + run;
                continue;
            }
        }
        if (ch == '*' || ch == '_') && i + 1 < n && c[i + 1] == ch {
            let boundary_ok = ch == '*' || i == 0 || !word(c[i - 1]);
            if boundary_ok && i + 2 < n && !c[i + 2].is_whitespace()
                && let Some(end) = find_seq(&c, i + 2, &[ch, ch])
                    && end > i + 2 && !c[end - 1].is_whitespace() && (ch == '*' || end + 2 >= n || !word(c[end + 2])) {
                        flush!();
                        let inner: String = c[i + 2..end].iter().collect();
                        inline_into(&inner, Style { bold: true, ..st }, link, out);
                        i = end + 2;
                        continue;
                    }
        }
        if ch == '*' || ch == '_' {
            let boundary_ok = ch == '*' || i == 0 || !word(c[i - 1]);
            if boundary_ok && i + 1 < n && !c[i + 1].is_whitespace() && c[i + 1] != ch {
                let end = (i + 1..n).find(|&j| c[j] == ch && !c[j - 1].is_whitespace() && (ch == '*' || j + 1 >= n || !word(c[j + 1])) && (j + 1 >= n || c[j + 1] != ch));
                if let Some(end) = end {
                    flush!();
                    let inner: String = c[i + 1..end].iter().collect();
                    inline_into(&inner, Style { italic: true, ..st }, link, out);
                    i = end + 1;
                    continue;
                }
            }
        }
        if ch == '['
            && let Some(mid) = find_seq(&c, i + 1, &[']', '('])
                && let Some(end) = (mid + 2..n).find(|&j| c[j] == ')') {
                    flush!();
                    let label: String = c[i + 1..mid].iter().collect();
                    let url: String = c[mid + 2..end].iter().collect();
                    inline_into(&label, st, Some(url.trim()), out);
                    i = end + 1;
                    continue;
                }
        buf.push(ch);
        i += 1;
    }
    flush!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Span {
        Span { text: text.into(), ..Default::default() }
    }

    #[test]
    fn inline_styles() {
        let spans = inline("a **b** *c* `d` [e](http://x)");
        assert_eq!(spans.len(), 8);
        assert!(spans[1].bold && spans[1].text == "b");
        assert!(spans[3].italic && spans[3].text == "c");
        assert!(spans[5].code && spans[5].text == "d");
        assert_eq!(spans[7].link.as_deref(), Some("http://x"));
        assert_eq!(plain(&spans), "a b c d e");
    }

    #[test]
    fn snake_case_and_globs_are_not_emphasis() {
        assert_eq!(inline("snake_case_name and __init__"), vec![Span { text: "snake_case_name and ".into(), ..Default::default() }, Span { text: "init".into(), bold: true, ..Default::default() }]);
        assert_eq!(inline("2 * 3 * 4"), vec![s("2 * 3 * 4")]);
        assert_eq!(inline("unclosed **bold"), vec![s("unclosed **bold")]);
    }

    #[test]
    fn nested_and_escaped() {
        let spans = inline("**bold `code`** \\*not\\*");
        assert!(spans[0].bold && !spans[0].code);
        assert!(spans[1].bold && spans[1].code);
        assert_eq!(spans[2].text, " *not*");
    }

    #[test]
    fn blocks() {
        let md = "# Title\n\nSome *text*\nmore text\n\n- one\n  - nested\n1. first\n\n```rust\nfn main() {}\n```\n> quote\n\n---\n";
        let b = parse(md);
        assert!(matches!(&b[0], Block::Heading(1, _)));
        assert!(matches!(&b[1], Block::Para(p) if plain(p) == "Some text more text"));
        assert!(matches!(&b[2], Block::Item { indent: 0, marker, .. } if marker == "\u{2022}"));
        assert!(matches!(&b[3], Block::Item { indent: 1, .. }));
        assert!(matches!(&b[4], Block::Item { marker, .. } if marker == "1."));
        assert!(matches!(&b[5], Block::Code { lang, text } if lang == "rust" && text == "fn main() {}"));
        assert!(matches!(&b[6], Block::Quote(_)));
        assert_eq!(b[7], Block::Rule);
    }

    #[test]
    fn unfinished_code_fence_shows_what_has_arrived() {
        let b = parse("text\n```py\nprint(1)\nprint(");
        assert!(matches!(&b[1], Block::Code { text, .. } if text == "print(1)\nprint("));
    }

    #[test]
    fn tables() {
        let b = parse("| a | b |\n|---|:-:|\n| 1 | **2** |\n| 3 | 4 |\n\nafter");
        let Block::Table { header, rows } = &b[0] else { panic!("{b:?}") };
        assert_eq!(header.len(), 2);
        assert_eq!(rows.len(), 2);
        assert!(rows[0][1][0].bold);
        assert!(matches!(&b[1], Block::Para(_)));
    }

    #[test]
    fn list_continuation_lines_join_the_item() {
        let b = parse("- first line\n  continues here\n- second");
        assert!(matches!(&b[0], Block::Item { spans, .. } if plain(spans) == "first line continues here"));
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn horizontal_rule_is_not_a_list() {
        assert_eq!(parse("***"), vec![Block::Rule]);
        assert_eq!(parse("- - -"), vec![Block::Rule]);
    }
}
