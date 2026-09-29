//! Validation: git integration against real temporary repositories. Included from git.rs.

use super::*;

pub fn scratch_repo(name: &str) -> Repo {
    let d = std::env::temp_dir().join(format!("rim-validate-git-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let r = Repo::init(&d).unwrap();
    r.run(&["config", "user.name", "Rim Test"]).unwrap();
    r.run(&["config", "user.email", "rim@example.test"]).unwrap();
    r.run(&["config", "commit.gpgsign", "false"]).unwrap();
    r.run(&["config", "core.quotepath", "true"]).unwrap();
    r.run(&["checkout", "-q", "-b", "main"]).unwrap();
    r
}

fn write(r: &Repo, rel: &str, s: &str) {
    let p = r.root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn find<'a>(st: &'a Status, path: &str) -> Option<&'a FileStatus> {
    st.files.iter().find(|f| f.path == path)
}

#[test]
fn status_untracked_unicode_and_spaces() {
    let r = scratch_repo("status");
    let st = r.status().unwrap();
    assert_eq!(st.branch, "main", "no-commits branch line parsed");
    assert!(st.files.is_empty());
    assert_eq!(st.head, "");
    write(&r, "plain.txt", "a\n");
    write(&r, "dir/ünïcödé 日本.txt", "b\n");
    write(&r, "with space.txt", "c\n");
    write(&r, "deep/a/b/c.txt", "d\n");
    let st = r.status().unwrap();
    for p in ["plain.txt", "dir/ünïcödé 日本.txt", "with space.txt", "deep/a/b/c.txt"] {
        let f = find(&st, p).unwrap_or_else(|| panic!("{p} missing from {:?}", st.files));
        assert_eq!((f.staged, f.unstaged), (None, Some('?')));
    }
    std::fs::remove_dir_all(&r.root).ok();
}

#[test]
fn stage_unstage_commit_discard_and_marks() {
    let r = scratch_repo("flow");
    write(&r, "a.txt", "one\ntwo\nthree\n");
    write(&r, "ü.txt", "x\n");
    // Unstage in a repo without commits.
    r.stage("a.txt").unwrap();
    assert_eq!(find(&r.status().unwrap(), "a.txt").unwrap().staged, Some('A'));
    r.unstage("a.txt").unwrap();
    assert_eq!(find(&r.status().unwrap(), "a.txt").unwrap().unstaged, Some('?'));
    r.stage_all().unwrap();
    let out = r.commit("first commit").unwrap();
    assert!(out.contains("first commit"));
    let st = r.status().unwrap();
    assert!(st.files.is_empty(), "{:?}", st.files);
    assert_eq!(st.head.len(), 40);
    assert_eq!(r.head_content("a.txt").as_deref(), Some("one\ntwo\nthree\n"));
    assert_eq!(r.head_content("ü.txt").as_deref(), Some("x\n"), "unicode path at HEAD");
    assert_eq!(r.head_content("missing.txt"), None);

    // Modify: staged + unstaged on the same file.
    write(&r, "a.txt", "one\nTWO\nthree\n");
    r.stage("a.txt").unwrap();
    write(&r, "a.txt", "one\nTWO\nthree\nfour\n");
    let st = r.status().unwrap();
    let f = find(&st, "a.txt").unwrap();
    assert_eq!((f.staged, f.unstaged), (Some('M'), Some('M')));
    assert!(r.diff(f, true).contains("+TWO"));
    assert!(r.diff(f, false).contains("+four"));
    // Line markers vs HEAD for the buffer.
    let marks = line_marks(&r.head_content("a.txt").unwrap(), "one\nTWO\nthree\nfour\n");
    assert_eq!(marks, vec![(1, 2, LineMark::Modified), (3, 4, LineMark::Added)]);
    let marks = line_marks("a\nb\nc\n", "a\nc\n");
    assert_eq!(marks, vec![(1, 2, LineMark::Deleted)]);

    // Discard worktree changes: back to the staged version.
    r.discard(f).unwrap();
    assert_eq!(std::fs::read_to_string(r.root.join("a.txt")).unwrap(), "one\nTWO\nthree\n");
    // Discard an untracked file deletes it.
    write(&r, "new.txt", "n");
    let st = r.status().unwrap();
    r.discard(find(&st, "new.txt").unwrap()).unwrap();
    assert!(!r.root.join("new.txt").exists());
    // Deleted file.
    std::fs::remove_file(r.root.join("ü.txt")).unwrap();
    let st = r.status().unwrap();
    assert_eq!(find(&st, "ü.txt").unwrap().unstaged, Some('D'));
    r.discard(find(&st, "ü.txt").unwrap()).unwrap();
    assert!(r.root.join("ü.txt").exists());
    // Commit with an empty index fails with git's message.
    r.unstage_all().unwrap();
    let e = r.commit("nothing").unwrap_err();
    assert!(!e.is_empty());
    std::fs::remove_dir_all(&r.root).ok();
}

#[test]
fn staged_rename_is_one_entry() {
    let r = scratch_repo("rename");
    write(&r, "old name.txt", "same content here\nline2\nline3\n");
    write(&r, "keep.txt", "k\n");
    r.stage_all().unwrap();
    r.commit("c").unwrap();
    r.run(&["mv", "old name.txt", "nëw name.txt"]).unwrap();
    let st = r.status().unwrap();
    assert_eq!(st.files.len(), 1, "{:?}", st.files);
    let f = &st.files[0];
    assert_eq!(f.path, "nëw name.txt");
    assert_eq!(f.staged, Some('R'));
    std::fs::remove_dir_all(&r.root).ok();
}

#[test]
fn worktree_rename_of_intent_to_add_file() {
    // `git add -N new && rm old` makes git report a *worktree* rename (Y = 'R'), which in -z
    // mode is also followed by the original path. Only X = 'R'/'C' is handled.
    let r = scratch_repo("wtrename");
    write(&r, "old.txt", "same content here\nline2\nline3\n");
    r.stage_all().unwrap();
    r.commit("c").unwrap();
    std::fs::rename(r.root.join("old.txt"), r.root.join("new.txt")).unwrap();
    r.run(&["add", "-N", "new.txt"]).unwrap();
    let raw = r.run(&["status", "--porcelain=v1", "-z", "--branch", "--untracked-files=all"]).unwrap();
    eprintln!("raw status: {raw:?}");
    let st = r.status().unwrap();
    let paths: Vec<&str> = st.files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.iter().all(|p| *p == "new.txt" || *p == "old.txt"), "garbage entry parsed from the rename's original path: {:?}", st.files);
    std::fs::remove_dir_all(&r.root).ok();
}

