//! Validation: installing language servers with fake downloads (file:// URLs through the real
//! curl / tar / gunzip) and a fake npm. Included from lsp_install.rs.

use super::*;
use crate::lsp_registry::{Group, Package};
use std::os::unix::fs::PermissionsExt;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-install-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn test_pkg(source: Source, program: &'static str) -> Package {
    Package {
        id: "test-ls",
        name: "Test LS",
        languages: "Test",
        group: Group::More,
        extensions: &["tst"],
        file_names: &[],
        command: Box::leak(Box::new([program])),
        source,
        folder: "test-ls",
        requires: &[],
        addon: false,
        marker: None,
        init: None,
        settings: None,
        env: &[],
        description: "",
        homepage: "",
    }
}

/// A fake GitHub API answering `releases/latest` for `o/r` with the given asset file.
fn fake_github(base: &Path, asset: &Path) -> Tools {
    let api = base.join("api");
    let latest = api.join("repos/o/r/releases/latest");
    std::fs::create_dir_all(latest.parent().unwrap()).unwrap();
    let name = asset.file_name().unwrap().to_string_lossy();
    let json = serde_json::json!({
        "tag_name": "v1.2.3",
        "assets": [
            { "name": "unrelated.txt", "browser_download_url": "file:///nonexistent" },
            { "name": name, "browser_download_url": format!("file://{}", asset.display()) },
        ],
    });
    std::fs::write(&latest, json.to_string()).unwrap();
    Tools { github_api: format!("file://{}", api.display()), ..Default::default() }
}

fn run_install(p: &Package, servers: &Path, tools: &Tools) -> (Result<InstallInfo, String>, Vec<String>) {
    let log = std::sync::Mutex::new(Vec::new());
    let r = install(p, servers, tools, &|l| log.lock().unwrap().push(l));
    (r, log.into_inner().unwrap())
}

#[test]
fn installs_a_tarball_from_a_github_release() {
    let d = scratch("tar");
    let src = d.join("src/tool-1.2.3/bin/mytool");
    script(&src, "echo hi");
    let tarball = d.join("tool-1.2.3-any.tar.gz");
    let st = Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(d.join("src")).arg("tool-1.2.3").status().unwrap();
    assert!(st.success());
    let tools = fake_github(&d, &tarball);
    let p = test_pkg(Source::GithubRelease { repo: "o/r", asset: |_, _| Some("tool-*-any.tar.gz") }, "mytool");
    let servers = d.join("servers");
    let (r, log) = run_install(&p, &servers, &tools);
    let info = r.unwrap();
    assert_eq!(info.version, "v1.2.3");
    assert_eq!(info.bin, "tool-1.2.3/bin/mytool");
    assert!(log.iter().any(|l| l.contains("Downloading tool-1.2.3-any.tar.gz")), "{log:?}");
    let dir = servers.join("test-ls");
    let bin = program_path(&p, &dir, &info);
    assert!(bin.is_file());
    assert_eq!(load_installed(&servers).get("test-ls"), Some(&info));
    // Only the install folder is left: no temp folder, no download.
    let names: Vec<_> = std::fs::read_dir(&servers).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(names, ["test-ls"]);
    assert!(!dir.join(".download").exists());
    uninstall(&servers, "test-ls").unwrap();
    assert!(load_installed(&servers).is_empty());
}

#[test]
fn installs_a_gzipped_binary_and_makes_it_executable() {
    let d = scratch("gz");
    let raw = d.join("taplo-any");
    std::fs::write(&raw, "#!/bin/sh\necho taplo\n").unwrap();
    let st = Command::new("gzip").arg("-k").arg(&raw).status().unwrap();
    assert!(st.success());
    let tools = fake_github(&d, &d.join("taplo-any.gz"));
    let p = test_pkg(Source::GithubRelease { repo: "o/r", asset: |_, _| Some("taplo-any.gz") }, "taplo");
    let (r, _) = run_install(&p, &d.join("servers"), &tools);
    let info = r.unwrap();
    assert_eq!(info.bin, "taplo");
    let bin = d.join("servers/test-ls/taplo");
    assert_eq!(std::fs::read_to_string(&bin).unwrap(), "#!/bin/sh\necho taplo\n");
    assert!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o111 != 0);
    assert_eq!(Command::new(&bin).output().unwrap().stdout, b"taplo\n");
}

