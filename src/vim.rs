//! Vim-style modal editing: a practical subset (motions, operators, text objects,
//! visual modes, registers, dot-repeat, undo, `:w`/`:q`/`:N`, search).
//! Pure logic over a char buffer, independent of the UI so it can be unit tested.

mod motion;
mod parse;
mod text;

use motion::*;
use parse::*;
pub use text::Text;

const UNDO_MAX: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Esc,
    Enter,
    Backspace,
    Ctrl(char),
}

/// Things the editor must do on the vim engine's behalf.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Save,
    SaveClose,
    Close { force: bool },
    Copy(String),
}

#[derive(Clone, Default)]
struct Register {
    text: String,
    linewise: bool,
}

#[derive(Clone)]
struct Change {
    keys: Vec<char>,
    insert: Option<String>,
}

#[derive(Clone)]
struct Search {
    pat: String,
    forward: bool,
    whole: bool,
}

/// State shared by all documents: registers, last search, last change.
#[derive(Default)]
pub struct Shared {
    register: Register,
    last_find: Option<(char, char)>,
    last_search: Option<Search>,
    last_change: Option<Change>,
}

/// One undoable change: `removed` was replaced by `inserted` at `at`.
struct UndoOp {
    at: usize,
    removed: Vec<char>,
    inserted: Vec<char>,
    cursor: usize,
}

impl UndoOp {
    /// Only the changed middle is kept, so undo history stays small even for huge buffers.
    fn diff(before: &[char], after: &[char], cursor: usize) -> Option<UndoOp> {
        let p = before.iter().zip(after).take_while(|(a, b)| a == b).count();
        let max_s = before.len().min(after.len()) - p;
        let sfx = before.iter().rev().zip(after.iter().rev()).take(max_s).take_while(|(a, b)| a == b).count();
        let removed = before[p..before.len() - sfx].to_vec();
        let inserted = after[p..after.len() - sfx].to_vec();
        (!removed.is_empty() || !inserted.is_empty()).then_some(UndoOp { at: p, removed, inserted, cursor })
    }
}

struct Insert {
    start: usize,
    base_len: usize,
    record: bool,
    count: usize,
    /// Buffer before the command that entered insert mode; its undo step is recorded on Esc.
    before: Option<(Vec<char>, usize)>,
}

#[derive(Default)]
pub struct Vim {
    pub mode: Mode,
    pending: Vec<char>,
    anchor: usize,
    /// Cursor while in a visual mode (the editor's own cursor is a selection then).
    cur: usize,
    want_col: Option<usize>,
    undo: Vec<UndoOp>,
    redo: Vec<UndoOp>,
    /// Command line being typed: (':' | '/' | '?', text).
    pub cmdline: Option<(char, String)>,
    pub message: String,
    insert: Option<Insert>,
    replaying: bool,
}

