//! Validation: extra vim engine cases and a panic fuzzer. Included from vim.rs.

use super::*;

fn run(init: &str, keys: &[Key]) -> (String, usize, Mode) {
    let cursor = init.chars().position(|c| c == '|').unwrap_or(0);
    let text: String = init.chars().filter(|c| *c != '|').collect();
    let mut t = Text::new(&text, cursor);
    let (mut vim, mut sh, mut out) = (Vim::default(), Shared::default(), vec![]);
    for k in keys {
        vim.feed(&mut sh, *k, &mut t, &mut out);
    }
    (t.to_text(), t.cursor, vim.mode)
}

fn k(s: &str) -> Vec<Key> {
    s.chars().map(|c| if c == '\u{1b}' { Key::Esc } else { Key::Char(c) }).collect()
}

#[test]
fn change_inner_word_on_an_empty_line_enters_insert() {
    let (t, _, mode) = run("a\n|\nb", &k("ciw"));
    assert_eq!(mode, Mode::Insert, "vim enters insert mode for ciw on an empty line (text {t:?})");
}

#[test]
fn count_before_insert_repeats_text() {
    let (t, _, _) = run("|ab", &k("3ix\u{1b}"));
    assert_eq!(t, "xxxab", "3ix<Esc> should insert xxx (vim behaviour)");
}

#[test]
fn misc_edges() {
    assert_eq!(run("|a\nb\n", &k("Gdd")).0, "a\n");
    assert_eq!(run("a\n|b", &k("yyP")).0, "a\nb\nb");
    assert_eq!(run("|  a", &k("cwx\u{1b}")).0, "xa", "cw on whitespace changes the whitespace only");
    assert_eq!(run("|a😀b", &k("lx")).0, "ab");
    assert_eq!(run("|a😀b", &k("~~~")).0, "A😀B");
    assert_eq!(run("|é", &k("~")).0, "É");
    assert_eq!(run("|ß", &k("~")).0, "SS", "or unchanged - just must not panic");
}

/// Replays keys; returns Some(reason) if a panic or invariant violation happens.
fn replay(text: &str, cursor: usize, keys: &[Key]) -> Option<String> {
    let mut t = Text::new(text, cursor);
    t.page = 5;
    let (mut vim, mut sh, mut out) = (Vim::default(), Shared::default(), vec![]);
    for key in keys {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vim.feed(&mut sh, *key, &mut t, &mut out)));
        if res.is_err() {
            return Some("panic".into());
        }
        if t.cursor > t.buf.len() {
            return Some(format!("cursor {} > len {}", t.cursor, t.buf.len()));
        }
        if let Some((a, b)) = vim.selection(&t)
            && !(a <= b && b <= t.buf.len())
        {
            return Some(format!("selection {a}..{b} len {}", t.buf.len()));
        }
    }
    None
}

fn minimize(text: &str, cursor: usize, mut keys: Vec<Key>) -> (Vec<Key>, String) {
    let mut i = 0;
    while i < keys.len() {
        let mut k2 = keys.clone();
        k2.remove(i);
        if replay(text, cursor, &k2).is_some() { keys = k2; } else { i += 1; }
    }
    let why = replay(text, cursor, &keys).unwrap();
    (keys, why)
}

/// Random keys on random texts must never panic and must keep the cursor in bounds.
#[test]
fn fuzz_no_panics() {
    let mut seed: u64 = 42;
    let mut rnd = move |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % n.max(1)
    };
    let texts = ["", "\n", "a", "a\n", "\n\n", "foo bar\n  baz(qux)\n\n{x}\n", "😀é\tx\r\ny", "\"a\" 'b' [c] <d>", "a\nb"];
    let alphabet: Vec<char> = "hjklwbeWBE0^$GgftTFT;,%{}nN*#_+-xXdcyp PJr~uUvVia\"'([{<>)]}.:/?ZQs SoOAI123w".chars().collect();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut failures: Vec<String> = Vec::new();
    for round in 0..6000 {
        let text = texts[rnd(texts.len())];
        let c0 = rnd(text.chars().count() + 1);
        let mut log = Vec::new();
        for _ in 0..40 {
            let key = match rnd(12) {
                0 => Key::Esc,
                1 => Key::Enter,
                2 => Key::Backspace,
                3 => Key::Ctrl(['r', 'd', 'u', 'f', 'b', 'z'][rnd(6)]),
                _ => Key::Char(alphabet[rnd(alphabet.len())]),
            };
            log.push(key);
        }
        if replay(text, c0, &log).is_some() {
            let (keys, why) = minimize(text, c0, log);
            let kind = why.split(' ').next().unwrap().to_string();
            if !failures.iter().any(|f| f.starts_with(&kind)) {
                failures.push(format!("{why}: round {round}, text {text:?}, cursor {c0}, keys {keys:?}"));
            }
        }
    }
    std::panic::set_hook(prev_hook);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn redo_in_visual_mode_leaves_anchor_past_the_end() {
    let keys = vec![Key::Char('x'), Key::Char('x'), Key::Char('u'), Key::Char('u'), Key::Char('$'), Key::Char('v'), Key::Ctrl('r'), Key::Ctrl('r'), Key::Char('o')];
    assert_eq!(replay("abc", 0, &keys), None);
}
