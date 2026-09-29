//! Installs language servers from the registry into rim's own folder (`config::servers_dir()`),
//! one folder per package. Everything goes into a temporary folder first and is moved into place
//! only when the install worked, so a failed install never leaves a half-broken server.
//!
//! Like the rest of rim this shells out to the usual tools (npm, go, gem, dotnet, curl, tar, unzip)
//! instead of bundling a package manager.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::lsp_registry::{Package, Source};

const INFO_FILE: &str = "rim-install.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InstallInfo {
    pub version: String,
    /// The program, relative to the install folder (downloads only; npm & co. use fixed places).
    #[serde(default)]
    pub bin: String,
    #[serde(default)]
    pub installed_at: u64,
}

/// Where to find the tools the installer runs; tests point these at fakes.
pub struct Tools {
    pub github_api: String,
    pub find: Box<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>,
}

impl Default for Tools {
    fn default() -> Self {
        Self { github_api: "https://api.github.com".into(), find: Box::new(crate::format::find_in_path) }
    }
}

impl Tools {
    fn get(&self, name: &str) -> Result<PathBuf, String> {
        (self.find)(name).ok_or_else(|| format!("`{name}` was not found on PATH"))
    }
}

/// Installed packages by folder name.
pub fn load_installed(servers_dir: &Path) -> HashMap<String, InstallInfo> {
    let Ok(rd) = std::fs::read_dir(servers_dir) else { return HashMap::new() };
    rd.flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join(INFO_FILE)).ok()?;
            Some((e.file_name().to_string_lossy().to_string(), serde_json::from_str(&text).ok()?))
        })
        .collect()
}

/// The package's program inside its install folder.
pub fn program_path(p: &Package, dir: &Path, info: &InstallInfo) -> PathBuf {
    match &p.source {
        Source::Npm { .. } => dir.join("node_modules").join(".bin").join(p.program()),
        Source::Go { .. } | Source::Gem { .. } | Source::DotnetTool { .. } => dir.join("bin").join(p.program()),
        Source::GithubRelease { .. } | Source::Url { .. } => dir.join(&info.bin),
        Source::External { .. } => PathBuf::new(),
    }
}

pub fn uninstall(servers_dir: &Path, folder: &str) -> Result<(), String> {
    let dir = servers_dir.join(folder);
    if dir.exists() { std::fs::remove_dir_all(&dir).map_err(|e| e.to_string()) } else { Ok(()) }
}

/// Install (or update: install the latest and replace) `p` into `servers_dir/<folder>`.
/// Progress lines go to `log`.
pub fn install(p: &Package, servers_dir: &Path, tools: &Tools, log: &dyn Fn(String)) -> Result<InstallInfo, String> {
    let dir = servers_dir.join(p.folder);
    let tmp = servers_dir.join(format!(".{}.tmp", p.folder));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("can't create {}: {e}", tmp.display()))?;
    let result = install_into(p, &tmp, tools, log).and_then(|mut info| {
        info.installed_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let json = serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?;
        std::fs::write(tmp.join(INFO_FILE), json).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&tmp, &dir).map_err(|e| format!("can't move the server into place: {e}"))?;
        Ok(info)
    });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