#[test]
fn failed_install_leaves_nothing_behind_and_keeps_the_old_copy() {
    let d = scratch("fail");
    let asset = d.join("a.tar.gz");
    std::fs::write(&asset, "x").unwrap();
    let tools = fake_github(&d, &asset);
    let servers = d.join("servers");
    // Existing install that must survive a failed update.
    std::fs::create_dir_all(servers.join("test-ls")).unwrap();
    std::fs::write(servers.join("test-ls/rim-install.json"), r#"{"version":"old"}"#).unwrap();
    let no_match = test_pkg(Source::GithubRelease { repo: "o/r", asset: |_, _| Some("nope-*.zip") }, "x");
    let (r, _) = run_install(&no_match, &servers, &tools);
    assert!(r.unwrap_err().contains("no file matching nope-*.zip"));
    let broken = test_pkg(Source::GithubRelease { repo: "o/r", asset: |_, _| Some("a.tar.gz") }, "x");
    let (r, _) = run_install(&broken, &servers, &tools);
    assert!(r.is_err());
    let names: Vec<_> = std::fs::read_dir(&servers).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(names, ["test-ls"]);
    assert_eq!(load_installed(&servers)["test-ls"].version, "old");
    let unavailable = test_pkg(Source::GithubRelease { repo: "o/r", asset: |_, _| None }, "x");
    assert!(run_install(&unavailable, &servers, &tools).0.unwrap_err().contains("no download for this system"));
}

#[test]
fn npm_install_goes_into_the_package_folder() {
    let d = scratch("npm");
    // A fake npm: `npm install --prefix DIR … PKG EXTRA` creates the package and its program.
    let npm = d.join("bin/npm");
    script(
        &npm,
        r#"prefix=""; pkgs=""
while [ $# -gt 0 ]; do case "$1" in --prefix) prefix="$2"; shift;; --*|install) ;; *) pkgs="$pkgs $1";; esac; shift; done
echo "added $pkgs"
for p in $pkgs; do mkdir -p "$prefix/node_modules/$p"; echo '{"version":"4.5.6"}' > "$prefix/node_modules/$p/package.json"; done
mkdir -p "$prefix/node_modules/.bin"; printf '#!/bin/sh\n' > "$prefix/node_modules/.bin/fake-ls"; chmod +x "$prefix/node_modules/.bin/fake-ls""#,
    );
    let npm_path = npm.clone();
    let tools = Tools { find: Box::new(move |n| (n == "npm").then(|| npm_path.clone())), ..Default::default() };
    let p = test_pkg(Source::Npm { package: "fake-ls", extra: &["typescript"] }, "fake-ls");
    let servers = d.join("servers");
    let (r, log) = run_install(&p, &servers, &tools);
    let info = r.unwrap();
    assert_eq!(info.version, "4.5.6");
    assert_eq!(log, ["added  fake-ls typescript"]);
    assert!(program_path(&p, &servers.join("test-ls"), &info).is_file());
    assert!(servers.join("test-ls/node_modules/typescript/package.json").is_file());
    // Without npm: a clear message.
    let none = Tools { find: Box::new(|_| None), ..Default::default() };
    assert!(run_install(&p, &servers, &none).0.unwrap_err().contains("`npm` was not found"));
}

#[test]
fn npm_names_and_globs() {
    assert_eq!(npm_name("@vue/language-server@2"), "@vue/language-server");
    assert_eq!(npm_name("@angular/language-server"), "@angular/language-server");
    assert_eq!(npm_name("pyright"), "pyright");
    assert!(glob("clangd-mac-*.zip", "clangd-mac-23.1.0.zip"));
    assert!(!glob("clangd-mac-*.zip", "clangd_indexing_tools-mac-23.1.0.zip"));
    assert!(glob("lua-language-server-*-darwin-arm64.tar.gz", "lua-language-server-3.19.1-darwin-arm64.tar.gz"));
    assert!(!glob("lua-language-server-*-darwin-arm64.tar.gz", "lua-language-server-3.19.1-darwin-x64.tar.gz"));
    assert!(glob("server.zip", "server.zip") && !glob("server.zip", "server.zip.sha256"));
    assert!(!glob("a*a", "a"));
}

/// Downloads real servers (network!) and checks each one starts and answers `initialize`.
/// `RIM_INSTALL_TEST=taplo,pyright cargo test installs_and_starts_real_servers -- --ignored --nocapture`
#[test]
#[ignore]
fn installs_and_starts_real_servers() {
    let ids = std::env::var("RIM_INSTALL_TEST").unwrap_or_else(|_| "taplo,marksman,pyright".into());
    let d = scratch("real");
    let (servers, root) = (d.join("servers"), d.join("proj"));
    std::fs::create_dir_all(&root).unwrap();
    for id in ids.split(',') {
        let p = crate::lsp_registry::by_id(id).unwrap_or_else(|| panic!("unknown id {id}"));
        let start = std::time::Instant::now();
        let info = install(p, &servers, &Tools::default(), &|l| eprintln!("  [{id}] {l}")).unwrap_or_else(|e| panic!("{id}: {e}"));
        let dir = servers.join(p.folder);
        let bin = program_path(p, &dir, &info);
        let size: u64 = walk_size(&dir);
        eprintln!("{id}: installed {} in {:.1}s, {} MB, {}", info.version, start.elapsed().as_secs_f32(), size / 1_000_000, bin.display());
        let cfg = crate::lsp_registry::server_config(p, &bin, &dir, &root, true);
        let mut c = crate::lsp::Client::start_with(
            &cfg.command,
            &root,
            cfg.init_json.unwrap_or(serde_json::json!({})),
            cfg.settings.unwrap_or_default(),
            &cfg.env,
            egui::Context::default(),
        )
        .unwrap_or_else(|e| panic!("{id}: {e}"));
        let t = std::time::Instant::now();
        while !c.ready {
            for e in c.poll() {
                match e {
                    crate::lsp::LspEvent::InitFailed(m) => panic!("{id}: initialize failed: {m}"),
                    crate::lsp::LspEvent::Exited => panic!("{id}: exited"),
                    _ => {}
                }
            }
            assert!(t.elapsed().as_secs() < 90, "{id}: no answer to initialize");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        eprintln!("{id}: answered initialize after {:.1}s", t.elapsed().as_secs_f32());
    }
    std::fs::remove_dir_all(d).ok();
}

fn walk_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| if e.path().is_dir() { walk_size(&e.path()) } else { e.metadata().map(|m| m.len()).unwrap_or(0) }).sum())
        .unwrap_or(0)
}

/// Asks npm and GitHub for the newest versions (network!).
#[test]
#[ignore]
fn looks_up_latest_versions() {
    for id in ["taplo", "pyright", "vue-language-server", "gopls"] {
        let p = crate::lsp_registry::by_id(id).unwrap();
        let v = latest_version(p, &Tools::default()).unwrap();
        eprintln!("{id}: {v:?}");
        match id {
            "gopls" => assert_eq!(v, None, "not looked up"),
            "vue-language-server" => assert!(v.as_deref().is_some_and(|v| v.starts_with("2.")), "{v:?}"),
            _ => assert!(v.is_some_and(|v| !v.is_empty())),
        }
    }
}
