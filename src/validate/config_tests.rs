//! Validation: config load/save. Included from config.rs.

use super::*;
use crate::test_env;

#[test]
fn first_run_writes_defaults_and_round_trips() {
    let _g = test_env::lock();
    let p = Config::path();
    let _ = std::fs::remove_file(&p);
    let c = Config::load().unwrap();
    assert!(p.is_file(), "defaults written on first run");
    assert_eq!(c.theme, "tokyonight");
    let mut c2 = c.clone();
    c2.vim_mode = true;
    c2.tab_size = 2;
    c2.font = Some("~/fonts/x.ttf".into());
    c2.claude.model = Some("haiku".into());
    c2.formatters.insert("zig".into(), vec!["zig".into(), "fmt".into(), "--stdin".into()]);
    c2.language_servers[0].initialization_options = Some(toml::from_str("check = { command = \"clippy\" }").unwrap());
    c2.write_all().unwrap();
    let back = Config::load().unwrap();
    assert!(back.vim_mode);
    assert_eq!(back.tab_size, 2);
    assert_eq!(back.font.as_deref(), Some("~/fonts/x.ttf"));
    assert_eq!(back.claude.model.as_deref(), Some("haiku"));
    assert_eq!(back.formatters["zig"], ["zig", "fmt", "--stdin"]);
    assert_eq!(back.language_servers[0].initialization_options.as_ref().unwrap()["check"]["command"].as_str(), Some("clippy"));
    assert_eq!(toml::to_string_pretty(&back).unwrap(), toml::to_string_pretty(&c2).unwrap());
    std::fs::remove_file(&p).ok();
}

#[test]
fn missing_fields_use_defaults() {
    let c: Config = toml::from_str("vim_mode = true\n[claude]\nmodel = \"opus\"\n").unwrap();
    assert!(c.vim_mode);
    assert_eq!(c.tab_size, 4);
    assert_eq!(c.claude.permission_mode, "manual");
    assert!(c.claude.include_context);
    assert!(!c.language_servers.is_empty());
    let c: Config = toml::from_str("").unwrap();
    assert_eq!(c.font_size, 14.0);
}

#[test]
fn bad_toml_and_wrong_types_are_errors_not_panics() {
    let _g = test_env::lock();
    let p = Config::path();
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    for bad in ["vim_mode = ", "tab_size = \"four\"", "[[language_servers]]\nname = \"x\"\n", "theme = 5", "\u{0}\u{1}"] {
        std::fs::write(&p, bad).unwrap();
        let e = Config::load().unwrap_err();
        assert!(e.starts_with("config.toml:"), "{e}");
        // The broken file is left alone by load.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
    }
    std::fs::remove_file(&p).ok();
}

#[test]
fn a_user_server_list_replaces_the_defaults() {
    // Adding one [[language_servers]] entry drops all built-in servers (no merge).
    let c: Config = toml::from_str("[[language_servers]]\nname = \"mine\"\ncommand = [\"mine\"]\nextensions = [\"xyz\"]\n").unwrap();
    eprintln!("servers after adding one: {:?}", c.language_servers.iter().map(|s| &s.name).collect::<Vec<_>>());
    assert!(c.server_for_ext("rs").is_none());
}

#[test]
fn comment_tokens_and_language_ids() {
    assert_eq!(comment_token("rs", "a.rs"), Some("//"));
    assert_eq!(comment_token("", "Makefile"), Some("#"));
    assert_eq!(comment_token("md", "a.md"), None);
    assert_eq!(language_id("tsx"), "typescriptreact");
}

#[test]
fn user_entries_extend_the_built_in_servers_and_formatters() {
    let _g = test_env::lock();
    let p = Config::path();
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(
        &p,
        "[formatters]\nrs = []\nnim = [\"nimpretty\"]\n\n[[language_servers]]\nname = \"mine\"\ncommand = [\"my-ls\"]\nextensions = [\"rs\"]\n",
    )
    .unwrap();
    let c = Config::load().unwrap();
    assert_eq!(c.formatters["nim"], ["nimpretty"]);
    assert!(c.formatters["rs"].is_empty(), "user's empty entry disables the default");
    assert!(c.formatters.contains_key("ts"), "defaults for other extensions stay");
    assert_eq!(c.server_for_ext("rs").unwrap().name, "mine", "user's server wins for its extensions");
    assert!(c.server_for_ext("py").is_some(), "other built-in servers stay");
    assert!(!c.language_servers.iter().any(|s| s.name == "rust-analyzer"));
    std::fs::remove_file(&p).ok();
}
