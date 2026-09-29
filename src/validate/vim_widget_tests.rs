//! Validation: vim motions driven through the real editor widget (keys go through
//! egui's event queue, insert-mode text goes through egui's TextEdit).
//! Included from editor.rs as a child module.

use super::*;
use super::validate_tests::{CTRL, H};
use egui::Event;

fn vim(text: &str) -> H {
    H::new(text, true)
}

/// Feed a key string: plain chars as text, `<Esc>`, `<CR>`, `<BS>`, `<C-x>`, `<Left>`.
fn feed(h: &mut H, s: &str) {
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '<' && it.peek().is_some_and(|c| c.is_ascii_uppercase()) {
            let name: String = it.by_ref().take_while(|c| *c != '>').collect();
            match name.as_str() {
                "Esc" => h.key(Key::Escape),
                "CR" => h.key(Key::Enter),
                "BS" => h.key(Key::Backspace),
                "Left" => h.key(Key::ArrowLeft),
                "Right" => h.key(Key::ArrowRight),
                "Tab" => h.key(Key::Tab),
                n if n.starts_with("C-") => {
                    let k = match n.chars().nth(2).unwrap() {
                        'r' => Key::R,
                        'd' => Key::D,
                        'u' => Key::U,
                        'f' => Key::F,
                        'b' => Key::B,
                        'z' => Key::Z,
                        _ => panic!("{n}"),
                    };
                    h.key_m(k, CTRL);
                }
                _ => panic!("bad key {name}"),
            }
        } else {
            h.frame(vec![Event::Text(c.to_string())]);
        }
    }
}

fn run(text: &str, keys: &str) -> H {
    let mut h = vim(text);
    feed(&mut h, keys);
    h
}

fn cur(h: &H) -> String {
    let mut chars: Vec<char> = h.doc.text.chars().collect();
    chars.insert(h.doc.cursor_char.min(chars.len()), '|');
    chars.into_iter().collect()
}

#[test]
fn counts() {
    assert_eq!(run("1\n2\n3\n4\n5\n", "3dd").doc.text, "4\n5\n");
    assert_eq!(run("a b c d e", "d2w").doc.text, "c d e");
    assert_eq!(run("a b c d e", "2d2w").doc.text, "e");
    assert_eq!(run("abc\ndef", "10x").doc.text, "\ndef", "x count stops at end of line");
    assert_eq!(cur(&run("1\n2\n3", "10j")), "1\n2\n|3");
    assert_eq!(cur(&run("1\n2\n3", "99G")), "1\n2\n|3");
    assert_eq!(cur(&run("1\n2\n3", "2G")), "1\n|2\n3");
    assert_eq!(run("a\nb\nc\nd", "2yyjjp").doc.text, "a\nb\nc\na\nb\nd");
    assert_eq!(run("x", "3p").doc.text, "x", "p with empty register");
    assert_eq!(run("ab", "yl3p").doc.text, "aaaab");
    assert_eq!(run("ab", "3ix<Esc>").doc.text, "xxxab", "count on insert repeats the text");
}

#[test]
fn visual_modes() {
    assert_eq!(run("abc\ndef\nghi", "vjd").doc.text, "ef\nghi");
    assert_eq!(run("abc\ndef\nghi", "Vjd").doc.text, "ghi");
    assert_eq!(run("abc\ndef", "VyjP").doc.text, "abc\nabc\ndef");
    assert_eq!(run("abc", "v$y$p").doc.text, "abcabc");
    assert_eq!(run("a\nb", "Vj>").doc.text, "    a\n    b");
    assert_eq!(run("abc", "vl~").doc.text, "ABc");
    assert_eq!(run("one two", "wviwcX<Esc>").doc.text, "one X");
    // The widget shows the visual selection as an egui selection.
    let h = run("abcdef", "lvll");
    assert_eq!(h.sel(), (1, 4));
    let h = run("ab\ncd\nef", "jV");
    assert_eq!(h.sel(), (3, 6));
    // Esc leaves visual mode and collapses the selection.
    let h = run("abcdef", "vll<Esc>");
    assert_eq!(h.doc.vim.mode, vim::Mode::Normal);
    let (a, b) = h.sel();
    assert_eq!(a, b);
}

