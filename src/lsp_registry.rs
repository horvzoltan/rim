//! Language servers rim can install (Settings → Language Servers), and which server runs for a file.
//!
//! A server for a file is picked in this order: a `[[language_servers]]` entry the user wrote in
//! config.toml, then a server installed by rim, then a known server found on `PATH`, then the
//! built-in defaults. Add-on servers (ESLint, Tailwind, Angular) run next to that one server and
//! only contribute diagnostics.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use crate::config::{Config, ServerConfig};
use crate::lsp_install::InstallInfo;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Popular,
    Web,
    ConfigDocs,
    More,
}

impl Group {
    pub const ALL: [Group; 4] = [Group::Popular, Group::Web, Group::ConfigDocs, Group::More];

    pub fn label(self) -> &'static str {
        match self {
            Group::Popular => "Popular",
            Group::Web => "Web",
            Group::ConfigDocs => "Config & docs",
            Group::More => "More languages",
        }
    }
}

pub enum Source {
    /// `npm install --prefix <dir> <package> <extra…>`; programs in `node_modules/.bin`.
    Npm { package: &'static str, extra: &'static [&'static str] },
    /// `GOBIN=<dir>/bin go install <module>@latest`.
    Go { module: &'static str },
    /// `gem install --install-dir <dir> <package>`; programs in `bin`.
    Gem { package: &'static str },
    /// `dotnet tool install --tool-path <dir>/bin <package>`.
    DotnetTool { package: &'static str },
    /// The latest GitHub release asset whose name matches the pattern for this OS / CPU (`*` matches
    /// anything). Archives are unpacked; the program is found by name inside.
    GithubRelease { repo: &'static str, asset: fn(&str, &str) -> Option<&'static str> },
    /// A fixed download URL (an archive), for servers not released on GitHub.
    Url { url: &'static str, version: &'static str },
    /// Comes with another toolchain; rim can only find it.
    External { hint: &'static str },
}

impl Source {
    /// The program needed to install from this source, with a hint for getting it.
    pub fn installer(&self) -> Option<(&'static str, &'static str)> {
        match self {
            Source::Npm { .. } => Some(("npm", "Needs Node.js (nodejs.org)")),
            Source::Go { .. } => Some(("go", "Needs Go (go.dev/dl)")),
            Source::Gem { .. } => Some(("gem", "Needs Ruby (ruby-lang.org)")),
            Source::DotnetTool { .. } => Some(("dotnet", "Needs the .NET SDK (dot.net)")),
            Source::GithubRelease { .. } | Source::Url { .. } => Some(("curl", "Needs curl")),
            Source::External { .. } => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Source::Npm { package, .. } => format!("npm package {package}"),
            Source::Go { module } => format!("go install {module}"),
            Source::Gem { package } => format!("Ruby gem {package}"),
            Source::DotnetTool { package } => format!(".NET tool {package}"),
            Source::GithubRelease { repo, .. } => format!("GitHub release of {repo}"),
            Source::Url { url, .. } => format!("download from {url}"),
            Source::External { hint } => hint.to_string(),
        }
    }
}

pub struct Package {
    /// Server name, as used in config.toml, the status bar and `disabled_language_servers`.
    pub id: &'static str,
    pub name: &'static str,
    pub languages: &'static str,
    pub group: Group,
    pub extensions: &'static [&'static str],
    /// Matched by exact (lowercase) file name, for files like `Dockerfile`.
    pub file_names: &'static [&'static str],
    /// Program and arguments. Placeholders: `{dir}` (install folder), `{root}`, `{root_uri}`,
    /// `{root_name}`, `{root_hash}`.
    pub command: &'static [&'static str],
    pub source: Source,
    /// Install folder name; packages that come from one install share it.
    pub folder: &'static str,
    /// Programs the server needs at runtime, as (program, what to install).
    pub requires: &'static [(&'static str, &'static str)],
    /// Runs next to the file's main server and only adds diagnostics.
    pub addon: bool,
    /// Only used in projects that have this file at the root.
    pub marker: Option<&'static str>,
    /// `initializationOptions` and `workspace/configuration` answers, as JSON with placeholders.
    pub init: Option<&'static str>,
    pub settings: Option<&'static str>,
    pub env: &'static [(&'static str, &'static str)],
    pub description: &'static str,
    pub homepage: &'static str,
}

impl Package {
    pub fn program(&self) -> &'static str {
        self.command[0]
    }

    fn matches(&self, ext: &str, file_name: &str) -> bool {
        (!ext.is_empty() && self.extensions.contains(&ext)) || self.file_names.contains(&file_name)
    }

    /// An extension (or file name) of this language, for its icon.
    pub fn sample_file(&self) -> String {
        match (self.extensions.first(), self.file_names.first()) {
            (Some(e), _) => format!("x.{e}"),
            (None, Some(f)) => f.to_string(),
            _ => String::new(),
        }
    }
}

const fn pkg(id: &'static str, name: &'static str, languages: &'static str, group: Group) -> Package {
    Package {
        id,
        name,
        languages,
        group,
        extensions: &[],
        file_names: &[],
        command: &[],
        source: Source::External { hint: "" },
        folder: id,
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

const NODE: &[(&str, &str)] = &[("node", "Node.js")];

pub fn all() -> &'static [Package] {
    static ALL: OnceLock<Vec<Package>> = OnceLock::new();
    ALL.get_or_init(packages)
}

#[cfg(test)]
pub fn by_id(id: &str) -> Option<&'static Package> {
    all().iter().find(|p| p.id == id)
}

fn packages() -> Vec<Package> {
    use Group::*;
    let npm = |package, extra| Source::Npm { package, extra };
    // The servers that bundle TypeScript need its JavaScript API (`tsserver.js`), which
    // TypeScript 7 (the native port) no longer has.
    vec![
        // ---------------- popular ----------------
        Package {
            extensions: &["rs"],
            command: &["rust-analyzer"],
            source: Source::GithubRelease {
                repo: "rust-lang/rust-analyzer",
                asset: |os, arch| match (os, arch) {
                    ("macos", "aarch64") => Some("rust-analyzer-aarch64-apple-darwin.gz"),
                    ("macos", "x86_64") => Some("rust-analyzer-x86_64-apple-darwin.gz"),
                    ("linux", "aarch64") => Some("rust-analyzer-aarch64-unknown-linux-gnu.gz"),
                    ("linux", "x86_64") => Some("rust-analyzer-x86_64-unknown-linux-gnu.gz"),
                    _ => None,
                },
            },
            description: "The official Rust language server.",
            homepage: "https://rust-analyzer.github.io",
            ..pkg("rust-analyzer", "rust-analyzer", "Rust", Popular)
        },
        Package {
            extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
            command: &["typescript-language-server", "--stdio"],
            source: npm("typescript-language-server", &["typescript@6"]),
            requires: NODE,
            init: Some(r#"{"tsserver":{"fallbackPath":"{dir}/node_modules/typescript/lib/tsserver.js"}}"#),
            description: "TypeScript and JavaScript, including React (JSX/TSX). Uses the project's own TypeScript when it has one.",
            homepage: "https://github.com/typescript-language-server/typescript-language-server",
            ..pkg("typescript-language-server", "TypeScript Language Server", "TypeScript, JavaScript, React", Popular)
        },
        Package {
            extensions: &["py", "pyi"],
            command: &["pyright-langserver", "--stdio"],
            source: npm("pyright", &[]),
            requires: NODE,
            settings: Some(r#"{"python":{"analysis":{"autoSearchPaths":true,"useLibraryCodeForTypes":true,"diagnosticMode":"openFilesOnly"}}}"#),
            description: "Fast type checker and language server for Python, from Microsoft.",
            homepage: "https://github.com/microsoft/pyright",
            ..pkg("pyright", "Pyright", "Python", Popular)
        },
        Package {
            extensions: &["c", "h", "cpp", "hpp", "cc", "cxx", "hh", "hxx", "m", "mm"],
            command: &["clangd"],
            source: Source::GithubRelease {
                repo: "clangd/clangd",
                asset: |os, arch| match (os, arch) {
                    ("macos", _) => Some("clangd-mac-*.zip"),
                    ("linux", "x86_64") => Some("clangd-linux-*.zip"),
                    _ => None,
                },
            },
            description: "C, C++ and Objective-C, from the LLVM project. Reads compile_commands.json for build flags.",
            homepage: "https://clangd.llvm.org",
            ..pkg("clangd", "clangd", "C, C++", Popular)
        },
        Package {
            extensions: &["go"],
            file_names: &["go.mod", "go.sum", "go.work"],
            command: &["gopls"],
            source: Source::Go { module: "golang.org/x/tools/gopls" },
            requires: &[("go", "Go")],
            description: "The official Go language server.",
            homepage: "https://go.dev/gopls",
            ..pkg("gopls", "gopls", "Go", Popular)
        },
        Package {
            extensions: &["lua"],
            command: &["lua-language-server"],
            source: Source::GithubRelease {
                repo: "LuaLS/lua-language-server",
                asset: |os, arch| match (os, arch) {
                    ("macos", "aarch64") => Some("lua-language-server-*-darwin-arm64.tar.gz"),
                    ("macos", "x86_64") => Some("lua-language-server-*-darwin-x64.tar.gz"),
                    ("linux", "aarch64") => Some("lua-language-server-*-linux-arm64.tar.gz"),
                    ("linux", "x86_64") => Some("lua-language-server-*-linux-x64.tar.gz"),
                    _ => None,
                },
            },
            settings: Some(r#"{"Lua":{"telemetry":{"enable":false}}}"#),
            description: "Lua 5.1–5.4 and LuaJIT.",
            homepage: "https://luals.github.io",
            ..pkg("lua-language-server", "Lua Language Server", "Lua", Popular)
        },
        Package {
            extensions: &["zig", "zon"],
            command: &["zls"],
            source: Source::GithubRelease {
                repo: "zigtools/zls",
                asset: |os, arch| match (os, arch) {
                    ("macos", "aarch64") => Some("zls-aarch64-macos.tar.xz"),
                    ("macos", "x86_64") => Some("zls-x86_64-macos.tar.xz"),
                    ("linux", "aarch64") => Some("zls-aarch64-linux.tar.xz"),
                    ("linux", "x86_64") => Some("zls-x86_64-linux.tar.xz"),
                    _ => None,
                },
            },
            requires: &[("zig", "Zig")],
            description: "Zig. The latest release matches the latest Zig release.",
            homepage: "https://github.com/zigtools/zls",
            ..pkg("zls", "ZLS", "Zig", Popular)
        },
        // ---------------- web ----------------
        Package {
            extensions: &["html", "htm"],
            command: &["vscode-html-language-server", "--stdio"],
            source: npm("vscode-langservers-extracted", &[]),
            folder: "vscode-langservers-extracted",
            requires: NODE,
            init: Some(r#"{"provideFormatter":true,"embeddedLanguages":{"css":true,"javascript":true}}"#),
            settings: Some(r#"{"html":{"validate":{"scripts":true,"styles":true}}}"#),
            description: "HTML from VS Code (one install with the CSS, JSON and ESLint servers).",
            homepage: "https://github.com/hrsh7th/vscode-langservers-extracted",
            ..pkg("vscode-html-language-server", "HTML Language Server", "HTML", Web)
        },
        Package {
            extensions: &["css", "scss", "less"],
            command: &["vscode-css-language-server", "--stdio"],
            source: npm("vscode-langservers-extracted", &[]),
            folder: "vscode-langservers-extracted",
            requires: NODE,
            init: Some(r#"{"provideFormatter":true}"#),
            settings: Some(r#"{"css":{"validate":true},"scss":{"validate":true},"less":{"validate":true}}"#),
            description: "CSS, SCSS and Less from VS Code (one install with the HTML, JSON and ESLint servers).",
            homepage: "https://github.com/hrsh7th/vscode-langservers-extracted",
            ..pkg("vscode-css-language-server", "CSS Language Server", "CSS, SCSS, Less", Web)
        },
        Package {
            extensions: &["json", "jsonc", "json5"],
            command: &["vscode-json-language-server", "--stdio"],
            source: npm("vscode-langservers-extracted", &[]),
            folder: "vscode-langservers-extracted",
            requires: NODE,
            init: Some(r#"{"provideFormatter":true}"#),
            settings: Some(r#"{"json":{"validate":{"enable":true}}}"#),
            description: "JSON from VS Code (one install with the HTML, CSS and ESLint servers).",
            homepage: "https://github.com/hrsh7th/vscode-langservers-extracted",
            ..pkg("vscode-json-language-server", "JSON Language Server", "JSON", Web)
        },
        Package {
            extensions: &["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "vue", "svelte"],
            command: &["vscode-eslint-language-server", "--stdio"],
            source: npm("vscode-langservers-extracted", &[]),
            folder: "vscode-langservers-extracted",
            requires: NODE,
            addon: true,
            settings: Some(
                r#"{"validate":"on","packageManager":"npm","useESLintClass":false,"experimental":{},"codeActionOnSave":{"enable":false,"mode":"all"},"format":false,"quiet":false,"onIgnoredFiles":"off","options":{},"rulesCustomizations":[],"run":"onType","problems":{"shortenToSingleLine":false},"nodePath":null,"workingDirectory":{"mode":"auto"},"workspaceFolder":{"uri":"{root_uri}","name":"{root_name}"},"codeAction":{"disableRuleComment":{"enable":true,"location":"separateLine"},"showDocumentation":{"enable":true}}}"#,
            ),
            description: "ESLint problems next to the TypeScript server. Uses the project's own ESLint.",
            homepage: "https://github.com/hrsh7th/vscode-langservers-extracted",
            ..pkg("vscode-eslint-language-server", "ESLint", "ESLint", Web)
        },
        Package {
            extensions: &["html", "css", "scss", "js", "jsx", "ts", "tsx", "vue", "svelte", "astro"],
            command: &["tailwindcss-language-server", "--stdio"],
            source: npm("@tailwindcss/language-server", &[]),
            requires: NODE,
            addon: true,
            settings: Some(
                r#"{"editor":{"tabSize":2},"tailwindCSS":{"validate":true,"emmetCompletions":false,"classAttributes":["class","className","class:list","classList","ngClass"],"lint":{"cssConflict":"warning","invalidApply":"error","invalidScreen":"error","invalidVariant":"error","invalidConfigPath":"error","invalidTailwindDirective":"error","recommendedVariantOrder":"warning"},"experimental":{},"includeLanguages":{}}}"#,
            ),
            description: "Tailwind CSS class checks next to the file's main server. Does nothing in projects without Tailwind.",
            homepage: "https://github.com/tailwindlabs/tailwindcss-intellisense",
            ..pkg("tailwindcss-language-server", "Tailwind CSS", "Tailwind CSS", Web)
        },
        Package {
            extensions: &["vue"],
            command: &["vue-language-server", "--stdio"],
            // Version 2 still works on its own (no TypeScript-server plugin needed).
            source: npm("@vue/language-server@2", &["typescript@6"]),
            requires: NODE,
            init: Some(r#"{"typescript":{"tsdk":"{dir}/node_modules/typescript/lib"},"vue":{"hybridMode":false}}"#),
            description: "Vue single-file components (Vue Language Tools 2).",
            homepage: "https://github.com/vuejs/language-tools",
            ..pkg("vue-language-server", "Vue Language Server", "Vue", Web)
        },
        Package {
            extensions: &["svelte"],
            command: &["svelteserver", "--stdio"],
            source: npm("svelte-language-server", &[]),
            requires: NODE,
            description: "Svelte components.",
            homepage: "https://github.com/sveltejs/language-tools",
            ..pkg("svelte-language-server", "Svelte Language Server", "Svelte", Web)
        },
        Package {
            extensions: &["ts", "html"],
            command: &[
                "ngserver",
                "--stdio",
                "--tsProbeLocations",
                "{root},{dir}/node_modules",
                "--ngProbeLocations",
                "{root},{dir}/node_modules",
            ],
            source: npm("@angular/language-server", &["typescript@6"]),
            requires: NODE,
            addon: true,
            marker: Some("angular.json"),
            description: "Angular template checks, in projects with an angular.json.",
            homepage: "https://github.com/angular/vscode-ng-language-service",
            ..pkg("angular-language-server", "Angular Language Service", "Angular", Web)
        },
        // ---------------- config & docs ----------------
        Package {
            extensions: &["yaml", "yml"],
            command: &["yaml-language-server", "--stdio"],
            source: npm("yaml-language-server", &[]),
            requires: NODE,
            settings: Some(r#"{"yaml":{"validate":true,"hover":true,"completion":true,"format":{"enable":true},"schemaStore":{"enable":true}},"redhat":{"telemetry":{"enabled":false}}}"#),
            description: "YAML, with JSON schemas from SchemaStore (GitHub workflows, docker-compose, …).",
            homepage: "https://github.com/redhat-developer/yaml-language-server",
            ..pkg("yaml-language-server", "YAML Language Server", "YAML", ConfigDocs)
        },
        Package {
            extensions: &["toml"],
            command: &["taplo", "lsp", "stdio"],
            source: Source::GithubRelease {
                repo: "tamasfe/taplo",
                asset: |os, arch| match (os, arch) {
                    ("macos", "aarch64") => Some("taplo-darwin-aarch64.gz"),
                    ("macos", "x86_64") => Some("taplo-darwin-x86_64.gz"),
                    ("linux", "aarch64") => Some("taplo-linux-aarch64.gz"),
                    ("linux", "x86_64") => Some("taplo-linux-x86_64.gz"),
                    _ => None,
                },
            },
            description: "TOML, with schemas for Cargo.toml, pyproject.toml and more.",
            homepage: "https://taplo.tamasfe.dev",
            ..pkg("taplo", "Taplo", "TOML", ConfigDocs)
        },
        Package {
            extensions: &["md", "markdown"],
            command: &["marksman", "server"],
            source: Source::GithubRelease {
                repo: "artempyanykh/marksman",
                asset: |os, arch| match (os, arch) {
                    ("macos", _) => Some("marksman-macos"),
                    ("linux", "aarch64") => Some("marksman-linux-arm64"),
                    ("linux", "x86_64") => Some("marksman-linux-x64"),
                    _ => None,
                },
            },
            description: "Markdown: links between notes, headings, references.",
            homepage: "https://github.com/artempyanykh/marksman",
            ..pkg("marksman", "Marksman", "Markdown", ConfigDocs)
        },
        Package {
            extensions: &["sh", "bash", "zsh"],
            file_names: &[".bashrc", ".bash_profile", ".zshrc", ".profile"],
            command: &["bash-language-server", "start"],
            source: npm("bash-language-server", &[]),
            requires: NODE,
            description: "Bash and shell scripts. Uses shellcheck for problems when it is installed.",
            homepage: "https://github.com/bash-lsp/bash-language-server",
            ..pkg("bash-language-server", "Bash Language Server", "Bash", ConfigDocs)
        },
        Package {
            extensions: &["dockerfile"],
            file_names: &["dockerfile", "containerfile"],
            command: &["docker-langserver", "--stdio"],
            source: npm("dockerfile-language-server-nodejs", &[]),
            requires: NODE,
            description: "Dockerfiles.",
            homepage: "https://github.com/rcjsuen/dockerfile-language-server",
            ..pkg("dockerfile-language-server", "Dockerfile Language Server", "Dockerfile", ConfigDocs)
        },
        // ---------------- more languages ----------------
        Package {
            extensions: &["java"],
            command: &["jdtls", "-data", "{dir}/workspaces/{root_hash}"],
            source: Source::Url {
                url: "https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz",
                version: "latest snapshot",
            },
            requires: &[("java", "Java 21+"), ("python3", "Python 3")],
            description: "Java, from Eclipse (Maven and Gradle projects).",
            homepage: "https://github.com/eclipse-jdtls/eclipse.jdt.ls",
            ..pkg("jdtls", "Eclipse JDT Language Server", "Java", More)
        },
        Package {
            extensions: &["kt", "kts"],
            command: &["kotlin-language-server"],
            source: Source::GithubRelease { repo: "fwcd/kotlin-language-server", asset: |_, _| Some("server.zip") },
            requires: &[("java", "Java 17–21")],
            description: "Kotlin (community server). Its last release doesn't start on Java 22 or newer.",
            homepage: "https://github.com/fwcd/kotlin-language-server",
            ..pkg("kotlin-language-server", "Kotlin Language Server", "Kotlin", More)
        },
        Package {
            extensions: &["cs", "csx"],
            command: &["csharp-ls"],
            source: Source::DotnetTool { package: "csharp-ls" },
            requires: &[("dotnet", ".NET SDK")],
            description: "C# (Roslyn based).",
            homepage: "https://github.com/razzmatazz/csharp-language-server",
            ..pkg("csharp-ls", "csharp-ls", "C#", More)
        },
        Package {
            extensions: &["rb", "rake", "gemspec"],
            file_names: &["gemfile", "rakefile"],
            command: &["ruby-lsp"],
            source: Source::Gem { package: "ruby-lsp" },
            requires: &[("ruby", "Ruby")],
            env: &[("GEM_HOME", "{dir}"), ("GEM_PATH", "{dir}")],
            description: "Ruby, from Shopify.",
            homepage: "https://shopify.github.io/ruby-lsp",
            ..pkg("ruby-lsp", "Ruby LSP", "Ruby", More)
        },
        Package {
            extensions: &["php"],
            command: &["intelephense", "--stdio"],
            source: npm("intelephense", &[]),
            requires: NODE,
            init: Some(r#"{"storagePath":"{dir}/storage","globalStoragePath":"{dir}/global-storage"}"#),
            description: "PHP (free features of Intelephense).",
            homepage: "https://intelephense.com",
            ..pkg("intelephense", "Intelephense", "PHP", More)
        },
        Package {
            extensions: &["swift"],
            command: &["sourcekit-lsp"],
            source: Source::External {
                hint: "Comes with Xcode on macOS and with the Swift toolchain on Linux (swift.org/install)",
            },
            description: "Swift, from Apple.",
            homepage: "https://github.com/swiftlang/sourcekit-lsp",
            ..pkg("sourcekit-lsp", "SourceKit-LSP", "Swift", More)
        },
        Package {
            extensions: &["ex", "exs", "heex", "eex"],
            command: &["language_server.sh"],
            source: Source::GithubRelease { repo: "elixir-lsp/elixir-ls", asset: |_, _| Some("elixir-ls-*.zip") },
            requires: &[("elixir", "Elixir")],
            description: "Elixir.",
            homepage: "https://github.com/elixir-lsp/elixir-ls",
            ..pkg("elixir-ls", "ElixirLS", "Elixir", More)
        },
    ]
}

/// Everything needed to decide which servers run for a file.
pub struct Env<'a> {
    pub cfg: &'a Config,
    pub installed: &'a HashMap<String, InstallInfo>,
    pub servers_dir: &'a Path,
    pub root: &'a Path,
    pub on_path: &'a dyn Fn(&str) -> Option<PathBuf>,
}

/// Replace `{dir}`, `{root}`, … in a command argument or JSON text.
pub fn expand(s: &str, dir: &Path, root: &Path) -> String {
    expand_with(s, dir, root, |v| v.to_string())
}

fn expand_with(s: &str, dir: &Path, root: &Path, esc: impl Fn(&str) -> String) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    let root_name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    s.replace("{dir}", &esc(&dir.to_string_lossy()))
        .replace("{root_uri}", &esc(&crate::lsp::uri(root)))
        .replace("{root_name}", &esc(&root_name))
        .replace("{root_hash}", &format!("{:016x}", h.finish()))
        .replace("{root}", &esc(&root.to_string_lossy()))
}

fn json(text: Option<&str>, dir: &Path, root: &Path) -> Option<Value> {
    // Values go inside JSON strings, so escape them the JSON way.
    let esc = |v: &str| {
        let quoted = serde_json::to_string(v).unwrap_or_default();
        quoted.strip_prefix('"').and_then(|q| q.strip_suffix('"')).unwrap_or("").to_string()
    };
    serde_json::from_str(&expand_with(text?, dir, root, esc)).ok()
}

/// The server config for a package, run from `program` (an installed or PATH copy).
pub fn server_config(p: &Package, program: &Path, dir: &Path, root: &Path, managed: bool) -> ServerConfig {
    let mut command = vec![program.to_string_lossy().to_string()];
    command.extend(p.command[1..].iter().map(|a| expand(a, dir, root)));
    ServerConfig {
        name: p.id.to_string(),
        command,
        extensions: p.extensions.iter().map(|s| s.to_string()).collect(),
        file_names: p.file_names.iter().map(|s| s.to_string()).collect(),
        initialization_options: None,
        env: p.env.iter().map(|(k, v)| (k.to_string(), expand(v, dir, root))).collect::<BTreeMap<_, _>>(),
        init_json: json(p.init, dir, root),
        settings: json(p.settings, dir, root),
        managed,
    }
}

/// Where the package's program is: rim's copy when installed, else one on `PATH`.
pub fn locate(p: &Package, env: &Env) -> Option<(PathBuf, bool)> {
    let dir = env.servers_dir.join(p.folder);
    if let Some(info) = env.installed.get(p.folder) {
        let bin = crate::lsp_install::program_path(p, &dir, info);
        if bin.is_file() {
            return Some((bin, true));
        }
    }
    (env.on_path)(p.program()).map(|b| (b, false))
}

/// The servers for a file, main server first, then add-ons.
pub fn servers_for(env: &Env, ext: &str, file_name: &str) -> Vec<ServerConfig> {
    let cfg = env.cfg;
    let file_name = file_name.to_lowercase();
    let enabled = |name: &str| !cfg.server_disabled(name);
    let usable = |p: &Package| enabled(p.id) && p.matches(ext, &file_name) && p.marker.is_none_or(|m| env.root.join(m).exists());
    let from_pkg = |p: &Package| {
        locate(p, env).map(|(bin, managed)| server_config(p, &bin, &env.servers_dir.join(p.folder), env.root, managed))
    };
    let custom = cfg.language_servers.iter().find(|s| enabled(&s.name) && s.matches(ext, &file_name) && Config::is_custom_server(s));
    let main = custom
        .cloned()
        .or_else(|| {
            // Installed by rim beats found on PATH.
            let pkgs: Vec<&Package> = all().iter().filter(|p| !p.addon && usable(p)).collect();
            let found: Vec<ServerConfig> = pkgs.iter().filter_map(|p| from_pkg(p)).collect();
            found.iter().find(|s| s.managed).or(found.first()).cloned()
        })
        // A built-in default only counts when its program exists; otherwise the status bar can
        // offer to install a registry package for the language.
        .or_else(|| {
            cfg.language_servers
                .iter()
                .find(|s| enabled(&s.name) && s.matches(ext, &file_name) && s.command.first().is_some_and(|c| (env.on_path)(c).is_some()))
                .cloned()
        });
    let mut out: Vec<ServerConfig> = main.into_iter().collect();
    for p in all().iter().filter(|p| p.addon && usable(p)) {
        if out.iter().any(|s| s.name == p.id) {
            continue;
        }
        if let Some(s) = from_pkg(p) {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
#[path = "validate/lsp_registry_tests.rs"]
mod validate_tests;
