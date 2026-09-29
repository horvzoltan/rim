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
    /// File extension → formatter command. The file's contents are piped to stdin and
    /// the formatted result is read from stdout. `{file}` is replaced by the file path.
    /// A binary found in `<workspace>/node_modules/.bin` takes precedence over `$PATH`.
    pub formatters: BTreeMap<String, Vec<String>>,
    pub language_servers: Vec<ServerConfig>,
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
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self { path: None, model: None, permission_mode: "manual".into(), include_context: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub command: Vec<String>,
    pub extensions: Vec<String>,
    /// Passed to the server as `initializationOptions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<toml::Table>,
}

fn cmd(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

impl Default for Config {
    fn default() -> Self {
        let mut formatters = BTreeMap::new();
        let prettier = cmd(&["prettier", "--stdin-filepath", "{file}"]);
        for ext in [
            "js", "jsx", "mjs", "cjs", "ts", "tsx", "json", "css", "scss", "less", "html", "vue", "svelte", "md",
            "yaml", "yml", "graphql",
        ] {
            formatters.insert(ext.to_string(), prettier.clone());
        }
        formatters.insert("rs".into(), cmd(&["rustfmt", "--edition", "2024"]));
        formatters.insert("py".into(), cmd(&["ruff", "format", "--stdin-filename", "{file}", "-"]));
        formatters.insert("go".into(), cmd(&["gofmt"]));
        for ext in ["c", "h", "cpp", "hpp", "cc"] {
            formatters.insert(ext.into(), cmd(&["clang-format", "--assume-filename={file}"]));
        }
        formatters.insert("lua".into(), cmd(&["stylua", "--stdin-filepath", "{file}", "-"]));

        let server = |name: &str, command: &[&str], exts: &[&str]| ServerConfig {
            name: name.into(),
            command: cmd(command),
            extensions: exts.iter().map(|s| s.to_string()).collect(),
            initialization_options: None,
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
                server("clangd", &["clangd"], &["c", "h", "cpp", "hpp", "cc", "cxx"]),
                server("gopls", &["gopls"], &["go"]),
                server("lua-language-server", &["lua-language-server"], &["lua"]),
                server("zls", &["zls"], &["zig"]),
            ],
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("rim").join("config.toml")
    }

    /// Load the config, writing the defaults on first run.
    pub fn load() -> Result<Self, String> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str::<Config>(&s).map(Config::with_defaults).map_err(|e| format!("config.toml: {e}")),
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
            let taken = self
                .language_servers
                .iter()
                .any(|u| u.name == server.name || u.extensions.iter().any(|e| server.extensions.contains(e)));
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
            return Err("config.toml has errors, so settings were not saved. Fix the file first.".into());
        }
        let path = Self::path();
        let existing = std::fs::read_to_string(&path).ok();
        let out = match &existing {
            None => toml::to_string_pretty(self).map_err(|e| e.to_string())?,
            Some(text) => {
                let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
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
        fn set<V: Into<toml_edit::Value> + Clone>(t: &mut Table, key: &str, v: V, same: impl Fn(&Item) -> bool) {
            if !t.get(key).is_some_and(&same) {
                t[key] = value(v);
            }
        }
        let root = doc.as_table_mut();
        set(root, "theme", self.theme.as_str(), |i| i.as_str() == Some(self.theme.as_str()));
        set(root, "font_size", self.font_size as f64, |i| i.as_float().or_else(|| i.as_integer().map(|n| n as f64)) == Some(self.font_size as f64));
        set_opt(root, "font", self.font.as_deref());
        set(root, "tab_size", self.tab_size as i64, |i| i.as_integer() == Some(self.tab_size as i64));
        set(root, "insert_spaces", self.insert_spaces, |i| i.as_bool() == Some(self.insert_spaces));
        set(root, "format_on_save", self.format_on_save, |i| i.as_bool() == Some(self.format_on_save));
        set(root, "vim_mode", self.vim_mode, |i| i.as_bool() == Some(self.vim_mode));
        if !root.contains_key("claude") {
            root.insert("claude", Item::Table(Table::new()));
        }
        if let Some(c) = root.get_mut("claude").and_then(|i| i.as_table_mut()) {
            let cc = &self.claude;
            set_opt(c, "path", cc.path.as_deref());
            set_opt(c, "model", cc.model.as_deref());
            set(c, "permission_mode", cc.permission_mode.as_str(), |i| i.as_str() == Some(cc.permission_mode.as_str()));
            set(c, "include_context", cc.include_context, |i| i.as_bool() == Some(cc.include_context));
        }
    }

    pub fn server_for_ext(&self, ext: &str) -> Option<&ServerConfig> {
        self.language_servers.iter().find(|s| s.extensions.iter().any(|e| e == ext))
    }
}

/// Last opened folder, remembered between sessions.
pub fn state_file() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rim")
        .join("last_folder")
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
        "scss" => "scss",
        "html" => "html",
        "md" => "markdown",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" => "shellscript",
        "java" => "java",
        _ => "plaintext",
    }
}

pub fn comment_token(ext: &str, file_name: &str) -> Option<&'static str> {
    match ext {
        "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "go"
        | "java" | "kt" | "swift" | "cs" | "zig" | "scss" | "less" | "dart" | "proto" | "jsonc" => Some("//"),
        "py" | "sh" | "bash" | "zsh" | "fish" | "yaml" | "yml" | "toml" | "rb" | "pl" | "nix" | "r" | "conf"
        | "ini" | "cmake" | "mk" | "ex" | "exs" => Some("#"),
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