fn install_into(p: &Package, dir: &Path, tools: &Tools, log: &dyn Fn(String)) -> Result<InstallInfo, String> {
    match &p.source {
        Source::Npm { package, extra } => {
            let npm = tools.get("npm")?;
            let mut cmd = Command::new(npm);
            cmd.args(["install", "--prefix"]).arg(dir).args(["--no-audit", "--no-fund", "--loglevel=error", package]).args(*extra);
            run(&mut cmd, &format!("npm install {package}"), log)?;
            let name = npm_name(package);
            let pj = std::fs::read_to_string(dir.join("node_modules").join(name).join("package.json")).unwrap_or_default();
            let version = serde_json::from_str::<serde_json::Value>(&pj).ok().and_then(|v| v["version"].as_str().map(String::from));
            Ok(InstallInfo { version: version.unwrap_or_default(), ..Default::default() })
        }
        Source::Go { module } => {
            let go = tools.get("go")?;
            let bin = dir.join("bin");
            run(Command::new(&go).args(["install", &format!("{module}@latest")]).env("GOBIN", &bin), &format!("go install {module}"), log)?;
            let out = Command::new(&go).args(["version", "-m"]).arg(bin.join(p.program())).output().map_err(|e| e.to_string())?;
            let version = String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.trim().strip_prefix("mod\t").and_then(|r| r.split('\t').nth(1)).map(String::from));
            Ok(InstallInfo { version: version.unwrap_or_default(), ..Default::default() })
        }
        Source::Gem { package } => {
            let gem = tools.get("gem")?;
            let mut cmd = Command::new(gem);
            cmd.args(["install", "--no-document", "--install-dir"]).arg(dir).arg("--bindir").arg(dir.join("bin")).arg(package).env("GEM_HOME", dir);
            run(&mut cmd, &format!("gem install {package}"), log)?;
            let version = std::fs::read_dir(dir.join("specifications")).ok().and_then(|rd| {
                rd.flatten().find_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.strip_prefix(&format!("{package}-"))?.strip_suffix(".gemspec").map(String::from)
                })
            });
            Ok(InstallInfo { version: version.unwrap_or_default(), ..Default::default() })
        }
        Source::DotnetTool { package } => {
            let dotnet = tools.get("dotnet")?;
            let bin = dir.join("bin");
            run(Command::new(dotnet).args(["tool", "install", "--tool-path"]).arg(&bin).arg(package), &format!("dotnet tool install {package}"), log)?;
            let store = bin.join(".store").join(package.to_lowercase());
            let version = std::fs::read_dir(store).ok().and_then(|rd| rd.flatten().next().map(|e| e.file_name().to_string_lossy().to_string()));
            Ok(InstallInfo { version: version.unwrap_or_default(), ..Default::default() })
        }
        Source::GithubRelease { repo, asset } => {
            let pattern = asset(std::env::consts::OS, std::env::consts::ARCH)
                .ok_or_else(|| format!("{} has no download for this system ({} {})", p.name, std::env::consts::OS, std::env::consts::ARCH))?;
            log(format!("Looking up the latest release of {repo}"));
            let (tag, name, url) = latest_asset(tools, repo, pattern)?;
            log(format!("Downloading {name} ({tag})"));
            let bin = download_and_unpack(tools, &url, &name, dir, p.program(), log)?;
            Ok(InstallInfo { version: tag, bin, installed_at: 0 })
        }
        Source::Url { url, version } => {
            let name = url.rsplit('/').next().unwrap_or("download").to_string();
            log(format!("Downloading {name}"));
            let bin = download_and_unpack(tools, url, &name, dir, p.program(), log)?;
            Ok(InstallInfo { version: version.to_string(), bin, installed_at: 0 })
        }
        Source::External { hint } => Err(hint.to_string()),
    }
}

/// `@vue/language-server@2` → `@vue/language-server`.
fn npm_name(spec: &str) -> &str {
    match spec[1..].find('@') {
        Some(i) => &spec[..i + 1],
        None => spec,
    }
}

/// Newest version available, when it can be looked up cheaply (npm, GitHub).
pub fn latest_version(p: &Package, tools: &Tools) -> Result<Option<String>, String> {
    match &p.source {
        Source::Npm { package, .. } => {
            let out = Command::new(tools.get("npm")?).args(["view", package, "version"]).output().map_err(|e| e.to_string())?;
            // A range (`pkg@2`) lists every match as `pkg@x.y.z 'x.y.z'`; the last is the newest.
            let text = String::from_utf8_lossy(&out.stdout);
            let last = text.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("");
            Ok(Some(last.rsplit(' ').next().unwrap_or("").trim_matches('\'').to_string()).filter(|v| !v.is_empty()))
        }
        Source::GithubRelease { repo, .. } => Ok(Some(latest_release(tools, repo)?.0)),
        _ => Ok(None),
    }
}

