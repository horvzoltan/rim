//! Workspace file listing (quick open) and text search, honoring .gitignore.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) fn walker(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(false)
        // Honor .gitignore even in folders that aren't git repositories (yet).
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
}

pub fn list_files(root: &Path, limit: usize) -> Vec<String> {
    walker(root)
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| e.path().strip_prefix(root).ok().map(|p| p.to_string_lossy().to_string()))
        .take(limit)
        .collect()
}

pub struct Match {
    pub line: usize,
    /// Char column and length of the match within the line.
    pub col: usize,
    pub len: usize,
    /// Up to 200 chars of the line, starting after the leading whitespace (or 60 chars before a
    /// match far to the right), and the match's char column in it.
    pub text: String,
    pub text_col: usize,
}

pub struct FileResult {
    pub path: PathBuf,
    pub rel: String,
    pub matches: Vec<Match>,
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Options {
    pub case_sensitive: bool,
    /// Only matches not surrounded by letters, digits or `_`.
    pub whole_word: bool,
}

/// First match in `line` as (char column, char length).
fn find_in_line(line: &str, query: &str, qlen: usize, opts: Options) -> Option<(usize, usize)> {
    if !opts.whole_word {
        return if opts.case_sensitive {
            line.find(query).map(|b| (line[..b].chars().count(), qlen))
        } else {
            crate::text::find_ci(line, query, 1).first().map(|&(a, b)| (a, b - a))
        };
    }
    let hits: Vec<(usize, usize)> = if opts.case_sensitive {
        line.match_indices(query).map(|(b, _)| (line[..b].chars().count(), qlen)).collect()
    } else {
        crate::text::find_ci(line, query, usize::MAX).into_iter().map(|(a, b)| (a, b - a)).collect()
    };
    if hits.is_empty() {
        return None;
    }
    let chars: Vec<char> = line.chars().collect();
    hits.into_iter().find(|&(col, len)| {
        (col == 0 || !crate::text::is_word_char(chars[col - 1])) && chars.get(col + len).is_none_or(|&c| !crate::text::is_word_char(c))
    })
}

/// Returns results and whether the result limit was hit.
pub fn search(root: &Path, query: &str, opts: Options, limit: usize) -> (Vec<FileResult>, bool) {
    let mut out = Vec::new();
    let mut total = 0;
    if query.is_empty() {
        return (out, false);
    }
    let qlen = query.chars().count();
    let ascii_needle = query.to_ascii_lowercase();
    for entry in walker(root).flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
        let mut matches = Vec::new();
        // Cheap whole-file rejection before the per-line work.
        let may_match = if opts.case_sensitive {
            content.contains(query)
        } else if content.is_ascii() {
            content.to_ascii_lowercase().contains(&ascii_needle)
        } else {
            true
        };
        if !may_match {
            continue;
        }
        for (i, line) in content.lines().enumerate() {
            if let Some((col, len)) = find_in_line(line, query, qlen, opts) {
                let lead = line.chars().take_while(|c| c.is_whitespace()).count();
                // A window of the line around the match, so hits far to the right stay visible.
                let start = if col > lead + 140 { col - 60 } else { lead };
                let text: String = line.chars().skip(start).take(200).collect();
                matches.push(Match { line: i, col, len, text, text_col: col - start });
                total += 1;
                if total >= limit {
                    break;
                }
            }
        }
        if !matches.is_empty() {
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path()).to_string_lossy().to_string();
            out.push(FileResult { path: entry.path().to_path_buf(), rel, matches });
        }
        if total >= limit {
            return (out, true);
        }
    }
    (out, false)
}

#[cfg(test)]
#[path = "validate/search_tests.rs"]
mod validate_tests;