impl Vim {
    pub fn label(&self) -> &'static str {
        match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        }
    }

    pub fn pending(&self) -> String {
        self.pending.iter().collect()
    }

    pub fn visual_cursor(&self) -> usize {
        self.cur
    }

    /// Selected char range (end exclusive) to show while in a visual mode.
    pub fn selection(&self, t: &Text) -> Option<(usize, usize)> {
        let (lo, hi) = (self.anchor.min(t.cursor), self.anchor.max(t.cursor));
        match self.mode {
            Mode::Visual => Some((lo, (hi + 1).min(t.len()))),
            Mode::VisualLine => Some((t.ls(lo), (t.le(hi) + 1).min(t.len()))),
            _ => None,
        }
    }

    pub fn feed(&mut self, sh: &mut Shared, key: Key, t: &mut Text, out: &mut Vec<Action>) {
        self.message.clear();
        let backup = t.read_only.then(|| (t.buf.clone(), t.cursor));
        if self.cmdline.is_some() {
            self.feed_cmdline(sh, key, t, out);
        } else {
            match self.mode {
                Mode::Insert => self.feed_insert(sh, key, t),
                Mode::Normal => self.feed_normal(sh, key, t, out),
                Mode::Visual | Mode::VisualLine => self.feed_visual(sh, key, t, out),
            }
        }
        if let Some((b, c)) = backup {
            if t.edits > 0 {
                t.buf = b;
                t.cursor = c;
                t.edits = 0;
                self.message = "Buffer is read-only".into();
            }
            if self.mode == Mode::Insert {
                self.mode = Mode::Normal;
                self.insert = None;
            }
        }
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            // Undo/redo can shrink the buffer under the selection.
            let last = t.len().saturating_sub(1);
            t.cursor = t.cursor.min(last);
            self.anchor = self.anchor.min(last);
            self.cur = t.cursor;
        }
    }

    // ---------- insert mode ----------

    fn enter_insert(&mut self, t: &Text) {
        self.enter_insert_n(t, 1);
    }

    fn enter_insert_n(&mut self, t: &Text, count: usize) {
        self.mode = Mode::Insert;
        self.insert = Some(Insert { start: t.cursor, base_len: t.len(), record: !self.replaying, count, before: None });
    }

    fn feed_insert(&mut self, sh: &mut Shared, key: Key, t: &mut Text) {
        match key {
            Key::Esc => self.leave_insert(sh, t),
            Key::Char(c) => {
                let cur = t.cursor;
                t.replace(cur, cur, &c.to_string());
                t.cursor += 1;
            }
            Key::Enter => {
                let cur = t.cursor;
                let ind: String = t.indent_of(t.ls(cur)).chars().take(cur - t.ls(cur)).collect();
                t.replace(cur, cur, &format!("\n{ind}"));
                t.cursor += 1 + ind.chars().count();
            }
            Key::Backspace => {
                if t.cursor > 0 {
                    let cur = t.cursor;
                    t.replace(cur - 1, cur, "");
                    t.cursor -= 1;
                }
            }
            Key::Ctrl(_) => {}
        }
    }

    fn leave_insert(&mut self, sh: &mut Shared, t: &mut Text) {
        if let Some(ins) = self.insert.take() {
            if ins.record {
                let grown = t.len().saturating_sub(ins.base_len);
                let end = ins.start + grown;
                let typed = if grown > 0 && t.cursor == end && end <= t.len() { t.slice(ins.start, end) } else { String::new() };
                if let Some(ch) = &mut sh.last_change {
                    ch.insert = Some(typed);
                }
            }
            // `3ix<Esc>` inserts the typed text three times.
            let grown = t.len().saturating_sub(ins.base_len);
            if ins.count > 1 && grown > 0 && t.cursor == ins.start + grown {
                let typed = t.slice(ins.start, ins.start + grown).repeat(ins.count - 1);
                let c = t.cursor;
                t.replace(c, c, &typed);
                t.cursor = c + typed.chars().count();
            }
            // One undo step for the command plus everything typed (none if nothing changed).
            if let Some((before, cursor)) = ins.before
                && let Some(op) = UndoOp::diff(&before, &t.buf, cursor)
            {
                self.push_op(op);
            }
        }
        self.mode = Mode::Normal;
        if t.cursor > t.ls(t.cursor) {
            t.cursor -= 1;
        }
        t.clamp();
        self.want_col = None;
    }

    // ---------- normal mode ----------

    fn feed_normal(&mut self, sh: &mut Shared, key: Key, t: &mut Text, out: &mut Vec<Action>) {
        t.clamp();
        let c = match key {
            Key::Esc => {
                self.pending.clear();
                return;
            }
            Key::Ctrl(c) => {
                self.pending.clear();
                self.ctrl(c, t);
                return;
            }
            Key::Backspace => 'h',
            Key::Enter => {
                if self.pending.last() == Some(&'r') { '\n' } else { '+' }
            }
            Key::Char(c) => c,
        };
        self.pending.push(c);
        match parse(&self.pending, false) {
            Parse::Incomplete => {}
            Parse::Invalid => self.pending.clear(),
            Parse::Cmd(cmd) => {
                let raw = std::mem::take(&mut self.pending);
                self.exec(sh, t, out, cmd, raw);
            }
        }
    }

    fn ctrl(&mut self, c: char, t: &mut Text) {
        let half = (t.page / 2).max(1);
        let full = t.page.saturating_sub(2).max(1);
        match c {
            'r' | 'y' => self.redo(t),
            'z' => self.undo(t, 1),
            'd' | 'u' | 'f' | 'b' => {
                let (n, down) = match c {
                    'd' => (half, true),
                    'u' => (half, false),
                    'f' => (full, true),
                    _ => (full, false),
                };
                let col0 = t.cursor - t.ls(t.cursor);
                if let Some(m) = self.vertical(t, n, down) {
                    t.cursor = m.pos;
                    self.want_col = Some(self.want_col.unwrap_or(col0));
                }
                t.clamp();
            }
            _ => {}
        }
    }

    fn push_op(&mut self, op: UndoOp) {
        self.undo.push(op);
        if self.undo.len() > UNDO_MAX {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Record the change a command made, given the buffer before it.
    fn push_undo(&mut self, t: &Text, (before, cursor): (Vec<char>, usize)) {
        if let Some(op) = UndoOp::diff(&before, &t.buf, cursor) {
            self.push_op(op);
        }
    }

    fn undo(&mut self, t: &mut Text, n: usize) {
        for _ in 0..n {
            let Some(op) = self.undo.pop() else {
                self.message = "Already at oldest change".into();
                break;
            };
            let end = (op.at + op.inserted.len()).min(t.buf.len());
            t.buf.splice(op.at.min(end)..end, op.removed.iter().copied());
            t.cursor = op.cursor;
            t.edits += 1;
            self.redo.push(op);
        }
        t.clamp();
    }

    fn redo(&mut self, t: &mut Text) {
        let Some(op) = self.redo.pop() else {
            self.message = "Already at newest change".into();
            return;
        };
        let end = (op.at + op.removed.len()).min(t.buf.len());
        t.buf.splice(op.at.min(end)..end, op.inserted.iter().copied());
        t.cursor = op.at;
        t.edits += 1;
        self.undo.push(op);
        t.clamp();
    }

    fn exec(&mut self, sh: &mut Shared, t: &mut Text, out: &mut Vec<Action>, cmd: Cmd, raw: Vec<char>) {
        let edits0 = t.edits;
        let before = cmd.modifies().then(|| (t.buf.clone(), t.cursor));
        let mut repeatable = false;
        let is_motion = matches!(cmd.kind, CmdKind::Motion(_));
        match cmd.kind {
            CmdKind::Motion(keys) => self.do_motion(sh, t, &keys, cmd.count),
            CmdKind::Op { op, count2, target } => {
                let count = match (cmd.count, count2) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(1) * b.unwrap_or(1)),
                };
                self.do_operator(sh, t, out, op, count, target);
                repeatable = op != 'y';
            }
            CmdKind::Simple(c, arg) => repeatable = self.do_simple(sh, t, out, c, arg, cmd.count),
            CmdKind::Obj(..) => {}
        }
        let changed = t.edits != edits0;
        if let Some(b) = before {
            if self.mode == Mode::Insert {
                // Recorded together with the typed text when insert mode ends.
                if let Some(ins) = self.insert.as_mut() {
                    ins.before = Some(b);
                }
            } else if changed {
                self.push_undo(t, b);
            }
        }
        if repeatable && !self.replaying && (changed || self.mode == Mode::Insert) {
            sh.last_change = Some(Change { keys: raw, insert: None });
        }
        if !is_motion {
            self.want_col = None;
        }
        if self.mode == Mode::Normal {
            t.clamp();
        }
    }

    fn do_motion(&mut self, sh: &mut Shared, t: &mut Text, keys: &[char], count: Option<usize>) {
        let col0 = t.cursor - t.ls(t.cursor);
        let Some(m) = self.motion(sh, t, keys, count, None) else { return };
        t.cursor = m.pos.min(t.len());
        t.clamp();
        if m.vert {
            self.want_col = Some(self.want_col.unwrap_or(col0));
        } else if m.end {
            self.want_col = Some(usize::MAX);
        } else {
            self.want_col = None;
        }
    }

    fn vertical(&self, t: &Text, n: usize, down: bool) -> Option<Mv> {
        let cur = t.cursor;
        let col = self.want_col.unwrap_or(cur - t.ls(cur));
        let mut ls = t.ls(cur);
        let mut moved = 0;
        for _ in 0..n {
            let next = if down { t.next_line_start(ls) } else { t.prev_line_start(ls) };
            match next {
                Some(x) => {
                    ls = x;
                    moved += 1;
                }
                None => break,
            }
        }
        if moved == 0 {
            return None;
        }
        let line_len = t.le(ls) - ls;
        Some(Mv { pos: ls + col.min(line_len.saturating_sub(1)), kind: Kind::Line, vert: true, end: false })
    }

    fn find_char(&self, t: &Text, k: char, ch: char, n: usize, repeat: bool) -> Option<Mv> {
        let cur = t.cursor;
        let (ls, le) = (t.ls(cur), t.le(cur));
        match k {
            'f' | 't' => {
                let from = if repeat && k == 't' { cur + 1 } else { cur };
                let mut p = from;
                for _ in 0..n {
                    p = (p + 1..le).find(|&i| t.at(i) == ch)?;
                }
                Some(mv(if k == 't' { p - 1 } else { p }, Kind::Incl))
            }
            _ => {
                let to = if repeat && k == 'T' { cur.saturating_sub(1) } else { cur };
                let mut p = to;
                for _ in 0..n {
                    p = (ls..p).rev().find(|&i| t.at(i) == ch)?;
                }
                Some(mv(if k == 'T' { p + 1 } else { p }, Kind::Excl))
            }
        }
    }

    fn search(&mut self, sh: &Shared, t: &Text, forward_flip: bool, n: usize) -> Option<Mv> {
        let Some(s) = sh.last_search.clone() else {
            self.message = "No previous search pattern".into();
            return None;
        };
        let fwd = s.forward != forward_flip;
        match find_pat(t, &s.pat, s.whole, t.cursor, fwd, n) {
            Some(p) => Some(mv(p, Kind::Excl)),
            None => {
                self.message = format!("Pattern not found: {}", s.pat);
                None
            }
        }
    }

    /// Evaluate a motion from the cursor. `op` is the pending operator, if any.
    fn motion(&mut self, sh: &mut Shared, t: &Text, keys: &[char], count: Option<usize>, op: Option<char>) -> Option<Mv> {
        let n = count.unwrap_or(1).max(1);
        let cur = t.cursor;
        let k = keys[0];
        Some(match k {
            'h' => mv(cur.saturating_sub(n).max(t.ls(cur)), Kind::Excl),
            'l' | ' ' => mv((cur + n).min(t.le(cur)), Kind::Excl),
            'j' => self.vertical(t, n, true)?,
            'k' => self.vertical(t, n, false)?,
            '0' => mv(t.ls(cur), Kind::Excl),
            '^' => mv(t.fnb(t.ls(cur)), Kind::Excl),
            '$' => {
                let mut ls = t.ls(cur);
                for _ in 1..n {
                    match t.next_line_start(ls) {
                        Some(x) => ls = x,
                        None => break,
                    }
                }
                Mv { pos: t.le(ls), kind: Kind::Excl, vert: false, end: true }
            }
            'w' | 'W' => {
                let big = k == 'W';
                let mut p = cur;
                for i in 0..n {
                    let mut np = word_fwd(t, p, big);
                    // An operator on the last word of a line must not swallow the newline.
                    if op.is_some() && i == n - 1 && t.buf[p.min(t.len())..np.min(t.len())].contains(&'\n') && !t.is_empty_line(t.ls(p)) {
                        np = t.le(p);
                    }
                    p = np;
                }
                mv(p, Kind::Excl)
            }
            'b' | 'B' => {
                let mut p = cur;
                for _ in 0..n {
                    p = word_back(t, p, k == 'B');
                }
                mv(p, Kind::Excl)
            }
            'e' | 'E' => {
                let mut p = cur;
                for _ in 0..n {
                    p = word_end(t, p, k == 'E');
                }
                mv(p, Kind::Incl)
            }
            'G' => {
                let ls = match count {
                    Some(c) => t.line_start_of(c - 1),
                    None => t.last_line_start(),
                };
                mv(t.fnb(ls), Kind::Line)
            }
            'g' => match keys[1] {
                'g' => mv(t.fnb(t.line_start_of(count.map(|c| c - 1).unwrap_or(0))), Kind::Line),
                e => {
                    let mut p = cur;
                    for _ in 0..n {
                        p = word_end_back(t, p, e == 'E');
                    }
                    mv(p, Kind::Incl)
                }
            },
            'f' | 'F' | 't' | 'T' => {
                sh.last_find = Some((k, keys[1]));
                self.find_char(t, k, keys[1], n, false)?
            }
            ';' | ',' => {
                let (fk, ch) = sh.last_find?;
                let fk = if k == ',' {
                    match fk {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    }
                } else {
                    fk
                };
                self.find_char(t, fk, ch, n, true)?
            }
            '%' => mv(match_bracket(t, cur)?, Kind::Incl),
            '}' | '{' => {
                let fwd = k == '}';
                let mut ls = t.ls(cur);
                let mut reached_edge = false;
                for _ in 0..n {
                    let step = |ls: usize| if fwd { t.next_line_start(ls) } else { t.prev_line_start(ls) };
                    // Skip blank lines, then the paragraph body, stopping on the next blank line.
                    while t.is_empty_line(ls) {
                        match step(ls) {
                            Some(x) => ls = x,
                            None => {
                                reached_edge = true;
                                break;
                            }
                        }
                    }
                    while !reached_edge {
                        match step(ls) {
                            Some(x) => {
                                ls = x;
                                if t.is_empty_line(x) {
                                    break;
                                }
                            }
                            None => {
                                reached_edge = true;
                            }
                        }
                    }
                }
                if reached_edge {
                    mv(if fwd { t.len() } else { 0 }, Kind::Excl)
                } else {
                    mv(ls, Kind::Excl)
                }
            }
            'n' | 'N' => self.search(sh, t, k == 'N', n)?,
            '*' | '#' => {
                let mut i = cur;
                while i < t.le(cur) && !(t.at(i).is_alphanumeric() || t.at(i) == '_') {
                    i += 1;
                }
                let word = |c: char| c.is_alphanumeric() || c == '_';
                if !word(t.at(i)) {
                    return None;
                }
                let mut a = i;
                while a > 0 && word(t.at(a - 1)) {
                    a -= 1;
                }
                let mut b = i;
                while b < t.len() && word(t.at(b)) {
                    b += 1;
                }
                sh.last_search = Some(Search { pat: t.slice(a, b), forward: k == '*', whole: true });
                let s = sh.last_search.clone()?;
                mv(find_pat(t, &s.pat, true, a, s.forward, n)?, Kind::Excl)
            }
            '_' | '+' | '-' => {
                let mut ls = t.ls(cur);
                let steps = if k == '_' { n - 1 } else { n };
                for _ in 0..steps {
                    let next = if k == '-' { t.prev_line_start(ls) } else { t.next_line_start(ls) };
                    match next {
                        Some(x) => ls = x,
                        None => break,
                    }
                }
                mv(t.fnb(ls), Kind::Line)
            }
            _ => return None,
        })
    }

    // ---------- operators ----------

    fn do_operator(&mut self, sh: &mut Shared, t: &mut Text, out: &mut Vec<Action>, op: char, count: Option<usize>, target: Target) {
        let cur = t.cursor;
        let (a, b, lw) = match target {
            Target::Line => {
                let n = count.unwrap_or(1).max(1);
                let ls = t.ls(cur);
                let mut le = t.le(cur);
                for _ in 1..n {
                    if let Some(next) = t.next_line_start(le) {
                        le = t.le(next);
                    } else {
                        break;
                    }
                }
                (ls, le, true)
            }
            Target::Obj(k, ch) => match text_object(t, k, ch) {
                Some(r) => r,
                None => return,
            },
            Target::Motion(keys) => {
                let big = keys[0] == 'W';
                let cw = op == 'c' && matches!(keys[0], 'w' | 'W') && cls(t.at(cur), big) != 0;
                let m = if cw {
                    // `cw` on a word behaves like `ce`, but stays put on the last char of a word.
                    let mut p = cur;
                    for i in 0..count.unwrap_or(1).max(1) {
                        let same = p + 1 < t.len() && cls(t.at(p + 1), big) == cls(t.at(p), big);
                        if i == 0 && !same {
                            continue;
                        }
                        p = word_end(t, p, big);
                    }
                    mv(p, Kind::Incl)
                } else {
                    match self.motion(sh, t, &keys, count, Some(op)) {
                        Some(m) => m,
                        None => return,
                    }
                };
                let (lo, hi) = (cur.min(m.pos), cur.max(m.pos));
                match m.kind {
                    Kind::Line => (t.ls(lo), t.le(hi), true),
                    Kind::Incl => (lo, (hi + 1).min(t.len()), false),
                    Kind::Excl => {
                        // An exclusive motion ending in column 0 stops at the end of the previous line,
                        // and becomes linewise when it starts at or before the first non-blank.
                        if hi > lo && hi == t.ls(hi) && hi <= t.len() && t.at(hi.saturating_sub(1)) == '\n' {
                            let end = hi - 1;
                            if lo <= t.fnb(t.ls(lo)) { (t.ls(lo), end, true) } else { (lo, end, false) }
                        } else {
                            (lo, hi, false)
                        }
                    }
                }
            }
        };
        self.apply_op(sh, t, out, op, (a, b, lw));
    }

    fn set_reg(&mut self, sh: &mut Shared, t: &Text, a: usize, b: usize, lw: bool) -> String {
        let mut s = t.slice(a, b);
        if lw {
            s.push('\n');
        }
        sh.register = Register { text: s.clone(), linewise: lw };
        s
    }

    fn yank(&mut self, sh: &mut Shared, t: &Text, out: &mut Vec<Action>, a: usize, b: usize, lw: bool) {
        let s = self.set_reg(sh, t, a, b, lw);
        out.push(Action::Copy(s));
    }

    fn delete(&mut self, sh: &mut Shared, t: &mut Text, a: usize, b: usize, lw: bool) {
        self.set_reg(sh, t, a, b, lw);
        if lw {
            if b < t.len() {
                t.replace(a, b + 1, "");
                t.cursor = a;
            } else if a > 0 {
                t.replace(a - 1, b, "");
                t.cursor = t.ls(a - 1);
            } else {
                t.replace(a, b, "");
                t.cursor = 0;
            }
            if t.cursor > 0 && t.cursor >= t.len() && t.at(t.cursor - 1) == '\n' {
                t.cursor = t.ls(t.cursor - 1);
            }
            t.cursor = t.fnb(t.cursor);
        } else {
            t.replace(a, b, "");
            t.cursor = a;
        }
    }

    fn apply_op(&mut self, sh: &mut Shared, t: &mut Text, out: &mut Vec<Action>, op: char, (a, b, lw): (usize, usize, bool)) {
        match op {
            'y' => {
                let cur = t.cursor;
                self.yank(sh, t, out, a, b, lw);
                t.cursor = if lw {
                    if a < t.ls(cur) { (a + (cur - t.ls(cur))).min(t.le(a)) } else { cur }
                } else {
                    a
                };
            }
            'd' => {
                if b > a || lw {
                    self.delete(sh, t, a, b, lw);
                }
            }
            'c' => {
                self.set_reg(sh, t, a, b, lw);
                if lw {
                    let ind = t.indent_of(a);
                    t.replace(a, b, &ind);
                    t.cursor = a + ind.chars().count();
                } else {
                    t.replace(a, b, "");
                    t.cursor = a;
                }
                self.enter_insert(t);
            }
            '>' | '<' => self.shift(t, a, b, op == '>'),
            _ => {}
        }
    }

    fn shift(&mut self, t: &mut Text, a: usize, b: usize, right: bool) {
        let last = t.ls(b);
        let mut starts = Vec::new();
        let mut ls = t.ls(a);
        loop {
            starts.push(ls);
            if ls >= last {
                break;
            }
            ls = t.le(ls) + 1;
        }
        let unit = t.indent.clone();
        for &s in starts.iter().rev() {
            if right {
                if t.le(s) > s {
                    t.replace(s, s, &unit);
                }
            } else if t.at(s) == '\t' {
                t.replace(s, s + 1, "");
            } else {
                let n = t.buf[s..].iter().take(unit.chars().count()).take_while(|c| **c == ' ').count();
                t.replace(s, s + n, "");
            }
        }
        t.cursor = t.fnb(starts[0]);
    }

    fn join(&mut self, t: &mut Text, ls: usize, joins: usize) {
        for _ in 0..joins {
            let le = t.le(ls);
            if le >= t.len() {
                break;
            }
            let next = le + 1;
            let j = t.skip_ws(next);
            let next_empty = t.at(j) == '\n' || j >= t.len();
            let last = if le > ls { t.at(le - 1) } else { '\n' };
            let sep = if next_empty || matches!(last, ' ' | '\t' | '\n') || t.at(j) == ')' { "" } else { " " };
            t.replace(le, j, sep);
            t.cursor = le;
        }
    }

    fn paste(&mut self, sh: &Shared, t: &mut Text, before: bool, count: usize) {
        let reg = sh.register.clone();
        if reg.text.is_empty() {
            return;
        }
        let text = reg.text.repeat(count);
        let cur = t.cursor;
        if reg.linewise {
            if before {
                let ls = t.ls(cur);
                t.replace(ls, ls, &text);
                t.cursor = t.fnb(ls);
            } else {
                let le = t.le(cur);
                if le < t.len() {
                    t.replace(le + 1, le + 1, &text);
                    t.cursor = t.fnb(le + 1);
                } else {
                    let s = format!("\n{}", text.strip_suffix('\n').unwrap_or(&text));
                    t.replace(le, le, &s);
                    t.cursor = t.fnb(le + 1);
                }
            }
        } else {
            let n = text.chars().count();
            let pos = if before || t.at(cur) == '\n' || cur >= t.len() { cur } else { cur + 1 };
            t.replace(pos, pos, &text);
            t.cursor = if text.contains('\n') { pos } else { pos + n - 1 };
        }
    }

    fn toggle_case(c: char) -> String {
        if c.is_lowercase() { c.to_uppercase().collect() } else { c.to_lowercase().collect() }
    }

    // ---------- simple commands ----------

    fn do_simple(&mut self, sh: &mut Shared, t: &mut Text, out: &mut Vec<Action>, c: char, arg: Option<char>, count: Option<usize>) -> bool {
        let n = count.unwrap_or(1).max(1);
        let cur = t.cursor;
        match c {
            'x' | 'X' | 's' => {
                let (a, b) = if c == 'X' { (cur.saturating_sub(n).max(t.ls(cur)), cur) } else { (cur, (cur + n).min(t.le(cur))) };
                if b > a {
                    self.delete(sh, t, a, b, false);
                }
                if c == 's' {
                    self.enter_insert(t);
                }
                true
            }
            'D' | 'C' => {
                let mut e = t.le(cur);
                for _ in 1..n {
                    if e < t.len() {
                        e = t.le(e + 1);
                    }
                }
                if e > cur {
                    self.delete(sh, t, cur, e, false);
                }
                if c == 'C' {
                    self.enter_insert(t);
                }
                true
            }
            'S' => {
                let ls = t.ls(cur);
                let mut le = t.le(cur);
                for _ in 1..n {
                    if le < t.len() {
                        le = t.le(le + 1);
                    }
                }
                self.apply_op(sh, t, out, 'c', (ls, le, true));
                true
            }
            'Y' => {
                let ls = t.ls(cur);
                let mut le = t.le(cur);
                for _ in 1..n {
                    if le < t.len() {
                        le = t.le(le + 1);
                    }
                }
                self.yank(sh, t, out, ls, le, true);
                false
            }
            'p' | 'P' => {
                self.paste(sh, t, c == 'P', n);
                true
            }
            'J' => {
                self.join(t, t.ls(cur), n.max(2) - 1);
                true
            }
            'r' => {
                let ch = arg.unwrap_or(' ');
                let le = t.le(cur);
                if cur + n <= le && le > cur {
                    let s: String = std::iter::repeat_n(ch, n).collect();
                    t.replace(cur, cur + n, &s);
                    t.cursor = cur + n - 1;
                    if ch == '\n' {
                        t.cursor = cur + 1;
                    }
                }
                true
            }
            '~' => {
                let e = (cur + n).min(t.le(cur));
                if e > cur {
                    let s: String = t.buf[cur..e].iter().flat_map(|c| Self::toggle_case(*c).chars().collect::<Vec<_>>()).collect();
                    t.replace(cur, e, &s);
                    t.cursor = e;
                }
                true
            }
            'i' => {
                self.enter_insert_n(t, n);
                true
            }
            'a' => {
                if t.at(cur) != '\n' && cur < t.len() {
                    t.cursor += 1;
                }
                self.enter_insert_n(t, n);
                true
            }
            'I' => {
                t.cursor = t.skip_ws(t.ls(cur)).min(t.le(cur));
                self.enter_insert_n(t, n);
                true
            }
            'A' => {
                t.cursor = t.le(cur);
                self.enter_insert_n(t, n);
                true
            }
            'o' | 'O' => {
                let ls = t.ls(cur);
                let mut ind = t.indent_of(ls);
                let le = t.le(cur);
                if c == 'o' {
                    if matches!(t.buf[ls..le].iter().rev().find(|c| !c.is_whitespace()), Some('{' | '[' | '(')) {
                        ind.push_str(&t.indent);
                    }
                    t.replace(le, le, &format!("\n{ind}"));
                    t.cursor = le + 1 + ind.chars().count();
                } else {
                    t.replace(ls, ls, &format!("{ind}\n"));
                    t.cursor = ls + ind.chars().count();
                }
                self.enter_insert(t);
                true
            }
            'u' => {
                self.undo(t, n);
                false
            }
            'v' | 'V' => {
                self.mode = if c == 'v' { Mode::Visual } else { Mode::VisualLine };
                self.anchor = cur;
                self.cur = cur;
                false
            }
            '.' => {
                self.dot(sh, t, out);
                false
            }
            ':' | '/' | '?' => {
                self.cmdline = Some((c, String::new()));
                false
            }
            'Z' => {
                match arg {
                    Some('Z') => out.push(Action::SaveClose),
                    _ => out.push(Action::Close { force: true }),
                }
                false
            }
            _ => false,
        }
    }

    fn dot(&mut self, sh: &mut Shared, t: &mut Text, out: &mut Vec<Action>) {
        let Some(ch) = sh.last_change.clone() else { return };
        self.replaying = true;
        for c in &ch.keys {
            self.feed_normal(sh, Key::Char(*c), t, out);
        }
        if self.mode == Mode::Insert {
            if let Some(txt) = &ch.insert {
                let c = t.cursor;
                t.replace(c, c, txt);
                t.cursor = c + txt.chars().count();
            }
            self.leave_insert(sh, t);
        }
        self.replaying = false;
    }

    // ---------- command line ----------

    fn feed_cmdline(&mut self, sh: &mut Shared, key: Key, t: &mut Text, out: &mut Vec<Action>) {
        match key {
            Key::Esc => self.cmdline = None,
            Key::Backspace => {
                if self.cmdline.as_mut().is_some_and(|(_, s)| s.pop().is_none()) {
                    self.cmdline = None;
                }
            }
            Key::Char(c) => {
                if let Some((_, s)) = &mut self.cmdline {
                    s.push(c);
                }
            }
            Key::Enter => {
                if let Some((kind, text)) = self.cmdline.take() {
                    self.run_cmdline(sh, t, out, kind, text);
                }
            }
            Key::Ctrl(_) => {}
        }
    }

    fn run_cmdline(&mut self, sh: &mut Shared, t: &mut Text, out: &mut Vec<Action>, kind: char, text: String) {
        if kind != ':' {
            let forward = kind == '/';
            if !text.is_empty() {
                sh.last_search = Some(Search { pat: text, forward, whole: false });
            } else if let Some(s) = &mut sh.last_search {
                s.forward = forward;
            }
            if let Some(m) = self.search(sh, t, false, 1) {
                t.cursor = m.pos;
                t.clamp();
            }
            return;
        }
        let cmd = text.trim().trim_start_matches(':').trim();
        match cmd {
            "" | "noh" | "nohlsearch" => {}
            "w" | "write" => out.push(Action::Save),
            "q" | "quit" => out.push(Action::Close { force: false }),
            "q!" | "quit!" => out.push(Action::Close { force: true }),
            "wq" | "x" | "xit" | "wq!" | "exit" => out.push(Action::SaveClose),
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                let line = n.parse::<usize>().unwrap_or(1).max(1);
                t.cursor = t.fnb(t.line_start_of(line - 1));
            }
            other => self.message = format!("Not an editor command: {other}"),
        }
    }

    // ---------- visual mode ----------

    fn feed_visual(&mut self, sh: &mut Shared, key: Key, t: &mut Text, out: &mut Vec<Action>) {
        let c = match key {
            Key::Esc => {
                self.pending.clear();
                self.mode = Mode::Normal;
                t.clamp();
                return;
            }
            Key::Ctrl(c) => {
                self.ctrl(c, t);
                return;
            }
            Key::Backspace => 'h',
            Key::Enter => '+',
            Key::Char(c) => c,
        };
        self.pending.push(c);
        match parse(&self.pending, true) {
            Parse::Incomplete => {}
            Parse::Invalid => self.pending.clear(),
            Parse::Cmd(cmd) => {
                self.pending.clear();
                match cmd.kind {
                    CmdKind::Motion(keys) => self.do_motion(sh, t, &keys, cmd.count),
                    CmdKind::Obj(k, ch) => {
                        if let Some((a, b, lw)) = text_object(t, k, ch) {
                            self.anchor = a;
                            t.cursor = b.saturating_sub(1).max(a);
                            if lw {
                                self.mode = Mode::VisualLine;
                            }
                        }
                    }
                    CmdKind::Simple(c, arg) => self.visual_op(sh, t, out, c, arg),
                    CmdKind::Op { .. } => {}
                }
            }
        }
    }

    fn visual_range(&self, t: &Text, force_lines: bool) -> (usize, usize, bool) {
        let (lo, hi) = (self.anchor.min(t.cursor), self.anchor.max(t.cursor));
        if self.mode == Mode::VisualLine || force_lines {
            (t.ls(lo), t.le(hi), true)
        } else {
            (lo, (hi + 1).min(t.len()), false)
        }
    }

    fn visual_op(&mut self, sh: &mut Shared, t: &mut Text, out: &mut Vec<Action>, c: char, arg: Option<char>) {
        match c {
            'o' => {
                std::mem::swap(&mut self.anchor, &mut t.cursor);
                return;
            }
            'v' | 'V' => {
                let target = if c == 'v' { Mode::Visual } else { Mode::VisualLine };
                self.mode = if self.mode == target { Mode::Normal } else { target };
                t.clamp();
                return;
            }
            ':' => {
                self.mode = Mode::Normal;
                self.cmdline = Some((':', String::new()));
                return;
            }
            _ => {}
        }
        let (a, b, lw) = self.visual_range(t, matches!(c, 'X' | 'D' | 'Y' | 'S' | 'C'));
        let edits0 = t.edits;
        let before = (t.buf.clone(), a);
        self.mode = Mode::Normal;
        match c {
            'y' | 'Y' => {
                self.yank(sh, t, out, a, b, lw);
                t.cursor = a;
            }
            'd' | 'x' | 'X' | 'D' => self.delete(sh, t, a, b, lw),
            'c' | 's' | 'S' | 'C' => self.apply_op(sh, t, out, 'c', (a, b, lw)),
            '>' | '<' => self.shift(t, a, b, c == '>'),
            'J' => {
                let joins = t.buf[a..b.min(t.len())].iter().filter(|c| **c == '\n').count().max(1);
                self.join(t, t.ls(a), joins);
            }
            '~' | 'u' | 'U' => {
                let s: String = t.slice(a, b).chars().flat_map(|ch| match c {
                    'u' => ch.to_lowercase().collect::<Vec<_>>(),
                    'U' => ch.to_uppercase().collect(),
                    _ => Self::toggle_case(ch).chars().collect(),
                }).collect();
                t.replace(a, b, &s);
                t.cursor = a;
            }
            'r' => {
                let ch = arg.unwrap_or(' ');
                let s: String = t.slice(a, b).chars().map(|x| if x == '\n' { x } else { ch }).collect();
                t.replace(a, b, &s);
                t.cursor = a;
            }
            'p' | 'P' => {
                let reg = sh.register.clone();
                if reg.text.is_empty() {
                    return;
                }
                let removed = self.set_reg(sh, t, a, b, lw);
                let len_before = t.len();
                if lw {
                    let mut txt = reg.text.clone();
                    if !reg.linewise {
                        txt.push('\n');
                    }
                    if b < len_before {
                        t.replace(a, b + 1, &txt);
                    } else {
                        txt.pop();
                        t.replace(a, b, &txt);
                    }
                    t.cursor = t.fnb(a);
                } else {
                    let txt = if reg.linewise { format!("\n{}", reg.text) } else { reg.text.clone() };
                    t.replace(a, b, &txt);
                    t.cursor = if reg.linewise { a + 1 } else { a + txt.chars().count().saturating_sub(1) };
                }
                sh.register = Register { text: removed, linewise: lw };
            }
            _ => {}
        }
        if self.mode == Mode::Insert {
            if let Some(ins) = self.insert.as_mut() {
                ins.before = Some(before);
            }
        } else if t.edits != edits0 {
            self.push_undo(t, before);
        }
        if self.mode == Mode::Normal {
            t.clamp();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|` marks the cursor. Keys: plain chars, `<Esc>`, `<CR>`, `<BS>`, `<C-r>`.
    fn keys(s: &str) -> Vec<Key> {
        let mut out = vec![];
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '<' {
                let name: String = it.by_ref().take_while(|c| *c != '>').collect();
                out.push(match name.as_str() {
                    "Esc" => Key::Esc,
                    "CR" => Key::Enter,
                    "BS" => Key::Backspace,
                    "lt" => Key::Char('<'),
                    n if n.starts_with("C-") => Key::Ctrl(n.chars().nth(2).unwrap()),
                    _ => panic!("bad key {name}"),
                });
            } else {
                out.push(Key::Char(c));
            }
        }
        out
    }

    fn show(t: &Text) -> String {
        let mut s: Vec<char> = t.buf.clone();
        s.insert(t.cursor.min(s.len()), '|');
        s.into_iter().collect()
    }

    fn run_with(init: &str, k: &str) -> (Text, Vim, Shared, Vec<Action>) {
        let cursor = init.chars().position(|c| c == '|').unwrap();
        let text: String = init.chars().filter(|c| *c != '|').collect();
        let mut t = Text::new(&text, cursor);
        t.indent = "  ".into();
        let (mut vim, mut sh, mut out) = (Vim::default(), Shared::default(), vec![]);
        for key in keys(k) {
            vim.feed(&mut sh, key, &mut t, &mut out);
        }
        (t, vim, sh, out)
    }

    fn run(init: &str, k: &str) -> String {
        show(&run_with(init, k).0)
    }

    #[test]
    fn basic_motions() {
        assert_eq!(run("|foo bar", "w"), "foo |bar");
        assert_eq!(run("foo| bar", "b"), "|foo bar");
        assert_eq!(run("|foo bar", "e"), "fo|o bar");
        assert_eq!(run("|foo bar", "$"), "foo ba|r");
        assert_eq!(run("  fo|o", "0"), "|  foo");
        assert_eq!(run("  fo|o", "^"), "  |foo");
        assert_eq!(run("|a\nb\nc", "jj"), "a\nb\n|c");
        assert_eq!(run("a\nb\n|c", "gg"), "|a\nb\nc");
        assert_eq!(run("|a\nb\nc\n", "G"), "a\nb\n|c\n");
        assert_eq!(run("|a\nb\nc", "2G"), "a\n|b\nc");
        assert_eq!(run("|a,b,c", "f,;"), "a,b|,c");
        assert_eq!(run("|a,b,c", "t,"), "|a,b,c");
        assert_eq!(run("|a(b)c", "%"), "a(b|)c");
        assert_eq!(run("|foo.bar baz", "W"), "foo.bar |baz");
    }

    #[test]
    fn keeps_column_on_vertical_moves() {
        assert_eq!(run("abc|d\n\nabcd", "jj"), "abcd\n\nabc|d");
        assert_eq!(run("ab|c\nx\nabc", "jjk"), "abc\n|x\nabc");
    }

    #[test]
    fn delete_change_yank() {
        assert_eq!(run("|foo bar", "dw"), "|bar");
        assert_eq!(run("|foo bar", "2dw"), "|");
        assert_eq!(run("foo |bar", "dw"), "foo| ");
        assert_eq!(run("a\n|b\nc", "dd"), "a\n|c");
        assert_eq!(run("|a\nb\nc", "2dd"), "|c");
        assert_eq!(run("a\nb\n|c", "dd"), "a\n|b");
        assert_eq!(run("|foo bar", "cwxy<Esc>"), "x|y bar");
        assert_eq!(run("|foo bar", "ccx<Esc>"), "|x");
        assert_eq!(run("|abc", "3x"), "|");
        assert_eq!(run("a|bc", "D"), "|a");
        assert_eq!(run("a|bc", "Cx<Esc>"), "a|x");
        assert_eq!(run("|abc", "ylp"), "a|abc");
        assert_eq!(run("|a\nb", "yyp"), "a\n|a\nb");
        assert_eq!(run("a\n|b", "yyP"), "a\n|b\nb");
        assert_eq!(run("|ab", "xp"), "b|a");
        assert_eq!(run("|foo bar", "d$"), "|");
        assert_eq!(run("a\n|b\nc", "dk"), "|c");
        assert_eq!(run("a\n\n|b", "dgg"), "|");
    }

    #[test]
    fn cw_on_last_char_of_word() {
        assert_eq!(run("fo|o bar", "cwX<Esc>"), "fo|X bar");
    }

    #[test]
    fn text_objects() {
        assert_eq!(run("foo |bar baz", "ciwX<Esc>"), "foo |X baz");
        assert_eq!(run("foo |bar baz", "daw"), "foo |baz");
        assert_eq!(run("say \"he|llo\" ok", "di\""), "say \"|\" ok");
        assert_eq!(run("say \"he|llo\" ok", "da\""), "say |ok");
        assert_eq!(run("f(a, |b)", "di("), "f(|)");
        assert_eq!(run("f(a, |b)", "ci)x<Esc>"), "f(|x)");
        assert_eq!(run("f(a, |b)", "da)"), "|f");
        assert_eq!(run("a[|1]", "yi[P"), "a[|11]");
        assert_eq!(run("fn x() {\n  |a;\n  b;\n}", "di{"), "fn x() {\n|}");
        assert_eq!(run("a\n|b\nc\n\nd", "dap"), "|d");
        assert_eq!(run("a\n|b\n\nc", "dip"), "|\nc");
    }

    #[test]
    fn insert_commands() {
        assert_eq!(run("|ab", "ix<Esc>"), "|xab");
        assert_eq!(run("|ab", "ax<Esc>"), "a|xb");
        assert_eq!(run("a|b", "Ax<Esc>"), "ab|x");
        assert_eq!(run("  a|b", "Ix<Esc>"), "  |xab");
        assert_eq!(run("  x|", "oy<Esc>"), "  x\n  |y");
        assert_eq!(run("  x|", "Oy<Esc>"), "  |y\n  x");
        assert_eq!(run("f() |{", "ox<Esc>"), "f() {\n  |x");
        assert_eq!(run("|abc", "sX<Esc>"), "|Xbc");
    }

    #[test]
    fn dot_repeat() {
        assert_eq!(run("|a a a", "dw."), "|a");
        assert_eq!(run("|a b c d", "cwX<Esc>w."), "X |X c d");
        assert_eq!(run("|a\nb\nc", "ddj."), "|b");
        assert_eq!(run("|x\n", "ohi<Esc>."), "x\nhi\nh|i\n");
        assert_eq!(run("|abcd", "x.."), "|d");
    }

    #[test]
    fn undo_redo() {
        assert_eq!(run("|abc", "xu"), "|abc");
        assert_eq!(run("|abc", "xxu"), "|bc");
        assert_eq!(run("|abc", "xxuu"), "|abc");
        assert_eq!(run("|abc", "xu<C-r>"), "|bc");
        assert_eq!(run("|a b", "cwX<Esc>u"), "|a b");
        assert_eq!(run("|a", "ix<Esc>u"), "|a");
        assert_eq!(run("|a", "i<Esc>u"), "|a");
    }

    #[test]
    fn visual_mode() {
        assert_eq!(run("|abc def", "vlld"), "| def");
        assert_eq!(run("a|bc def", "viwd"), "| def");
        assert_eq!(run("|a\nb\nc", "Vjd"), "|c");
        assert_eq!(run("|abc", "vly$p"), "abca|b");
        assert_eq!(run("|abc", "vlU"), "|ABc");
        assert_eq!(run("|abc", "vlc-<Esc>"), "|-c");
        assert_eq!(run("|ab\ncd", "Vj>"), "  |ab\n  cd");
        assert_eq!(run("|abc", "vly<Esc>P"), "a|babc");
    }

    #[test]
    fn indent_join_case_replace() {
        assert_eq!(run("|x", ">>"), "  |x");
        assert_eq!(run("  |x", "<lt><lt>"), "|x");
        assert_eq!(run("|a\nb", "2>>"), "  |a\n  b");
        assert_eq!(run("|a\n  b", "J"), "a| b");
        assert_eq!(run("|ab", "~"), "A|b");
        assert_eq!(run("|abc", "rx"), "|xbc");
        assert_eq!(run("|abc", "2rx"), "x|xc");
    }

    #[test]
    fn search_and_counts() {
        assert_eq!(run("|a b a b", "/b<CR>"), "a |b a b");
        assert_eq!(run("|a b a b", "/b<CR>n"), "a b a |b");
        assert_eq!(run("|a b a b", "/b<CR>nN"), "a |b a b");
        assert_eq!(run("a b a |b", "?a<CR>"), "a b |a b");
        assert_eq!(run("|foo x foo", "*"), "foo x |foo");
        let (t, vim, _, _) = run_with("|abc", "/zzz<CR>");
        assert_eq!(show(&t), "|abc");
        assert!(vim.message.contains("not found"));
        assert_eq!(run("|a\n\nb\nc\n\nd", "}"), "a\n|\nb\nc\n\nd");
        assert_eq!(run("|a\n\nb\nc\n\nd", "}}"), "a\n\nb\nc\n|\nd");
        assert_eq!(run("a\n\nb\nc\n\n|d", "{"), "a\n\nb\nc\n|\nd");
    }

    #[test]
    fn ex_commands() {
        let (_, _, _, out) = run_with("|a", ":w<CR>");
        assert_eq!(out, vec![Action::Save]);
        let (_, _, _, out) = run_with("|a", ":wq<CR>");
        assert_eq!(out, vec![Action::SaveClose]);
        let (_, _, _, out) = run_with("|a", ":q!<CR>");
        assert_eq!(out, vec![Action::Close { force: true }]);
        assert_eq!(run("|a\nb\nc", ":3<CR>"), "a\nb\n|c");
        let (_, vim, _, _) = run_with("|a", ":bogus<CR>");
        assert!(vim.message.contains("bogus"));
        let (_, vim, _, out) = run_with("|a", ":w");
        assert!(vim.cmdline.is_some() && out.is_empty());
        let (_, vim, _, out) = run_with("|a", ":w<Esc>");
        assert!(vim.cmdline.is_none() && out.is_empty());
        let (_, vim, _, _) = run_with("|a", ":ab<BS><BS><BS>");
        assert!(vim.cmdline.is_none());
    }

    #[test]
    fn yank_copies_to_clipboard() {
        let (_, _, _, out) = run_with("|abc", "yl");
        assert_eq!(out, vec![Action::Copy("a".into())]);
    }

    #[test]
    fn read_only_buffers_cannot_change() {
        let mut t = Text::new("abc", 0);
        t.read_only = true;
        let (mut vim, mut sh, mut out) = (Vim::default(), Shared::default(), vec![]);
        for k in keys("xdwiZ<Esc>") {
            vim.feed(&mut sh, k, &mut t, &mut out);
        }
        assert_eq!(t.to_text(), "abc");
        assert_eq!(vim.mode, Mode::Normal);
    }

    #[test]
    fn invalid_sequences_reset() {
        assert_eq!(run("|abc", "dzx"), "|bc");
        assert_eq!(run("|abc", "d<Esc>x"), "|bc");
    }

    #[test]
    fn crlf_text_round_trips_and_hides_the_cr() {
        let mut t = Text::new("ab\r\ncd\r\n", 4);
        assert_eq!(t.buf.iter().collect::<String>(), "ab\ncd\n");
        assert_eq!(t.cursor, 3);
        assert_eq!(t.orig(3), 4);
        let (mut vim, mut sh, mut out) = (Vim::default(), Shared::default(), vec![]);
        vim.feed(&mut sh, Key::Char('$'), &mut t, &mut out);
        assert_eq!(t.cursor, 4, "$ lands on 'd', not on a CR");
        for k in [Key::Char('o'), Key::Char('x'), Key::Esc] {
            vim.feed(&mut sh, k, &mut t, &mut out);
        }
        assert_eq!(t.to_text(), "ab\r\ncd\r\nx\r\n");
    }

    #[test]
    fn undo_history_stores_only_the_changed_region() {
        let big: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let mut t = Text::new(&big, 0);
        let (mut vim, mut sh, mut out) = (Vim::default(), Shared::default(), vec![]);
        for _ in 0..50 {
            for k in [Key::Char('x'), Key::Char('j')] {
                vim.feed(&mut sh, k, &mut t, &mut out);
            }
        }
        let stored: usize = vim.undo.iter().map(|o| o.removed.len() + o.inserted.len()).sum();
        assert!(stored < 1000, "undo stores {stored} chars");
        vim.undo(&mut t, 50);
        assert_eq!(t.to_text(), big);
    }

    #[test]
    fn empty_buffer_is_safe() {
        for k in ["x", "dd", "p", "jk", "$", "w", "b", "e", "J", "yyp", "u", "oa<Esc>", "cwx<Esc>", ">>", "vd"] {
            let _ = run("|", k);
        }
    }
}

#[cfg(test)]
#[path = "validate/vim_engine_tests.rs"]
mod validate_tests;