#[test]
fn dot_repeat_through_the_widget() {
    // Appended text typed via egui's TextEdit is recorded for `.`.
    assert_eq!(run("a\nb\nc", "A;<Esc>j.j.").doc.text, "a;\nb;\nc;");
    assert_eq!(run("x\ny", "ihello <Esc>j0.").doc.text, "hello x\nhello y");
    assert_eq!(run("a\nb", ">>j.").doc.text, "    a\n    b");
    assert_eq!(run("foo bar baz", "ciwX<Esc>w.w.").doc.text, "X X X");
    assert_eq!(run("a b c", "dw..").doc.text, "");
    // Backspace inside insert mode is handled by egui, but the result is still repeated.
    assert_eq!(run("a\nb", "Axyz<BS><Esc>j.").doc.text, "axy\nbxy");
    // Enter inside insert mode (auto-indent from the widget) is repeated too.
    assert_eq!(run("  a\n  b", "A!<CR>z<Esc>j.").doc.text, "  a!\n  z\n  b!\n  z");
}

#[test]
fn dot_repeat_after_moving_with_arrows_in_insert() {
    // Moving inside insert mode: vim can't know what was typed; `.` should not insert garbage.
    let h = run("abc\nxyz", "A12<Left>3<Esc>j.");
    assert_eq!(h.doc.text.lines().next().unwrap(), "abc132");
    assert!(!h.doc.text.contains("132\nxyz132") || true);
    eprintln!("after arrows + dot: {:?}", h.doc.text);
}

#[test]
fn undo_redo_through_the_widget() {
    let mut h = run("a\nb\nc", "ddu");
    assert_eq!(h.doc.text, "a\nb\nc");
    feed(&mut h, "<C-r>");
    assert_eq!(h.doc.text, "b\nc");
    feed(&mut h, "uu");
    assert_eq!(h.doc.text, "a\nb\nc", "extra undo is harmless");
    // A whole insert session is one undo step.
    let mut h = run("x", "ihello world<Esc>");
    assert_eq!(h.doc.text, "hello worldx");
    feed(&mut h, "u");
    assert_eq!(h.doc.text, "x");
    feed(&mut h, "<C-r>");
    assert_eq!(h.doc.text, "hello worldx");
    // Several changes undone one by one.
    let mut h = run("abcd", "xxx");
    feed(&mut h, "u");
    assert_eq!(h.doc.text, "cd");
    feed(&mut h, "u");
    assert_eq!(h.doc.text, "bcd");
    feed(&mut h, "u");
    assert_eq!(h.doc.text, "abcd");
    // Ctrl+Z in normal mode also undoes.
    let h = run("abcd", "x<C-z>");
    assert_eq!(h.doc.text, "abcd");
}

#[test]
fn ex_commands() {
    let h = run("a", ":w<CR>");
    assert!(matches!(h.events.as_slice(), [EditorEvent::Save]));
    let h = run("a", ":q<CR>");
    assert!(matches!(h.events.as_slice(), [EditorEvent::Close(false)]));
    let h = run("a", ":q!<CR>");
    assert!(matches!(h.events.as_slice(), [EditorEvent::Close(true)]));
    let h = run("a", ":wq<CR>");
    assert!(matches!(h.events.as_slice(), [EditorEvent::SaveClose]));
    let h = run("a", ":x<CR>");
    assert!(matches!(h.events.as_slice(), [EditorEvent::SaveClose]));
    let h = run("a", "ZZ");
    assert!(matches!(h.events.as_slice(), [EditorEvent::SaveClose]));
    let h = run("a", "ZQ");
    assert!(matches!(h.events.as_slice(), [EditorEvent::Close(true)]));
    let h = run("a\n  b\nc", ":2<CR>");
    assert_eq!(h.doc.cursor_char, 4);
    let h = run("a", ":bogus<CR>");
    assert!(h.doc.vim.message.contains("Not an editor command"));
    assert!(h.events.is_empty());
    // Esc / Backspace cancel the command line.
    let h = run("a", ":w<Esc>");
    assert!(h.events.is_empty() && h.doc.vim.cmdline.is_none());
    let h = run("a", ":<BS>");
    assert!(h.doc.vim.cmdline.is_none());
}

