mod app;
mod chat;
mod claude;
mod config;
mod dap;
mod debug;
mod editor;
mod file_icons;
mod format;
mod git;
mod graph;
mod highlight;
mod lsp;
mod lsp_install;
mod lsp_registry;
mod md;
mod palette;
mod search;
mod text;
mod theme;
mod tree;
mod vim;
#[cfg(test)]
#[path = "validate/test_env.rs"]
mod test_env;

use std::path::PathBuf;

/// Apps started from the macOS Dock / Finder or a Linux desktop launcher get a minimal `PATH`,
/// so language servers, formatters and the tools they need (`node`, `go`, …) aren't found.
/// When rim wasn't started from a terminal, take `PATH` from the user's login shell.
fn use_login_shell_path() {
    if std::env::var_os("TERM").is_some() {
        return;
    }
    let Some(shell) = std::env::var_os("SHELL") else { return };
    let child = std::process::Command::new(shell)
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(out) = child.map_err(|e| e.to_string()).and_then(|c| format::wait_with_timeout(c, std::time::Duration::from_secs(3))) else {
        return;
    };
    let shell_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || shell_path.is_empty() {
        return;
    }
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&shell_path).collect();
    for d in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        // SAFETY: runs first thing in main, before any other thread exists.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

fn main() -> eframe::Result {
    use_login_shell_path();
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([480.0, 320.0])
            .with_title("rim")
            .with_app_id("rim"),
        ..Default::default()
    };
    eframe::run_native("rim", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, arg)))))
}

