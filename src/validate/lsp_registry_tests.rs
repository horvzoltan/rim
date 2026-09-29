//! Validation: the language server registry and which servers run for a file. Included from lsp_registry.rs.

use super::*;
use crate::lsp_install::InstallInfo;

#[test]
fn registry_is_consistent() {
    let mut ids = std::collections::HashSet::new();
    for p in all() {
        assert!(ids.insert(p.id), "duplicate id {}", p.id);
        assert!(!p.command.is_empty(), "{} has no command", p.id);
        assert!(!p.extensions.is_empty() || !p.file_names.is_empty(), "{} matches no files", p.id);
        assert!(p.file_names.iter().all(|f| *f == f.to_lowercase()), "{}: file names are matched lowercase", p.id);
        // Packages sharing a folder come from the same install.
        for o in all().iter().filter(|o| o.folder == p.folder) {
            assert_eq!(p.source.describe(), o.source.describe(), "{} and {} share a folder", p.id, o.id);
        }
        // Init options and settings are valid JSON once placeholders are filled, even with odd paths.
        let (dir, root) = (Path::new("/tmp/we\"ird\\dir"), Path::new("/home/u/my \"proj\""));
        for text in [p.init, p.settings].into_iter().flatten() {
            assert!(json(Some(text), dir, root).is_some(), "{}: bad JSON {text}", p.id);
        }
    }
    for lang in ["Rust", "React", "Angular", "Python", "Go", "Java", "Swift", "TOML", "Dockerfile", "Tailwind CSS"] {
        assert!(all().iter().any(|p| p.languages.contains(lang)), "no package for {lang}");
    }
}

#[test]
fn github_downloads_exist_for_common_systems() {
    for p in all() {
        let Source::GithubRelease { asset, .. } = &p.source else { continue };
        for (os, arch) in [("macos", "aarch64"), ("macos", "x86_64"), ("linux", "x86_64")] {
            assert!(asset(os, arch).is_some(), "{} has no download for {os} {arch}", p.id);
        }
    }
}

#[test]
fn placeholders_expand() {
    let (dir, root) = (Path::new("/srv/ls"), Path::new("/home/u/proj"));
    assert_eq!(expand("{dir}/node_modules,{root}", dir, root), "/srv/ls/node_modules,/home/u/proj");
    assert_eq!(expand("{root_name}", dir, root), "proj");
    assert_eq!(expand("{root_uri}", dir, root), "file:///home/u/proj");
    let h = expand("{root_hash}", dir, root);
    assert_eq!(h.len(), 16);
    assert_ne!(h, expand("{root_hash}", dir, Path::new("/home/u/other")));
    let angular = by_id("angular-language-server").unwrap();
    let s = server_config(angular, Path::new("/bin/ngserver"), dir, root, true);
    assert_eq!(s.command[0], "/bin/ngserver");
    assert!(s.command.contains(&"/home/u/proj,/srv/ls/node_modules".to_string()));
    let ts = server_config(by_id("typescript-language-server").unwrap(), Path::new("/x"), dir, root, true);
    assert_eq!(ts.init_json.unwrap()["tsserver"]["fallbackPath"], "/srv/ls/node_modules/typescript/lib/tsserver.js");
    let ruby = server_config(by_id("ruby-lsp").unwrap(), Path::new("/x"), dir, root, true);
    assert_eq!(ruby.env["GEM_HOME"], "/srv/ls");
}

struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    installed: HashMap<String, InstallInfo>,
    path_bins: Vec<&'static str>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rim-validate-registry-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        Fixture { dir, root, installed: HashMap::new(), path_bins: vec![] }
    }

    /// Pretend rim installed `id`: the programs of every package from that install exist.
    fn install(&mut self, id: &str) {
        let p = by_id(id).unwrap();
        let info = InstallInfo { version: "1.0.0".into(), ..Default::default() };
        for o in all().iter().filter(|o| o.folder == p.folder) {
            let bin = crate::lsp_install::program_path(o, &self.dir.join("servers").join(o.folder), &info);
            std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
            std::fs::write(&bin, "").unwrap();
        }
        self.installed.insert(p.folder.to_string(), info);
    }

    fn servers(&self, cfg: &Config, file: &str) -> Vec<ServerConfig> {
        let on_path = |prog: &str| self.path_bins.contains(&prog).then(|| PathBuf::from("/usr/bin").join(prog));
        let servers_dir = self.dir.join("servers");
        let env = Env { cfg, installed: &self.installed, servers_dir: &servers_dir, root: &self.root, on_path: &on_path };
        let ext = file.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
        servers_for(&env, &ext, file)
    }
}

fn names(s: &[ServerConfig]) -> Vec<&str> {
    s.iter().map(|s| s.name.as_str()).collect()
}

#[test]
fn user_config_beats_installed_beats_path() {
    let mut f = Fixture::new("priority");
    let mut cfg = Config::default();
    // Nothing installed or on PATH: no server, so the status bar can offer to install one
    // (the built-in default entry would only fail to start).
    assert!(f.servers(&cfg, "a.py").is_empty());
    f.path_bins.push("pyright-langserver");
    let s = f.servers(&cfg, "a.py");
    assert_eq!(s[0].command[0], "/usr/bin/pyright-langserver");
    assert!(!s[0].managed);
    f.install("pyright");
    let s = f.servers(&cfg, "a.py");
    assert!(s[0].managed && s[0].command[0].ends_with("node_modules/.bin/pyright-langserver"), "{:?}", s[0].command);
    cfg.language_servers.push(ServerConfig { name: "mine".into(), command: vec!["my-py".into()], extensions: vec!["py".into()], ..Default::default() });
    assert_eq!(names(&f.servers(&cfg, "a.py")), ["mine"]);
    // Disabling works at every level.
    cfg.disabled_language_servers = vec!["mine".into(), "pyright".into()];
    assert!(f.servers(&cfg, "a.py").is_empty());
}

#[test]
fn addons_run_next_to_the_main_server() {
    let mut f = Fixture::new("addons");
    let cfg = Config::default();
    f.install("typescript-language-server");
    f.install("vscode-eslint-language-server");
    f.install("angular-language-server");
    assert_eq!(names(&f.servers(&cfg, "App.tsx")), ["typescript-language-server", "vscode-eslint-language-server"]);
    // The ESLint install is shared with HTML / CSS / JSON.
    assert_eq!(names(&f.servers(&cfg, "index.html")), ["vscode-html-language-server"]);
    assert_eq!(names(&f.servers(&cfg, "a.json")), ["vscode-json-language-server"]);
    // Angular only in projects with an angular.json.
    std::fs::write(f.root.join("angular.json"), "{}").unwrap();
    assert_eq!(
        names(&f.servers(&cfg, "app.component.ts")),
        ["typescript-language-server", "vscode-eslint-language-server", "angular-language-server"]
    );
    // An add-on alone is not a main server.
    let mut only_eslint = Fixture::new("addons-only");
    only_eslint.install("vscode-eslint-language-server");
    let s = only_eslint.servers(&Config { language_servers: vec![], ..Config::default() }, "a.svelte");
    assert_eq!(names(&s), ["vscode-eslint-language-server"]);
}

#[test]
fn files_matched_by_name() {
    let mut f = Fixture::new("names");
    f.install("dockerfile-language-server");
    let cfg = Config::default();
    assert_eq!(names(&f.servers(&cfg, "Dockerfile")), ["dockerfile-language-server"]);
    assert_eq!(names(&f.servers(&cfg, "api.dockerfile")), ["dockerfile-language-server"]);
    assert!(f.servers(&cfg, "Dockerfile.txt").is_empty());
}

