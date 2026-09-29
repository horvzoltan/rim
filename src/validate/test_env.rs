//! Shared setup for tests that touch the config / state directories: points XDG dirs at a
//! scratch folder (never the user's real ~/.config/rim) and serializes such tests.
//! Included from main.rs as `#[cfg(test)] mod test_env`.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, Once};

static INIT: Once = Once::new();
static LOCK: Mutex<()> = Mutex::new(());

pub fn base() -> PathBuf {
    std::env::temp_dir().join(format!("rim-validate-env-{}", std::process::id()))
}

/// Set scratch XDG dirs (once per process) and take the global lock.
pub fn lock() -> MutexGuard<'static, ()> {
    INIT.call_once(|| {
        remove_stale_scratch_dirs();
        let b = base();
        std::fs::create_dir_all(&b).unwrap();
        // SAFETY: done once, before any test in this process reads these variables through `dirs`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", b.join("config"));
            std::env::set_var("XDG_STATE_HOME", b.join("state"));
            std::env::set_var("XDG_CACHE_HOME", b.join("cache"));
            std::env::set_var("XDG_DATA_HOME", b.join("data"));
        }
    });
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cfg = crate::config::Config::path();
    assert!(cfg.starts_with(base()), "refusing to touch a real config: {}", cfg.display());
    let servers = crate::config::servers_dir();
    assert!(servers.starts_with(base()), "refusing to touch real language servers: {}", servers.display());
    g
}

#[allow(dead_code)]
pub fn config_path() -> PathBuf {
    crate::config::Config::path()
}

/// Tests leave scratch folders named `rim-…-<pid>` in the temp dir. Remove those whose test
/// process has finished (no longer in /proc), so repeated runs don't pile them up.
fn remove_stale_scratch_dirs() {
    // Liveness is checked through /proc, which only Linux has.
    if !cfg!(target_os = "linux") {
        return;
    }
    let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else { return };
    let me = std::process::id().to_string();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("rim-") || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(pid) = name.rsplit('-').next().filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) else { continue };
        if pid != me && !std::path::Path::new("/proc").join(pid).exists() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

