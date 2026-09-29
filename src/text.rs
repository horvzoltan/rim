//! Text helpers: char/byte conversion, line indexing, LSP (UTF-16) positions.

use std::hash::{Hash, Hasher};

pub fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Line start offsets in both bytes and chars, for fast position conversion.
#[derive(Default, Clone)]
pub struct LineIndex {
    pub hash: u64,
    pub byte_starts: Vec<usize>,
    pub char_starts: Vec<usize>,
    pub total_chars: usize,
}

impl LineIndex {
    pub fn new(s: &str) -> Self {
        let mut byte_starts = vec![0];
        let mut char_starts = vec![0];
        let mut ci = 0;
        for (bi, c) in s.char_indices() {
            ci += 1;
            if c == '\n' {
                byte_starts.push(bi + 1);
                char_starts.push(ci);
            }
        }
        Self { hash: hash_str(s), byte_starts, char_starts, total_chars: ci }
    }

    pub fn line_count(&self) -> usize {
        self.char_starts.len()
    }

    pub fn line_of_char(&self, ci: usize) -> usize {
        match self.char_starts.binary_search(&ci) {
            Ok(l) => l,
            Err(l) => l - 1,
        }
    }

    /// Text of the line (without the trailing newline).
    pub fn line_text<'a>(&self, s: &'a str, line: usize) -> &'a str {
        let Some(&start) = self.byte_starts.get(line) else { return "" };
        let end = self.byte_starts.get(line + 1).map(|e| e - 1).unwrap_or(s.len());
        &s[start..end.max(start)]
    }

    /// (line, char column) of a char index.
    pub fn line_col(&self, ci: usize) -> (usize, usize) {
        let l = self.line_of_char(ci);
        (l, ci - self.char_starts[l])
    }

    /// Char index → byte offset, scanning only within its line.
    pub fn char_to_byte(&self, s: &str, ci: usize) -> usize {
        if ci >= self.total_chars {
            return s.len();
        }
        let l = self.line_of_char(ci);
        let start = self.byte_starts[l];
        start + s[start..].char_indices().nth(ci - self.char_starts[l]).map(|(b, _)| b).unwrap_or(s.len() - start)
    }

    /// LSP position (line, UTF-16 column) → char index.
    pub fn lsp_to_char(&self, s: &str, line: usize, col16: usize) -> usize {
        if line >= self.line_count() {
            return self.total_chars;
        }
        let mut units = 0;
        let mut ci = self.char_starts[line];
        for c in self.line_text(s, line).chars() {
            if units >= col16 {
                break;
            }
            units += c.len_utf16();
            ci += 1;
        }
        ci
    }

    /// Char index → LSP position (line, UTF-16 column).
    pub fn char_to_lsp(&self, s: &str, ci: usize) -> (usize, usize) {
        let (line, col) = self.line_col(ci.min(self.total_chars));
        let col16 = self.line_text(s, line).chars().take(col).map(|c| c.len_utf16()).sum();
        (line, col16)
    }
}