fn latest_release(tools: &Tools, repo: &str) -> Result<(String, serde_json::Value), String> {
    let url = format!("{}/repos/{repo}/releases/latest", tools.github_api);
    let out = Command::new(tools.get("curl")?)
        .args(["-fsSL", "-H", "Accept: application/vnd.github+json", &url])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("couldn't reach GitHub: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    let tag = v["tag_name"].as_str().unwrap_or("").to_string();
    Ok((tag, v))
}

/// (tag, asset name, download URL) of the latest release asset matching `pattern`.
fn latest_asset(tools: &Tools, repo: &str, pattern: &str) -> Result<(String, String, String), String> {
    let (tag, v) = latest_release(tools, repo)?;
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    assets
        .iter()
        .find_map(|a| {
            let name = a["name"].as_str()?;
            let url = a["browser_download_url"].as_str()?;
            glob(pattern, name).then(|| (tag.clone(), name.to_string(), url.to_string()))
        })
        .ok_or_else(|| format!("release {tag} of {repo} has no file matching {pattern}"))
}

/// `*` matches any run of characters.
pub fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name.ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Download `url` and unpack it into `dir`. Returns the path of `program` inside, relative to `dir`.
fn download_and_unpack(tools: &Tools, url: &str, name: &str, dir: &Path, program: &str, log: &dyn Fn(String)) -> Result<String, String> {
    let dl = dir.join(".download");
    std::fs::create_dir_all(&dl).map_err(|e| e.to_string())?;
    let file = dl.join(name);
    run(Command::new(tools.get("curl")?).args(["-fsSL", "--retry", "2", "-o"]).arg(&file).arg(url), "download", log)?;
    log(format!("Unpacking {name}"));
    let lower = name.to_lowercase();
    let is_tar = [".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tar"].iter().any(|s| lower.ends_with(s));
    if is_tar {
        run(Command::new(tools.get("tar")?).arg("-xf").arg(&file).arg("-C").arg(dir), "tar", log)?;
    } else if lower.ends_with(".zip") {
        unzip(tools, &file, dir, log)?;
    } else if lower.ends_with(".gz") {
        let out = std::fs::File::create(dir.join(program)).map_err(|e| e.to_string())?;
        let st = Command::new(tools.get("gunzip")?).arg("-c").arg(&file).stdout(out).status().map_err(|e| e.to_string())?;
        if !st.success() {
            return Err(format!("gunzip failed on {name}"));
        }
    } else {
        std::fs::copy(&file, dir.join(program)).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_dir_all(&dl);
    let bin = find_file(dir, program).ok_or_else(|| format!("{program} not found in {name}"))?;
    make_executable(&bin, dir);
    Ok(bin.strip_prefix(dir).unwrap_or(&bin).to_string_lossy().to_string())
}

/// `unzip`, else bsdtar (macOS `tar` reads zips), else Python's zipfile module.
fn unzip(tools: &Tools, file: &Path, dir: &Path, log: &dyn Fn(String)) -> Result<(), String> {
    if let Some(u) = (tools.find)("unzip") {
        return run(Command::new(u).args(["-q", "-o"]).arg(file).arg("-d").arg(dir), "unzip", log);
    }
    if cfg!(target_os = "macos")
        && let Some(t) = (tools.find)("tar")
    {
        return run(Command::new(t).arg("-xf").arg(file).arg("-C").arg(dir), "tar", log);
    }
    let py = tools.get("python3").map_err(|_| "unpacking a .zip needs `unzip` (or python3)".to_string())?;
    run(Command::new(py).args(["-m", "zipfile", "-e"]).arg(file).arg(dir), "unzip", log)
}

/// The shallowest file called `name` under `dir`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut level = vec![dir.to_path_buf()];
    while !level.is_empty() {
        let mut next = Vec::new();
        for d in level {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let p = e.path();
                if p.is_dir() {
                    next.push(p);
                } else if e.file_name() == name {
                    return Some(p);
                }
            }
        }
        level = next;
    }
    None
}

/// Downloads and unzipped files may lose their executable bit: restore it on the program, on
/// scripts and on everything in `bin` folders.
fn make_executable(program: &Path, dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &Path| {
            if let Ok(m) = std::fs::metadata(p) {
                let mut perms = m.permissions();
                perms.set_mode(perms.mode() | 0o755);
                let _ = std::fs::set_permissions(p, perms);
            }
        };
        set(program);
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            let in_bin = d.file_name().is_some_and(|n| n == "bin");
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if in_bin || p.extension().is_some_and(|x| x == "sh") {
                    set(&p);
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (program, dir);
}

/// Run a command, passing its output to `log` line by line. On failure the error has the last lines.
fn run(cmd: &mut Command, what: &str, log: &dyn Fn(String)) -> Result<(), String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{what}: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for r in [child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|s| Box::new(s) as _)]
        .into_iter()
        .flatten()
    {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx);
    let mut tail: Vec<String> = Vec::new();
    for line in rx {
        if line.trim().is_empty() {
            continue;
        }
        log(line.clone());
        tail.push(line);
        if tail.len() > 6 {
            tail.remove(0);
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        let code = status.code().map(|c| format!(" (exit code {c})")).unwrap_or_default();
        Err(format!("{what} failed{code}\n{}", tail.join("\n")).trim_end().to_string())
    }
}

#[cfg(test)]
#[path = "validate/lsp_install_tests.rs"]
mod validate_tests;