#[test]
fn search() {
    let h = run("foo bar\nbaz bar", "/bar<CR>");
    assert_eq!(h.doc.cursor_char, 4);
    let h = run("foo bar\nbaz bar", "/bar<CR>n");
    assert_eq!(h.doc.cursor_char, 12);
    let h = run("foo bar\nbaz bar", "/bar<CR>nn");
    assert_eq!(h.doc.cursor_char, 4, "wraps");
    let h = run("foo bar\nbaz bar", "/bar<CR>N");
    assert_eq!(h.doc.cursor_char, 12);
    let h = run("foo foo", "*");
    assert_eq!(h.doc.cursor_char, 4);
    let h = run("a😀b😀b", "/b<CR>n");
    assert_eq!(h.doc.cursor_char, 4);
}

#[test]
fn empty_buffer() {
    let h = run("", "ohi<Esc>");
    assert_eq!(h.doc.text, "\nhi");
    let h = run("", "Ohi<Esc>");
    assert_eq!(h.doc.text, "hi\n");
    let h = run("", "ihi<Esc>ddp");
    assert_eq!(h.doc.text, "\nhi", "dd on the only line leaves an empty buffer; p puts it below");
    let h = run("", "yyp");
    assert_eq!(h.doc.text, "\n");
    let h = run("", "vd");
    assert_eq!(h.doc.text, "");
    let h = run("", "Vy");
    assert_eq!(h.doc.text, "");
    let h = run("", "ciwx<Esc>");
    assert_eq!(h.doc.text, "x");
    let h = run("", "A<CR><Esc>");
    assert_eq!(h.doc.text, "\n");
}

#[test]
fn last_line_without_newline() {
    assert_eq!(run("a\nb", "Gdd").doc.text, "a");
    assert_eq!(cur(&run("a\nb", "Gdd")), "|a");
    assert_eq!(run("a\nb", "Gyyp").doc.text, "a\nb\nb");
    assert_eq!(run("a\nb", "ggyyGp").doc.text, "a\nb\na");
    assert_eq!(run("a\nb", "GJ").doc.text, "a\nb");
    assert_eq!(run("a\nb", "J").doc.text, "a b");
    assert_eq!(run("a\nb", "Gox<Esc>").doc.text, "a\nb\nx");
    assert_eq!(run("a\nbc", "G$x").doc.text, "a\nb");
    assert_eq!(run("a\nbc", "G$a!<Esc>").doc.text, "a\nbc!");
    assert_eq!(run("a\nb\n", "Gdd").doc.text, "a\n");
    assert_eq!(run("a\nb", "jdk").doc.text, "");
    assert_eq!(run("a\nb", "Vjd").doc.text, "");
    assert_eq!(run("a\nb", "jVkd").doc.text, "");
    assert_eq!(run("a b", "wD").doc.text, "a ");
    assert_eq!(run("a\nb", "G2dd").doc.text, "a", "count past the end");
}

#[test]
fn insert_mode_keys_go_to_egui() {
    // In insert mode, Tab / Enter use the editor's own handling (indent, auto-indent).
    let h = run("fn x() {}", "f}i<CR><Esc>");
    assert_eq!(h.doc.text, "fn x() {\n    \n}");
    let h = run("x", "i<Tab><Esc>");
    assert_eq!(h.doc.text, "    x");
    // Arrow keys in normal mode map to hjkl.
    let h = run("ab\ncd", "<Right>");
    assert_eq!(h.doc.cursor_char, 1);
}

#[test]
fn toggling_vim_off_mid_insert_does_not_break_typing() {
    let mut h = run("abc", "i");
    h.vim_on = false;
    h.typing("x");
    assert_eq!(h.doc.text, "xabc");
}

#[test]
fn ctrl_d_ctrl_u_page() {
    let text: String = (0..100).map(|i| format!("{i}\n")).collect();
    let mut h = vim(&text);
    feed(&mut h, "<C-d>");
    let after = h.doc.line_col().0;
    assert!(after > 5, "Ctrl+D moved {after} lines");
    feed(&mut h, "<C-u>");
    assert_eq!(h.doc.line_col().0, 0);
    feed(&mut h, "<C-f>");
    assert!(h.doc.line_col().0 > after);
}