/// Case-insensitive search over the original text, returning char ranges.
/// Works when lowercasing changes a character's length (e.g. 'İ'), unlike
/// searching a lowercased copy with byte offsets.
pub fn find_ci(hay: &str, needle: &str, limit: usize) -> Vec<(usize, usize)> {
    let pat: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if pat.is_empty() {
        return vec![];
    }
    // Folded chars, each tagged with the index of the original char it came from.
    let mut folded: Vec<(char, usize)> = Vec::with_capacity(hay.len());
    for (ci, c) in hay.chars().enumerate() {
        if c.is_ascii() {
            folded.push((c.to_ascii_lowercase(), ci));
        } else {
            folded.extend(c.to_lowercase().map(|l| (l, ci)));
        }
    }
    let mut out = Vec::new();
    let n = pat.len();
    let mut i = 0;
    while i + n <= folded.len() {
        let starts_char = i == 0 || folded[i - 1].1 != folded[i].1;
        let ends_char = i + n == folded.len() || folded[i + n].1 != folded[i + n - 1].1;
        if starts_char && ends_char && folded[i..i + n].iter().zip(&pat).all(|(f, p)| f.0 == *p) {
            out.push((folded[i].1, folded[i + n - 1].1 + 1));
            if out.len() >= limit {
                break;
            }
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

/// Expand an LSP snippet into plain text. Returns the text and the char range of the
/// first tabstop (so the cursor can be placed there).
pub fn expand_snippet(snippet: &str) -> (String, Option<(usize, usize)>) {
    let chars: Vec<char> = snippet.chars().collect();
    let mut out = String::new();
    let mut stops: Vec<(u32, usize, usize)> = Vec::new();
    parse_snippet(&chars, &mut 0, &mut out, &mut stops, false);
    let pick = stops
        .iter()
        .filter(|s| s.0 > 0)
        .min_by_key(|s| s.0)
        .or_else(|| stops.iter().find(|s| s.0 == 0));
    (out, pick.map(|s| (s.1, s.2)))
}

fn parse_snippet(c: &[char], i: &mut usize, out: &mut String, stops: &mut Vec<(u32, usize, usize)>, nested: bool) {
    let len = |s: &String| s.chars().count();
    while *i < c.len() {
        match c[*i] {
            '\\' if *i + 1 < c.len() => {
                out.push(c[*i + 1]);
                *i += 2;
            }
            '}' if nested => return,
            '$' if *i + 1 < c.len() && c[*i + 1].is_ascii_digit() => {
                *i += 1;
                let mut n = 0u32;
                while *i < c.len() && c[*i].is_ascii_digit() {
                    n = n * 10 + c[*i].to_digit(10).unwrap();
                    *i += 1;
                }
                let p = len(out);
                stops.push((n, p, p));
            }
            '$' if *i + 1 < c.len() && c[*i + 1] == '{' => {
                *i += 2;
                let mut n = 0u32;
                let mut is_num = false;
                while *i < c.len() && c[*i].is_ascii_digit() {
                    n = n * 10 + c[*i].to_digit(10).unwrap();
                    is_num = true;
                    *i += 1;
                }
                if !is_num {
                    // Variable like ${TM_FILENAME:default}: skip name, keep default.
                    while *i < c.len() && (c[*i].is_alphanumeric() || c[*i] == '_') {
                        *i += 1;
                    }
                }
                let start = len(out);
                if *i < c.len() && c[*i] == ':' {
                    *i += 1;
                    parse_snippet(c, i, out, stops, true);
                } else if *i < c.len() && c[*i] == '|' {
                    // Choice: take the first option.
                    *i += 1;
                    let mut first = true;
                    while *i < c.len() && c[*i] != '|' {
                        if c[*i] == ',' {
                            first = false;
                        } else if first {
                            out.push(c[*i]);
                        }
                        *i += 1;
                    }
                    *i += 1;
                }
                if *i < c.len() && c[*i] == '}' {
                    *i += 1;
                }
                if is_num {
                    stops.push((n, start, len(out)));
                }
            }
            ch => {
                out.push(ch);
                *i += 1;
            }
        }
    }
}

/// A prepared fuzzy query (lowercased, whitespace removed).
pub struct FuzzyQuery {
    chars: Vec<char>,
}

impl FuzzyQuery {
    pub fn new(query: &str) -> Self {
        Self { chars: query.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase()).collect() }
    }
}

/// A candidate prepared once (lowercased chars), so scoring allocates nothing.
pub struct FuzzyTarget {
    lower: Vec<char>,
    /// Char index where the file name starts (after the last '/').
    name_start: usize,
}

impl FuzzyTarget {
    pub fn new(cand: &str) -> Self {
        let lower: Vec<char> = cand.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
        let name_start = lower.iter().rposition(|c| *c == '/').map(|i| i + 1).unwrap_or(0);
        Self { lower, name_start }
    }

    /// Subsequence fuzzy match. Higher is better.
    pub fn score(&self, q: &FuzzyQuery) -> Option<i64> {
        let q = &q.chars;
        if q.is_empty() {
            return Some(0);
        }
        let cl = &self.lower;
        let mut qi = 0;
        let mut score: i64 = 0;
        let mut last: Option<usize> = None;
        for (i, c) in cl.iter().enumerate() {
            if qi < q.len() && *c == q[qi] {
                score += 10;
                if last.is_some_and(|l| l + 1 == i) {
                    score += 15;
                }
                if i == 0 || matches!(cl[i - 1], '/' | '_' | '-' | '.' | ' ' | ':') {
                    score += 10;
                }
                last = Some(i);
                qi += 1;
            }
        }
        if qi < q.len() {
            return None;
        }
        let name = &cl[self.name_start..];
        if name.starts_with(q) {
            score += 40;
        } else if name.windows(q.len()).any(|w| w == q.as_slice()) {
            score += 20;
        }
        Some(score - cl.len() as i64 / 3)
    }
}

/// Subsequence fuzzy match. Higher is better.
pub fn fuzzy_score(query: &str, cand: &str) -> Option<i64> {
    FuzzyTarget::new(cand).score(&FuzzyQuery::new(query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets() {
        assert_eq!(expand_snippet("println!($0)"), ("println!()".into(), Some((9, 9))));
        assert_eq!(expand_snippet("foo(${1:a}, ${2:b})$0"), ("foo(a, b)".into(), Some((4, 5))));
        assert_eq!(expand_snippet("\\$x"), ("$x".into(), None));
    }

    #[test]
    fn case_insensitive_find_handles_length_changing_lowercase() {
        assert_eq!(find_ci("İstanbul FOO foo", "foo", 10), vec![(9, 12), (13, 16)]);
        assert_eq!(find_ci("İİxK", "x", 10), vec![(2, 3)]);
        assert_eq!(find_ci("aaa", "aa", 10), vec![(0, 2)]);
        assert_eq!(find_ci("abc", "", 10), vec![]);
        // 'İ' folds to "i̇"; a plain "i" must not match half of it.
        assert_eq!(find_ci("İ", "i", 10), vec![]);
    }

    #[test]
    fn line_index_char_to_byte() {
        let s = "aé\nx😀y\n";
        let idx = LineIndex::new(s);
        for ci in 0..=s.chars().count() + 2 {
            assert_eq!(idx.char_to_byte(s, ci), char_to_byte(s, ci), "ci={ci}");
        }
    }

    #[test]
    fn lsp_positions() {
        let s = "a😀b\nxyz";
        let idx = LineIndex::new(s);
        assert_eq!(idx.lsp_to_char(s, 0, 3), 2);
        assert_eq!(idx.char_to_lsp(s, 2), (0, 3));
        assert_eq!(idx.lsp_to_char(s, 1, 1), 5);
        assert_eq!(idx.line_text(s, 0), "a😀b");
    }
}
