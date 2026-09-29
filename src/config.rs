//! User configuration, stored in `~/.config/rim/config.toml`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub font_size: f32,
    /// Optional path to a monospace .ttf/.otf font.
    pub font: Option<String>,
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub format_on_save: bool,
    /// Modal editing with vim motions instead of regular editing.
    pub vim_mode: bool,
    pub claude: ClaudeConfig,
    pub graph: GraphConfig,
    /// File extension → formatter command. The file's contents are piped to stdin and
    /// the formatted result is read from stdout. `{file}` is replaced by the file path.
    /// A binary found in `<workspace>/node_modules/.bin` takes precedence over `$PATH`.
    pub formatters: BTreeMap<String, Vec<String>>,
    pub language_servers: Vec<ServerConfig>,
    /// Language servers (by name) that never start, whether built in, installed or configured.
    pub disabled_language_servers: Vec<String>,
    /// Set when config.toml could not be read: we then never write it, so the user's
    /// broken-but-fixable file is not replaced by defaults.
    #[serde(skip)]
    pub read_only: bool,
}

/// Claude Code integration (`[claude]` in config.toml).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    /// Path to the `claude` executable. Found automatically when empty.
    pub path: Option<String>,
    /// Model alias or id (`opus`, `sonnet`, `haiku`, …). The CLI default when empty.
    pub model: Option<String>,
    /// `manual` (ask before edits), `acceptEdits` or `plan`.
    pub permission_mode: String,
    /// Share the open file and selection with each prompt.
    pub include_context: bool,
    /// Effort level (`low`, `medium`, `high`, `xhigh`, `max`); the model's default when empty.
    pub effort: Option<String>,
    pub fast_mode: bool,
    /// Extended thinking (off sends `alwaysThinkingEnabled = false`).
    pub thinking: bool,
    /// One of the CLI's output styles; `default` when empty.
    pub output_style: Option<String>,
    /// Continue on another model when a message is flagged, instead of asking each time.
    pub switch_models_on_flag: Option<bool>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            path: None,
            model: None,
            permission_mode: "manual".into(),
            include_context: true,
            effort: None,
            fast_mode: false,
            thinking: true,
            output_style: None,
            switch_models_on_flag: None,
        }
    }
}

impl ClaudeConfig {
    pub fn session_settings(&self) -> crate::claude::SessionSettings {
        crate::claude::SessionSettings {
            effort: self.effort.clone().filter(|e| !e.is_empty()),
            fast_mode: self.fast_mode,
            thinking: self.thinking,
            output_style: self.output_style.clone().filter(|s| !s.is_empty()),
            switch_models_on_flag: self.switch_models_on_flag,
        }
    }

    pub fn set_session_settings(&mut self, s: &crate::claude::SessionSettings) {
        self.effort = s.effort.clone();
        self.fast_mode = s.fast_mode;
        self.thinking = s.thinking;
        self.output_style = s.output_style.clone();
        self.switch_models_on_flag = s.switch_models_on_flag;
    }
}

/// The project graph view (`[graph]` in config.toml).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Model for "Describe with Claude"; `[claude] model` when empty.
    pub describe_model: Option<String>,
    /// Above this many files only directories are shown.
    pub max_files: usize,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self { describe_model: None, max_files: 5000 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub command: Vec<String>,
    pub extensions: Vec<String>,
    /// Files without a useful extension (`Dockerfile`), matched by exact name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_names: Vec<String>,
    /// Passed to the server as `initializationOptions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<toml::Table>,
    /// Extra environment variables for the server process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Set for servers from the language server manager: `initializationOptions` as JSON, and the
    /// settings returned for `workspace/configuration` requests.
    #[serde(skip)]
    pub init_json: Option<serde_json::Value>,
    #[serde(skip)]
    pub settings: Option<serde_json::Value>,
    /// Installed by rim (not a PATH lookup or a user entry).
    #[serde(skip)]
    pub managed: bool,
}

impl ServerConfig {
    pub fn matches(&self, ext: &str, file_name: &str) -> bool {
        (!ext.is_empty() && self.extensions.iter().any(|e| e == ext))
            || self
                .file_names
                .iter()
                .any(|f| f.eq_ignore_ascii_case(file_name))
    }
}

