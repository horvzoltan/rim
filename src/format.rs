//! External formatters (prettier, rustfmt, …) and PATH lookup.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Resolve a program, preferring a project-local `node_modules/.bin` copy.
pub fn resolve_program(name: &str, root: &Path) -> Option<PathBuf> {
    let local = root.join("node_modules").join(".bin").join(name);
    if local.is_file() {
        return Some(local);
    }
    find_in_path(name)
}

/// A formatter that hangs must not block saving forever.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(20);

/// Like `Child::wait_with_output`, but kills the process if it runs longer than `timeout`.
pub fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<Output, String> {
    fn drain(r: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    }
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output { status, stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() })
}

/// Open a URL or file with the desktop's default handler, without leaving a zombie process.
pub fn open_external(target: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if let Ok(mut child) = Command::new(opener).arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Move a file or folder to the desktop trash (Finder on macOS, `gio` elsewhere).
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    let out = if cfg!(target_os = "macos") {
        let p = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let script = format!("tell application \"Finder\" to delete POSIX file \"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
        Command::new("osascript").args(["-e", &script]).output()
    } else {
        Command::new("gio").arg("trash").arg(path).output()
    };
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(format!("could not move to trash: {e}")),
    }
}

/// Pipe `input` through the formatter command and return its stdout.
pub fn run_formatter(cmd: &[String], file: &Path, root: &Path, input: &str) -> Result<String, String> {
    let (prog, args) = cmd.split_first().ok_or("empty formatter command")?;
    let prog = resolve_program(prog, root).ok_or_else(|| format!("formatter `{prog}` not found"))?;
    let file_str = file.to_string_lossy();
    let args: Vec<String> = args.iter().map(|a| a.replace("{file}", &file_str)).collect();
    let cwd = file.parent().filter(|p| p.is_dir()).unwrap_or(root);
    let mut child = Command::new(&prog)
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", prog.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_owned();
    // Write on a separate thread so a large file can't deadlock on full pipes.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = wait_with_timeout(child, FORMAT_TIMEOUT).map_err(|e| format!("{}: {e}", prog.display()))?;
    let _ = writer.join();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let mut msg = err.lines().filter(|l| !l.trim().is_empty()).take(6).collect::<Vec<_>>().join("\n");
        if msg.is_empty() {
            // Some formatters report errors on stdout, or not at all.
            let stdout = String::from_utf8_lossy(&out.stdout);
            msg = stdout.lines().filter(|l| !l.trim().is_empty()).take(6).collect::<Vec<_>>().join("\n");
        }
        if msg.is_empty() {
            msg = format!("{} exited with {}", prog.display(), out.status);
        }
        return Err(msg);
    }
    String::from_utf8(out.stdout).map_err(|_| "formatter produced invalid UTF-8".into())
}

#[cfg(test)]
#[path = "validate/format_tests.rs"]
pub(crate) mod validate_tests;