#[test]
fn branches_checkout_create_and_upstream() {
    let r = scratch_repo("branch");
    write(&r, "a", "a");
    r.stage_all().unwrap();
    r.commit("c1").unwrap();
    r.create_branch("feature/ü").unwrap();
    assert_eq!(r.status().unwrap().branch, "feature/ü");
    let mut b = r.branches();
    b.sort();
    assert_eq!(b, vec!["feature/ü".to_string(), "main".to_string()]);
    r.checkout("main").unwrap();
    assert_eq!(r.status().unwrap().branch, "main");
    // Ahead/behind against a local "remote".
    let remote = r.root.with_extension("remote.git");
    let _ = std::fs::remove_dir_all(&remote);
    run_in(&r.root, &["clone", "-q", "--bare", ".", remote.to_str().unwrap()]).unwrap();
    r.run(&["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
    r.run(&["fetch", "-q", "origin"]).unwrap();
    r.run(&["branch", "-u", "origin/main"]).unwrap();
    write(&r, "b", "b");
    r.stage_all().unwrap();
    r.commit("c2").unwrap();
    let st = r.status().unwrap();
    assert_eq!((st.branch.as_str(), st.ahead, st.behind), ("main", 1, 0));
    // Push through remote_op, then ahead is 0.
    Repo::remote_op(r.root.clone(), "push").unwrap();
    assert_eq!(r.status().unwrap().ahead, 0);
    r.run(&["reset", "-q", "--hard", "HEAD~1"]).unwrap();
    let st = r.status().unwrap();
    assert_eq!((st.ahead, st.behind), (0, 1));
    // Detached HEAD.
    let head = r.run(&["rev-parse", "HEAD"]).unwrap();
    r.run(&["checkout", "-q", head.trim()]).unwrap();
    let st = r.status().unwrap();
    eprintln!("detached branch label: {:?}; branches(): {:?}", st.branch, r.branches());
    assert!(!r.log().is_empty());
    std::fs::remove_dir_all(&r.root).ok();
    std::fs::remove_dir_all(&remote).ok();
}

#[test]
fn parse_branch_variants() {
    let cases = [
        ("main", ("main", 0, 0)),
        ("main...origin/main", ("main", 0, 0)),
        ("main...origin/main [ahead 5]", ("main", 5, 0)),
        ("main...origin/main [behind 7]", ("main", 0, 7)),
        ("main...origin/main [gone]", ("main", 0, 0)),
        ("HEAD (no branch)", ("HEAD", 0, 0)),
        ("No commits yet on feature/x", ("feature/x", 0, 0)),
    ];
    for (line, (b, a, bh)) in cases {
        let mut st = Status::default();
        parse_branch(line, &mut st);
        assert_eq!((st.branch.as_str(), st.ahead, st.behind), (b, a, bh), "{line}");
    }
}

#[test]
fn line_marks_edge_cases() {
    assert_eq!(line_marks("", ""), vec![]);
    assert_eq!(line_marks("", "a\n"), vec![(0, 1, LineMark::Added)]);
    assert_eq!(line_marks("a\n", ""), vec![(0, 1, LineMark::Deleted)]);
    // Only the trailing newline was removed: git reports a change on the last line.
    eprintln!("no-trailing-newline marks: {:?}", line_marks("a\nb\n", "a\nb"));
    // CRLF base vs CRLF buffer: no change.
    assert_eq!(line_marks("a\r\nb\r\n", "a\r\nb\r\n"), vec![]);
    // Deleted lines at the very end: the mark is on the line after the last line.
    assert_eq!(line_marks("a\nb\nc\n", "a\n"), vec![(1, 2, LineMark::Deleted)]);
}
