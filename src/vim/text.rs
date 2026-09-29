//! The buffer as chars, with the line helpers motions and operators are built on.

/// The buffer being edited, as chars so motions are simple index arithmetic.
pub struct Text {
    pub buf: Vec<char>,
    pub cursor: usize,
    /// Visible lines, for page-wise motions.
    pub page: usize,
    pub read_only: bool,
    /// One indentation level (spaces or a tab).
    pub indent: String,
    pub(super) edits: usize,
    /// The source used CRLF; `buf` holds bare `\n` and `to_text` puts the `\r`s back.
    pub(super) crlf: bool,
}

impl Text {
    /// `cursor` is a char index into `text`.
    pub fn new(text: &str, cursor: usize) -> Self {
        let crlf = text.contains("\r\n");
        let mut buf: Vec<char> = Vec::with_capacity(text.len());
        let mut buf_cursor = None;
        let mut chars = text.chars().peekable();
        let mut ci = 0;
        while let Some(c) = chars.next() {
            if ci == cursor {
                buf_cursor = Some(buf.len());
            }
            ci += 1;
            if crlf && c == '\r' && chars.peek() == Some(&'\n') {
                continue;
            }
            buf.push(c);
        }
        let cursor = buf_cursor.unwrap_or(buf.len());
        Self { buf, cursor, page: 30, read_only: false, indent: "    ".into(), edits: 0, crlf }
    }

    /// Like `new`, but `cursor` is already an index into the (CR-stripped) buffer.
    pub fn with_buffer_cursor(text: &str, cursor: usize) -> Self {
        let mut t = Self::new(text, 0);
        t.cursor = cursor.min(t.buf.len());
        t
    }

    /// Buffer index → char index in `to_text()`.
    pub fn orig(&self, i: usize) -> usize {
        let i = i.min(self.buf.len());
        if self.crlf { i + self.buf[..i].iter().filter(|c| **c == '\n').count() } else { i }
    }

    pub fn edited(&self) -> bool {
        self.edits > 0
    }

    pub fn to_text(&self) -> String {
        if self.crlf {
            let mut s = String::with_capacity(self.buf.len() + self.buf.len() / 20);
            for &c in &self.buf {
                if c == '\n' {
                    s.push('\r');
                }
                s.push(c);
            }
            s
        } else {
            self.buf.iter().collect()
        }
    }

    pub(super) fn len(&self) -> usize {
        self.buf.len()
    }

    pub(super) fn at(&self, i: usize) -> char {
        self.buf.get(i).copied().unwrap_or('\0')
    }

    pub(super) fn ls(&self, i: usize) -> usize {
        let mut i = i.min(self.len());
        while i > 0 && self.buf[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    pub(super) fn le(&self, i: usize) -> usize {
        let mut i = i.min(self.len());
        while i < self.len() && self.buf[i] != '\n' {
            i += 1;
        }
        i
    }

    pub(super) fn skip_ws(&self, mut i: usize) -> usize {
        while matches!(self.at(i), ' ' | '\t') {
            i += 1;
        }
        i
    }

    /// First non-blank of the line starting at `ls` (last char if the line is all blank).
    pub(super) fn fnb(&self, ls: usize) -> usize {
        let e = self.le(ls);
        let p = self.skip_ws(ls);
        if p >= e && e > ls { e - 1 } else { p }
    }

    pub(super) fn is_empty_line(&self, ls: usize) -> bool {
        self.le(ls) == ls
    }

    /// The empty "line" after a trailing newline is not a real line, so it is never a target.
    pub(super) fn next_line_start(&self, i: usize) -> Option<usize> {
        let e = self.le(i);
        let n = e + 1;
        if e >= self.len() || n >= self.len() { None } else { Some(n) }
    }

    pub(super) fn prev_line_start(&self, i: usize) -> Option<usize> {
        let s = self.ls(i);
        if s == 0 { None } else { Some(self.ls(s - 1)) }
    }

    pub(super) fn last_line_start(&self) -> usize {
        let n = self.len();
        if n > 0 && self.buf[n - 1] == '\n' { self.ls(n - 1) } else { self.ls(n) }
    }

    pub(super) fn line_start_of(&self, line: usize) -> usize {
        let mut ls = 0;
        for _ in 0..line {
            match self.next_line_start(ls) {
                Some(x) => ls = x,
                None => break,
            }
        }
        ls
    }

    pub(super) fn slice(&self, a: usize, b: usize) -> String {
        let a = a.min(self.len());
        let b = b.clamp(a, self.len());
        self.buf[a..b].iter().collect()
    }

    pub(super) fn replace(&mut self, a: usize, b: usize, s: &str) {
        let a = a.min(self.len());
        let b = b.clamp(a, self.len());
        self.buf.splice(a..b, s.chars());
        self.edits += 1;
    }

    /// Normal mode keeps the cursor on a character, not on the newline.
    pub(super) fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.len());
        if (self.cursor == self.len() || self.at(self.cursor) == '\n') && self.cursor > self.ls(self.cursor) {
            self.cursor -= 1;
        }
    }

    pub(super) fn indent_of(&self, ls: usize) -> String {
        self.buf[ls.min(self.len())..].iter().take_while(|c| matches!(**c, ' ' | '\t')).collect()
    }
}
