//! Turning typed keys into a command: counts, operators, motions and text objects.


#[derive(Clone, Debug)]
pub(super) enum Target {
    Line,
    Motion(Vec<char>),
    Obj(char, char),
}

#[derive(Clone, Debug)]
pub(super) enum CmdKind {
    Motion(Vec<char>),
    Op { op: char, count2: Option<usize>, target: Target },
    Simple(char, Option<char>),
    Obj(char, char),
}

#[derive(Clone, Debug)]
pub(super) struct Cmd {
    pub(super) count: Option<usize>,
    pub(super) kind: CmdKind,
}

impl Cmd {
    pub(super) fn modifies(&self) -> bool {
        match &self.kind {
            CmdKind::Op { op, .. } => *op != 'y',
            CmdKind::Simple(c, _) => "xXDCsSpPJrioaIAO~".contains(*c),
            _ => false,
        }
    }
}

pub(super) enum Parse {
    Incomplete,
    Invalid,
    Cmd(Cmd),
}

pub(super) enum M {
    Incomplete,
    No,
    Yes,
}

pub(super) fn parse_count(keys: &[char], mut i: usize) -> (Option<usize>, usize) {
    if i < keys.len() && ('1'..='9').contains(&keys[i]) {
        let mut n = 0usize;
        while i < keys.len() && keys[i].is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(keys[i] as usize - '0' as usize);
            i += 1;
        }
        (Some(n.min(100_000)), i)
    } else {
        (None, i)
    }
}

pub(super) fn parse_motion(k: &[char]) -> M {
    match k[0] {
        'h' | 'j' | 'k' | 'l' | 'w' | 'W' | 'b' | 'B' | 'e' | 'E' | '0' | '^' | '$' | 'G' | '}' | '{' | '%' | 'n'
        | 'N' | '_' | '+' | '-' | ' ' | ';' | ',' | '*' | '#' => M::Yes,
        'f' | 'F' | 't' | 'T' => {
            if k.len() < 2 { M::Incomplete } else { M::Yes }
        }
        'g' => {
            if k.len() < 2 {
                M::Incomplete
            } else if matches!(k[1], 'g' | 'e' | 'E') {
                M::Yes
            } else {
                M::No
            }
        }
        _ => M::No,
    }
}

pub(super) fn parse(keys: &[char], visual: bool) -> Parse {
    let (count, mut i) = parse_count(keys, 0);
    if i >= keys.len() {
        return Parse::Incomplete;
    }
    let c = keys[i];
    let cmd = |kind| Parse::Cmd(Cmd { count, kind });
    if !visual && matches!(c, 'd' | 'c' | 'y' | '>' | '<') {
        i += 1;
        let (count2, ni) = parse_count(keys, i);
        i = ni;
        if i >= keys.len() {
            return Parse::Incomplete;
        }
        let t = keys[i];
        if t == c {
            return cmd(CmdKind::Op { op: c, count2, target: Target::Line });
        }
        if t == 'i' || t == 'a' {
            return if i + 1 >= keys.len() {
                Parse::Incomplete
            } else {
                cmd(CmdKind::Op { op: c, count2, target: Target::Obj(t, keys[i + 1]) })
            };
        }
        return match parse_motion(&keys[i..]) {
            M::Incomplete => Parse::Incomplete,
            M::No => Parse::Invalid,
            M::Yes => cmd(CmdKind::Op { op: c, count2, target: Target::Motion(keys[i..].to_vec()) }),
        };
    }
    if visual && (c == 'i' || c == 'a') {
        return if i + 1 >= keys.len() { Parse::Incomplete } else { cmd(CmdKind::Obj(c, keys[i + 1])) };
    }
    match parse_motion(&keys[i..]) {
        M::Incomplete => return Parse::Incomplete,
        M::Yes => return cmd(CmdKind::Motion(keys[i..].to_vec())),
        M::No => {}
    }
    match c {
        'r' => {
            if i + 1 >= keys.len() { Parse::Incomplete } else { cmd(CmdKind::Simple('r', Some(keys[i + 1]))) }
        }
        'Z' => {
            if i + 1 >= keys.len() {
                Parse::Incomplete
            } else if matches!(keys[i + 1], 'Z' | 'Q') {
                cmd(CmdKind::Simple('Z', Some(keys[i + 1])))
            } else {
                Parse::Invalid
            }
        }
        'x' | 'X' | 'D' | 'C' | 's' | 'S' | 'Y' | 'p' | 'P' | 'J' | 'u' | 'i' | 'a' | 'I' | 'A' | 'o' | 'O' | 'v'
        | 'V' | '.' | ':' | '/' | '?' | '~' | 'U' | 'd' | 'c' | 'y' | '>' | '<' => cmd(CmdKind::Simple(c, None)),
        _ => Parse::Invalid,
    }
}