fn cmd(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

impl Default for Config {
    fn default() -> Self {
        let mut formatters = BTreeMap::new();
        let prettier = cmd(&["prettier", "--stdin-filepath", "{file}"]);
        for ext in [
            "js", "jsx", "mjs", "cjs", "ts", "tsx", "json", "css", "scss", "less", "html", "vue",
            "svelte", "md", "yaml", "yml", "graphql",
        ] {
            formatters.insert(ext.to_string(), prettier.clone());
        }
        formatters.insert("rs".into(), cmd(&["rustfmt", "--edition", "2024"]));
        formatters.insert(
            "py".into(),
            cmd(&["ruff", "format", "--stdin-filename", "{file}", "-"]),
        );
        formatters.insert("go".into(), cmd(&["gofmt"]));
        for ext in ["c", "h", "cpp", "hpp", "cc"] {
            formatters.insert(
                ext.into(),
                cmd(&["clang-format", "--assume-filename={file}"]),
            );
        }
        formatters.insert(
            "lua".into(),
            cmd(&["stylua", "--stdin-filepath", "{file}", "-"]),
        );

        let server = |name: &str, command: &[&str], exts: &[&str]| ServerConfig {
            name: name.into(),
            command: cmd(command),
            extensions: exts.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        Self {
            theme: "tokyonight".into(),
            font_size: 14.0,
            font: None,
            tab_size: 4,
            insert_spaces: true,
            format_on_save: false,
            vim_mode: false,
            claude: ClaudeConfig::default(),
            graph: GraphConfig::default(),
            disabled_language_servers: Vec::new(),
            read_only: false,
            formatters,
            language_servers: vec![
                server("rust-analyzer", &["rust-analyzer"], &["rs"]),
                server(
                    "typescript-language-server",
                    &["typescript-language-server", "--stdio"],
                    &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
                ),
                server("pyright", &["pyright-langserver", "--stdio"], &["py"]),
                server(
                    "clangd",
                    &["clangd"],
                    &["c", "h", "cpp", "hpp", "cc", "cxx"],
                ),
                server("gopls", &["gopls"], &["go"]),
                server("lua-language-server", &["lua-language-server"], &["lua"]),
                server("zls", &["zls"], &["zig"]),
            ],
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rim")
            .join("config.toml")
    }

    /// Load the config, writing the defaults on first run.
    pub fn load() -> Result<Self, String> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str::<Config>(&s)
                .map(Config::with_defaults)
                .map_err(|e| format!("config.toml: {e}")),
            Err(_) => {
                let cfg = Self::default();
                let _ = cfg.write_all();
                Ok(cfg)
            }
        }
    }

    /// Entries in `[formatters]` / `[[language_servers]]` add to or override the built-in ones
    /// instead of replacing the whole list. An empty formatter command disables that extension.
    fn with_defaults(mut self) -> Self {
        let d = Config::default();
        for (ext, cmd) in d.formatters {
            self.formatters.entry(ext).or_insert(cmd);
        }
        for server in d.language_servers {
            let taken = self.language_servers.iter().any(|u| {
                u.name == server.name || u.extensions.iter().any(|e| server.extensions.contains(e))
            });
            if !taken {
                self.language_servers.push(server);
            }
        }
        self
    }

    /// Create config.toml with the defaults if it doesn't exist yet. Never touches an existing file.
    pub fn ensure_exists() {
        if !Self::path().exists() {
            let _ = Self::default().write_all();
        }
    }

    /// Save the settings the app can change (theme, font, indentation, vim, Claude options),
    /// editing only those keys so the user's comments, formatting and other entries survive.
    pub fn save(&self) -> Result<(), String> {
        if self.read_only {
            return Err(
                "config.toml has errors, so settings were not saved. Fix the file first.".into(),
            );
        }
        let path = Self::path();
        let existing = std::fs::read_to_string(&path).ok();
        let out = match &existing {
            None => toml::to_string_pretty(self).map_err(|e| e.to_string())?,
            Some(text) => {
                let mut doc: toml_edit::DocumentMut =
                    text.parse().map_err(|e| format!("config.toml: {e}"))?;
                self.write_keys(&mut doc);
                doc.to_string()
            }
        };
        if existing.as_deref() == Some(out.as_str()) {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(path, out).map_err(|e| e.to_string())
    }

    /// Write the whole config, replacing the file (first run, tests).
    pub fn write_all(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let out = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, out).map_err(|e| e.to_string())
    }

    fn write_keys(&self, doc: &mut toml_edit::DocumentMut) {
        use toml_edit::{Item, Table, value};
        fn set_opt(t: &mut Table, key: &str, v: Option<&str>) {
            match v {
                Some(v) => {
                    if t.get(key).and_then(|i| i.as_str()) != Some(v) {
                        t[key] = value(v);
                    }
                }
                None => {
                    t.remove(key);
                }
            }
        }
        // Only write a key when its value differs, so untouched lines keep their formatting.
        fn set<V: Into<toml_edit::Value> + Clone>(
            t: &mut Table,
            key: &str,
            v: V,
            same: impl Fn(&Item) -> bool,
        ) {
            if !t.get(key).is_some_and(&same) {
                t[key] = value(v);
            }
        }
        let root = doc.as_table_mut();
        set(root, "theme", self.theme.as_str(), |i| {
            i.as_str() == Some(self.theme.as_str())
        });
        set(root, "font_size", self.font_size as f64, |i| {
            i.as_float().or_else(|| i.as_integer().map(|n| n as f64)) == Some(self.font_size as f64)
        });
        set_opt(root, "font", self.font.as_deref());
        set(root, "tab_size", self.tab_size as i64, |i| {
            i.as_integer() == Some(self.tab_size as i64)
        });
        set(root, "insert_spaces", self.insert_spaces, |i| {
            i.as_bool() == Some(self.insert_spaces)
        });
        set(root, "format_on_save", self.format_on_save, |i| {
            i.as_bool() == Some(self.format_on_save)
        });
        set(root, "vim_mode", self.vim_mode, |i| {
            i.as_bool() == Some(self.vim_mode)
        });
        let disabled = &self.disabled_language_servers;
        let same_disabled = |i: &Item| {
            i.as_array().is_some_and(|a| {
                a.iter()
                    .map(|v| v.as_str())
                    .eq(disabled.iter().map(|s| Some(s.as_str())))
            })
        };
        if disabled.is_empty() {
            root.remove("disabled_language_servers");
        } else if !root
            .get("disabled_language_servers")
            .is_some_and(same_disabled)
        {
            root["disabled_language_servers"] = value(
                disabled
                    .iter()
                    .map(String::as_str)
                    .collect::<toml_edit::Array>(),
            );
        }
        if !root.contains_key("claude") {
            root.insert("claude", Item::Table(Table::new()));
        }
        if let Some(c) = root.get_mut("claude").and_then(|i| i.as_table_mut()) {
            let cc = &self.claude;
            set_opt(c, "path", cc.path.as_deref());
            set_opt(c, "model", cc.model.as_deref());
            set(c, "permission_mode", cc.permission_mode.as_str(), |i| {
                i.as_str() == Some(cc.permission_mode.as_str())
            });
            set(c, "include_context", cc.include_context, |i| {
                i.as_bool() == Some(cc.include_context)
            });
            set_opt(c, "effort", cc.effort.as_deref());
            set(c, "fast_mode", cc.fast_mode, |i| i.as_bool() == Some(cc.fast_mode));
            set(c, "thinking", cc.thinking, |i| i.as_bool() == Some(cc.thinking));
            set_opt(c, "output_style", cc.output_style.as_deref());
            match cc.switch_models_on_flag {
                Some(b) => set(c, "switch_models_on_flag", b, |i| i.as_bool() == Some(b)),
                None => {
                    c.remove("switch_models_on_flag");
                }
            }
        }
    }

    /// A `[[language_servers]]` entry the user wrote or changed (not one of the built-in defaults,
    /// which config.toml also lists after the first run).
    pub fn is_custom_server(s: &ServerConfig) -> bool {
        static DEFAULTS: std::sync::OnceLock<Vec<ServerConfig>> = std::sync::OnceLock::new();
        !DEFAULTS.get_or_init(|| Config::default().language_servers).contains(s)
    }

    #[cfg(test)]
    pub fn server_for_ext(&self, ext: &str) -> Option<&ServerConfig> {
        self.language_servers
            .iter()
            .find(|s| s.extensions.iter().any(|e| e == ext))
    }

    pub fn server_disabled(&self, name: &str) -> bool {
        self.disabled_language_servers.iter().any(|d| d == name)
    }
}

/// Where the language server manager installs servers, one folder per package.
pub fn servers_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rim")
        .join("servers")
}

