# rim

A small, fast code editor written in Rust (egui). It has no built-in terminal, by design.

## Features
- File explorer (git-status colors, new/rename/move to trash), tabs, quick open (`Ctrl+P`), command palette (`Ctrl+Shift+P`)
- Syntax highlighting for ~200 languages (syntect + bat's syntax set)
- Language servers: diagnostics, completion (`Ctrl+Space` / as you type), hover (mouse or `Ctrl+K`), go to definition (`F12` / `Ctrl+click`), formatting
- External formatters (prettier, rustfmt, ruff, gofmt, clang-format, stylua), with optional format on save; a project-local `node_modules/.bin/prettier` is preferred
- Git: status, stage/unstage/discard, commit, diffs, branch checkout/create, pull/push/fetch, log, gutter change markers
- Find in file (`Ctrl+F`), search in files (`Ctrl+Shift+F`), go to line (`Ctrl+G`), toggle comment (`Ctrl+/`), auto-indent
- Claude Code in a side panel (`Ctrl+L`): streaming chat, tool cards, inline diffs, approvals, session history
- Optional vim motions (off by default): toggle in Settings (`Ctrl+,`) or with "Preferences: Toggle Vim Motions"
- Themes: tokyonight, catppuccin (mocha/latte), gruvbox (dark/light), kanagawa, nord, onedark, rose-pine, everforest

## Usage
    cargo build --release
    ./target/release/rim [folder-or-file]

Needs Rust (https://rustup.rs). Runs on Linux and macOS.
On macOS, install Apple's command-line tools first (`xcode-select --install`); the config file lives in
`~/Library/Application Support/rim/config.toml` instead of `~/.config/rim/`. Start rim from a terminal so it
sees your `PATH` (language servers, formatters, `claude`); apps started from the Dock/Finder get a minimal `PATH`.

Settings live in `~/.config/rim/config.toml`. Open it with "Preferences: Open config.toml"; saving the file reloads it.
Language servers and formatters are ordinary programs on your `PATH`, configured per file extension in that file.
Your `[formatters]` and `[[language_servers]]` entries add to the built-in ones (same extension or name overrides;
`ext = []` turns a formatter off). Changing settings in the app only rewrites those keys, so comments and layout in the
file are kept, and a config.toml with errors is never overwritten.

Saving is safe: files are written to a temporary file and renamed into place (permissions kept), CRLF files keep
their line endings, and if another program changed the file since you opened it, rim asks before overwriting.

## Keys
| Key | Action |
|---|---|
| Ctrl+S / Ctrl+W / Ctrl+N / Ctrl+O | save / close tab / new file / open folder |
| Ctrl+B | toggle sidebar |
| Ctrl+Shift+E / F / G | explorer / search / git |
| Shift+Alt+F | format document |
| Ctrl+Tab, Ctrl+PgUp/PgDn | switch tabs |
| Ctrl+Enter (in commit box) | commit |

## Settings
`Ctrl+,` opens a small settings dialog: vim motions, format on save, indentation, font size and theme.
Everything is also in `~/.config/rim/config.toml` (e.g. `vim_mode = true`).

## Vim motions
When enabled, each tab starts in normal mode; the status bar shows the mode.
- Modes: normal, insert (`i a I A o O s S c...`), visual (`v`), visual line (`V`), command line (`:` `/` `?`)
- Motions: `h j k l w W b B e E ge 0 ^ $ gg G {n}G f F t T ; , % { } n N * # _ + -`, `Ctrl+d/u/f/b`, with counts
- Operators: `d c y > <` with motions, doubled for lines (`dd cc yy >>`), and text objects (`iw aw i" a" i( a( i{ a{ i[ a[ ip ap`)
- Other: `x X D C s S Y p P J r ~ u Ctrl+r .` (dot repeat), one shared register (yanks also go to the system clipboard)
- Ex commands: `:w :q :q! :wq :x :{line}`; `ZZ` / `ZQ`
- `Ctrl+F` / `Ctrl+B` page down/up in normal mode (Find and the sidebar toggle stay available from insert mode or the palette)
- Not supported: macros, marks, named registers, visual block, regex substitute (`:s`)

## Claude
`Ctrl+L` opens the Claude panel (and moves focus between chat and editor). It drives the local **Claude Code** CLI, so your login,
models, `CLAUDE.md`, MCP servers, skills and permission rules work exactly as they do in a terminal.
- **Setup:** install Claude Code; rim finds it on `PATH`, in the usual install locations, or in the VS Code extension's bundled copy
  (override with `[claude] path = "…"`). If you are already signed in from VS Code or a terminal, rim uses that login.
- **Signing in:** if you are signed out, the panel shows a Sign in banner: it opens your browser, you paste the code the page shows,
  and you're in. The Account menu shows who is signed in and has Sign out (which also signs out VS Code and terminals).
- **Context:** your open file and selection are shared with each prompt (untick the box under the prompt to stop). `@` mentions a file.
- **Approvals:** edits and commands show up as cards with a diff or the command, and wait for Allow / Deny.
  Modes: ask before edits, edit automatically, plan mode. Model: default, Opus, Sonnet or Haiku.
- **Also:** Esc stops a running turn, History resumes earlier conversations, files Claude changes reload in the editor.
- **Not yet:** images and pasted attachments, slash commands, MCP/`/` menus, and Claude's own diff-review tab inside the editor.

## Development
    cargo test                                   # ~200 tests, headless (no window needed)
    cargo test -- --ignored                      # also talks to real rust-analyzer / Claude Code (uses a little plan usage)
    cargo test --release bench_frames -- --ignored --nocapture   # frame times on 5k–50k line files

Large files stay responsive: highlighting parses lazily and only colours the visible lines, and git status,
commits, project search and file listing run in the background.

