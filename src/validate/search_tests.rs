//! Validation: quick-open listing and project search, honoring .gitignore. Included from search.rs.

use super::*;

fn tree(name: &str, git: bool) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-search-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    if git {
        std::process::Command::new("git").arg("-C").arg(&d).args(["init", "-q"]).status().unwrap();
    }
    let w = |rel: &str, s: &[u8]| {
        let p = d.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    };
    w(".gitignore", b"target/\n*.log\n");
    w("src/main.rs", b"fn main() {\n    println!(\"Hello NEEDLE\");\n}\n");
    w("src/lib.rs", b"// needle in lib\npub fn needle() {}\n");
    w("target/debug/out.rs", b"needle in build output\n");
    w("app.log", b"needle in a log\n");
    w(".hidden/conf", b"needle hidden\n");
    w("bin.dat", b"\x00\x01needle\xff\xfe");
    w("uni/ünï.txt", "Ünïcode NEEDLE 😀 needle\n".as_bytes());
    d
}

fn rels(root: &Path) -> Vec<String> {
    let mut v = list_files(root, 10_000);
    v.sort();
    v
}

#[test]
fn quick_open_respects_gitignore_in_a_repo() {
    let d = tree("qo", true);
    let files = rels(&d);
    assert!(files.contains(&"src/main.rs".to_string()));
    assert!(files.contains(&".hidden/conf".to_string()), "hidden files are listed");
    assert!(files.contains(&"uni/ünï.txt".to_string()));
    assert!(!files.iter().any(|f| f.starts_with("target/")), "{files:?}");
    assert!(!files.contains(&"app.log".to_string()));
    assert!(!files.iter().any(|f| f.starts_with(".git/")));
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn quick_open_respects_gitignore_outside_a_repo() {
    // A folder with a .gitignore that is not (yet) a git repository.
    let d = tree("qo-nogit", false);
    let files = rels(&d);
    assert!(!files.iter().any(|f| f.starts_with("target/")), ".gitignore ignored outside a git repo: {files:?}");
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn project_search_matches_and_ignores() {
    let d = tree("ps", true);
    let (res, trunc) = search(&d, "needle", false, 2000);
    assert!(!trunc);
    let mut found: Vec<(String, usize, usize)> = res.iter().flat_map(|r| r.matches.iter().map(move |m| (r.rel.clone(), m.line, m.col))).collect();
    found.sort();
    eprintln!("found: {found:?}");
    assert!(found.contains(&("src/main.rs".into(), 1, 20)));
    assert!(found.contains(&("src/lib.rs".into(), 0, 3)));
    assert!(found.contains(&("src/lib.rs".into(), 1, 7)));
    assert!(found.contains(&("uni/ünï.txt".into(), 0, 8)), "char column after non-ASCII chars");
    assert!(!found.iter().any(|f| f.0.starts_with("target") || f.0 == "app.log"));
    assert!(!found.iter().any(|f| f.0 == "bin.dat"), "binary (non-UTF-8) files are skipped");
    // Case-sensitive.
    let (res, _) = search(&d, "NEEDLE", true, 2000);
    let n: usize = res.iter().map(|r| r.matches.len()).sum();
    assert_eq!(n, 2);
    // Empty query.
    assert!(search(&d, "", false, 10).0.is_empty());
    // Limit.
    let (res, trunc) = search(&d, "needle", false, 2);
    assert!(trunc);
    assert_eq!(res.iter().map(|r| r.matches.len()).sum::<usize>(), 2);
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn search_line_with_length_changing_lowercase() {
    // A line containing 'İ' is skipped entirely in case-insensitive mode.
    let d = std::env::temp_dir().join(format!("rim-validate-search-dotted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("t.txt"), "İstanbul needle\n").unwrap();
    let (res, _) = search(&d, "needle", false, 10);
    assert_eq!(res.len(), 1, "case-insensitive search misses lines containing chars whose lowercase has a different byte length");
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn search_multiple_matches_on_one_line() {
    let d = std::env::temp_dir().join(format!("rim-validate-search-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("t.txt"), "a x a x a\n").unwrap();
    let (res, _) = search(&d, "a", false, 10);
    eprintln!("matches on a line with 3 hits: {}", res[0].matches.len());
    std::fs::remove_dir_all(d).ok();
}
