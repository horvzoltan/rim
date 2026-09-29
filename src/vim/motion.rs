//! Motions and text objects over a `Text`: words, brackets, searches, `iw`/`a(`/...

use super::text::Text;


#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Kind {
    Excl,
    Incl,
    Line,
}

#[derive(Clone, Copy)]
pub(super) struct Mv {
    pub(super) pos: usize,
    pub(super) kind: Kind,
    pub(super) vert: bool,
    pub(super) end: bool,
}

pub(super) fn mv(pos: usize, kind: Kind) -> Mv {
    Mv { pos, kind, vert: false, end: false }
}

pub(super) fn cls(c: char, big: bool) -> u8 {
    if matches!(c, '\n' | ' ' | '\t' | '\r') {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

pub(super) fn word_fwd(t: &Text, mut i: usize, big: bool) -> usize {
    let n = t.len();
    if i >= n {
        return n;
    }
    let c0 = cls(t.at(i), big);
    if c0 != 0 {
        while i < n && cls(t.at(i), big) == c0 {
            i += 1;
        }
    }
    while i < n && cls(t.at(i), big) == 0 {
        i += 1;
        if i < n && t.at(i) == '\n' && t.at(i - 1) == '\n' {
            break;
        }
    }
    i
}

pub(super) fn word_back(t: &Text, i: usize, big: bool) -> usize {
    if i == 0 {
        return 0;
    }
    let mut i = i - 1;
    while i > 0 && cls(t.at(i), big) == 0 {
        if t.at(i) == '\n' && t.at(i - 1) == '\n' {
            return i;
        }
        i -= 1;
    }
    let c = cls(t.at(i), big);
    if c == 0 {
        return i;
    }
    while i > 0 && cls(t.at(i - 1), big) == c {
        i -= 1;
    }
    i
}

pub(super) fn word_end(t: &Text, i: usize, big: bool) -> usize {
    let n = t.len();
    if n == 0 {
        return 0;
    }
    let mut i = i + 1;
    if i >= n {
        return n - 1;
    }
    while i < n && cls(t.at(i), big) == 0 {
        i += 1;
    }
    if i >= n {
        return n - 1;
    }
    let c = cls(t.at(i), big);
    while i + 1 < n && cls(t.at(i + 1), big) == c {
        i += 1;
    }
    i
}

pub(super) fn word_end_back(t: &Text, i: usize, big: bool) -> usize {
    if i == 0 {
        return 0;
    }
    let mut i = i;
    let c = cls(t.at(i), big);
    if c != 0 {
        while i > 0 && cls(t.at(i - 1), big) == c {
            i -= 1;
        }
    }
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && cls(t.at(i), big) == 0 {
        i -= 1;
    }
    i
}

pub(super) fn match_bracket(t: &Text, cur: usize) -> Option<usize> {
    let le = t.le(cur);
    let i = (cur..le).find(|&i| "()[]{}".contains(t.at(i)))?;
    let (open, close, fwd) = match t.at(i) {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        _ => ('{', '}', false),
    };
    let mut depth = 0;
    if fwd {
        for j in i + 1..t.len() {
            let c = t.at(j);
            if c == open {
                depth += 1;
            } else if c == close {
                if depth == 0 {
                    return Some(j);
                }
                depth -= 1;
            }
        }
    } else {
        for j in (0..i).rev() {
            let c = t.at(j);
            if c == close {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    return Some(j);
                }
                depth -= 1;
            }
        }
    }
    None
}

pub(super) fn find_pat(t: &Text, pat: &str, whole: bool, from: usize, forward: bool, n: usize) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    let len = t.len();
    if p.is_empty() || len == 0 {
        return None;
    }
    let ci = !pat.chars().any(|c| c.is_uppercase());
    let eq = |a: char, b: char| a == b || (ci && a.to_lowercase().eq(b.to_lowercase()));
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let hit = |i: usize| -> bool {
        if i + p.len() > len || !(0..p.len()).all(|k| eq(t.buf[i + k], p[k])) {
            return false;
        }
        !whole || ((i == 0 || !word(t.buf[i - 1])) && (i + p.len() >= len || !word(t.buf[i + p.len()])))
    };
    let mut pos = from;
    let mut found = None;
    for _ in 0..n.max(1) {
        let mut h = None;
        for d in 1..=len {
            let i = if forward { (pos + d) % len } else { (pos + len - d % len) % len };
            if hit(i) {
                h = Some(i);
                break;
            }
        }
        pos = h?;
        found = h;
    }
    found
}

