//! Validation: external formatter pipeline with fake formatter scripts. Included from format.rs.

use super::*;

pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-fmt-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn formats_via_stdin_stdout_with_file_placeholder() {
    let d = scratch("ok");
    // Upper-cases stdin and appends the {file} argument it was given, plus its cwd.
    let f = script(&d, "upfmt", "#!/bin/sh\ntr a-z A-Z\necho \"file=$1 cwd=$(pwd)\"\n");
    std::fs::create_dir_all(d.join("sub")).unwrap();
    let file = d.join("sub/x ü.rs");
    let out = run_formatter(&s(&[f.to_str().unwrap(), "{file}"]), &file, &d, "héllo\n").unwrap();
    assert_eq!(out, format!("HéLLO\nfile={} cwd={}\n", file.display(), d.join("sub").display()));
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn failure_reports_stderr() {
    let d = scratch("fail");
    let f = script(&d, "bad", "#!/bin/sh\ncat >/dev/null\necho 'line 1: syntax error' >&2\necho second >&2\nexit 3\n");
    let e = run_formatter(&s(&[f.to_str().unwrap()]), &d.join("a.rs"), &d, "x").unwrap_err();
    assert_eq!(e, "line 1: syntax error\nsecond");
    // Failure without stderr output still produces some message.
    let f = script(&d, "silent", "#!/bin/sh\nexit 1\n");
    let e = run_formatter(&s(&[f.to_str().unwrap()]), &d.join("a.rs"), &d, "x").unwrap_err();
    eprintln!("silent failure message: {e:?}");
    assert!(!e.trim().is_empty(), "a formatter that fails silently yields an empty error message");
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn non_utf8_output_is_an_error() {
    let d = scratch("utf8");
    let f = script(&d, "latin1", "#!/bin/sh\ncat >/dev/null\nprintf '\\377\\376abc'\n");
    let e = run_formatter(&s(&[f.to_str().unwrap()]), &d.join("a.rs"), &d, "x").unwrap_err();
    assert_eq!(e, "formatter produced invalid UTF-8");
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn missing_program_and_empty_command() {
    let d = scratch("missing");
    let e = run_formatter(&s(&["no-such-formatter-xyz"]), &d.join("a.rs"), &d, "x").unwrap_err();
    assert!(e.contains("not found"), "{e}");
    assert!(run_formatter(&[], &d.join("a.rs"), &d, "x").is_err());
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn large_input_does_not_deadlock() {
    let d = scratch("big");
    let f = script(&d, "catfmt", "#!/bin/sh\ncat\n");
    let input = "x".repeat(8 * 1024 * 1024);
    let out = run_formatter(&s(&[f.to_str().unwrap()]), &d.join("a.rs"), &d, &input).unwrap();
    assert_eq!(out.len(), input.len());
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn formatter_that_does_not_read_stdin() {
    // A formatter that exits without reading stdin: the writer thread gets EPIPE; must not hang or panic.
    let d = scratch("noread");
    let f = script(&d, "noread", "#!/bin/sh\necho formatted\n");
    let input = "x".repeat(4 * 1024 * 1024);
    let out = run_formatter(&s(&[f.to_str().unwrap()]), &d.join("a.rs"), &d, &input).unwrap();
    assert_eq!(out, "formatted\n");
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn node_modules_bin_is_preferred() {
    let d = scratch("nm");
    script(&d.join("node_modules/.bin"), "prettier", "#!/bin/sh\necho local\n");
    assert_eq!(resolve_program("prettier", &d), Some(d.join("node_modules/.bin/prettier")));
    let out = run_formatter(&s(&["prettier", "--stdin-filepath", "{file}"]), &d.join("a.js"), &d, "x").unwrap();
    assert_eq!(out, "local\n");
    std::fs::remove_dir_all(d).ok();
}