/// Last opened folder, remembered between sessions.
pub fn state_file() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rim")
        .join("last_folder")
}

/// Like `language_id`, but also knows files recognised by name.
pub fn language_id_for(ext: &str, file_name: &str) -> &'static str {
    match file_name.to_lowercase().as_str() {
        "dockerfile" | "containerfile" => "dockerfile",
        "gemfile" | "rakefile" => "ruby",
        "go.mod" => "go.mod",
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => "shellscript",
        _ => language_id(ext),
    }
}

pub fn language_id(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "cxx" | "hh" => "cpp",
        "go" => "go",
        "lua" => "lua",
        "zig" => "zig",
        "json" => "json",
        "css" => "css",
        "jsonc" | "json5" => "jsonc",
        "scss" => "scss",
        "less" => "less",
        "html" | "htm" => "html",
        "md" | "markdown" => "markdown",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" | "zsh" => "shellscript",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "cs" | "csx" => "csharp",
        "rb" | "rake" | "gemspec" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "ex" | "exs" => "elixir",
        "heex" => "phoenix-heex",
        "eex" => "eex",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "pyi" => "python",
        "hxx" => "cpp",
        "zon" => "zig",
        "dockerfile" => "dockerfile",
        _ => "plaintext",
    }
}

pub fn comment_token(ext: &str, file_name: &str) -> Option<&'static str> {
    match ext {
        "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "js" | "jsx" | "ts" | "tsx" | "mjs"
        | "cjs" | "go" | "java" | "kt" | "swift" | "cs" | "zig" | "scss" | "less" | "dart"
        | "proto" | "jsonc" => Some("//"),
        "py" | "sh" | "bash" | "zsh" | "fish" | "yaml" | "yml" | "toml" | "rb" | "pl" | "nix"
        | "r" | "conf" | "ini" | "cmake" | "mk" | "ex" | "exs" => Some("#"),
        "lua" | "sql" | "hs" | "elm" => Some("--"),
        "lisp" | "clj" | "el" | "scm" => Some(";"),
        "tex" | "erl" => Some("%"),
        "vim" => Some("\""),
        _ => match file_name {
            "Makefile" | "Dockerfile" | "Justfile" | ".gitignore" | ".env" => Some("#"),
            _ => None,
        },
    }
}

#[cfg(test)]
#[path = "validate/config_tests.rs"]
mod validate_tests;