pub(super) fn text_object(t: &Text, kind: char, obj: char) -> Option<(usize, usize, bool)> {
    let cur = t.cursor;
    let inner = kind == 'i';
    match obj {
        'w' | 'W' => {
            let big = obj == 'W';
            if t.at(cur) == '\n' || cur >= t.len() {
                // Empty line or end of buffer: an empty object (so `ciw` still enters insert mode).
                return Some((cur, cur, false));
            }
            let c = cls(t.at(cur), big);
            let (mut a, mut b) = (cur, cur + 1);
            while a > t.ls(cur) && cls(t.at(a - 1), big) == c {
                a -= 1;
            }
            while b < t.len() && cls(t.at(b), big) == c && t.at(b) != '\n' {
                b += 1;
            }
            if inner {
                return Some((a, b, false));
            }
            if c == 0 {
                // On blanks: blanks plus the following word.
                let cw = cls(t.at(b), big);
                if cw != 0 {
                    while b < t.len() && cls(t.at(b), big) == cw {
                        b += 1;
                    }
                }
                return Some((a, b, false));
            }
            let mut e = b;
            while matches!(t.at(e), ' ' | '\t') {
                e += 1;
            }
            if e > b {
                Some((a, e, false))
            } else {
                while a > t.ls(a) && matches!(t.at(a - 1), ' ' | '\t') {
                    a -= 1;
                }
                Some((a, b, false))
            }
        }
        '"' | '\'' | '`' => {
            let (ls, le) = (t.ls(cur), t.le(cur));
            let quotes: Vec<usize> = (ls..le).filter(|&i| t.at(i) == obj && (i == ls || t.at(i - 1) != '\\')).collect();
            let pair = quotes.chunks(2).filter(|c| c.len() == 2).find(|c| c[1] >= cur)?;
            let (a, b) = (pair[0], pair[1]);
            if inner {
                return Some((a + 1, b, false));
            }
            let mut e = b + 1;
            while matches!(t.at(e), ' ' | '\t') {
                e += 1;
            }
            let mut s = a;
            if e == b + 1 {
                while s > ls && matches!(t.at(s - 1), ' ' | '\t') {
                    s -= 1;
                }
            }
            Some((s, e, false))
        }
        '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>' => {
            let (open, close) = match obj {
                '(' | ')' | 'b' => ('(', ')'),
                '[' | ']' => ('[', ']'),
                '{' | '}' | 'B' => ('{', '}'),
                _ => ('<', '>'),
            };
            let open_idx = if t.at(cur) == open {
                cur
            } else {
                let mut depth = 0;
                let mut i = cur;
                loop {
                    if i == 0 {
                        return None;
                    }
                    i -= 1;
                    let c = t.at(i);
                    if c == close {
                        depth += 1;
                    } else if c == open {
                        if depth == 0 {
                            break i;
                        }
                        depth -= 1;
                    }
                }
            };
            let close_idx = {
                let mut depth = 0;
                let mut i = open_idx + 1;
                loop {
                    if i >= t.len() {
                        return None;
                    }
                    let c = t.at(i);
                    if c == open {
                        depth += 1;
                    } else if c == close {
                        if depth == 0 {
                            break i;
                        }
                        depth -= 1;
                    }
                    i += 1;
                }
            };
            if !inner {
                return Some((open_idx, close_idx + 1, false));
            }
            // A block whose braces sit on their own lines: take the inner lines whole.
            let close_ls = t.ls(close_idx);
            if t.at(open_idx + 1) == '\n' && t.buf[close_ls..close_idx].iter().all(|c| matches!(c, ' ' | '\t')) {
                let first = open_idx + 2;
                if first < close_ls {
                    return Some((first, close_ls - 1, true));
                }
                return Some((open_idx + 1, open_idx + 1, false));
            }
            Some((open_idx + 1, close_idx, false))
        }
        'p' => {
            let blank = |ls: usize| t.is_empty_line(ls);
            let cur_ls = t.ls(cur);
            let want = blank(cur_ls);
            let mut first = cur_ls;
            while let Some(p) = t.prev_line_start(first) {
                if blank(p) != want {
                    break;
                }
                first = p;
            }
            let mut last = cur_ls;
            while let Some(n) = t.next_line_start(last) {
                if blank(n) != want {
                    break;
                }
                last = n;
            }
            if !inner {
                // Include the following blank lines, or the preceding ones at the end.
                if let Some(mut n) = t.next_line_start(last).filter(|_| !want) {
                    last = n;
                    while let Some(m) = t.next_line_start(n) {
                        if !blank(m) {
                            break;
                        }
                        n = m;
                        last = m;
                    }
                }
            }
            Some((first, t.le(last), true))
        }
        _ => None,
    }
}
