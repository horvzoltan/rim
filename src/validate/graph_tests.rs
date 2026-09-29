//! Validation: project graph scanning, import resolution, layout and descriptions.
//! Included from graph.rs.

use std::path::{Path, PathBuf};

use super::describe::{Cache, Request, heuristic, readme_summary};
use super::layout::Layout;
use super::scan::{ImportRef, RustKind, extract_imports, scan};
use super::*;

fn fixture(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-graph-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let w = |rel: &str, s: &str| {
        let p = d.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    };
    w(".gitignore", "target/\n");
    w("Cargo.toml", "[package]\nname = \"x\"\n");
    w("README.md", "# x\n\n[![ci](b.svg)](u)\n\nA tiny example project used by the graph tests.\n\nMore text.\n");
    w("src/main.rs", "//! Entry point.\n//! Starts the app.\nmod app;\nmod config;\nuse crate::app::App;\nuse std::io;\nfn main() {}\n");
    w("src/app.rs", "// The application.\nmod panels;\nuse super::config::{Config, other};\nuse crate::{config, app::panels};\npub struct App;\n#[cfg(test)]\n#[path = \"validate/app_tests.rs\"]\nmod tests;\n");
    w("src/app/panels.rs", "use super::*;\nuse crate::config::Config;\n");
    w("src/config.rs", "/* Config loading. */\npub struct Config;\n");
    w("src/validate/app_tests.rs", "use super::*;\n");
    w("web/index.ts", "import { a } from './util';\nimport b from \"./components\";\nimport c from './legacy.js';\nimport react from 'react';\nexport * from './util';\nconst d = require('./data.json');\n");
    w("web/util.ts", "/**\n * Helpers for the web part.\n */\nexport const a = 1;\n");
    w("web/components/index.tsx", "export default 1;\n");
    w("web/legacy.ts", "export default 2;\n");
    w("web/data.json", "{}\n");
    w("py/pkg/__init__.py", "\"\"\"The package.\"\"\"\n");
    w("py/pkg/a.py", "from .b import thing\nfrom . import c\nimport pkg.d\nimport os\n");
    w("py/pkg/b.py", "# Does b things.\n# Second line.\n#\n# Not this.\nthing = 1\n");
    w("py/pkg/c.py", "\n");
    w("py/pkg/d.py", "\n");
    w("c/main.c", "#include \"util.h\"\n#include <stdio.h>\n#include \"../include/api.h\"\n");
    w("c/util.h", "\n");
    w("include/api.h", "\n");
    w("target/junk.rs", "mod app;\n");
    std::fs::create_dir_all(d.join("assets")).unwrap();
    std::fs::write(d.join("assets/blob.bin"), [0xffu8, 0xfe, 0x00, 0x41]).unwrap();
    d
}

fn idx(g: &ProjectGraph, rel: &str) -> usize {
    g.by_rel(rel).unwrap_or_else(|| panic!("no node {rel}"))
}

fn has_edge(g: &ProjectGraph, from: &str, to: &str) -> bool {
    let (a, b) = (idx(g, from), idx(g, to));
    g.edges.iter().any(|e| e.from == a && e.to == b)
}

#[test]
fn extracts_rust_imports() {
    let src = "#[path = \"x/y.rs\"]\nmod y;\npub(crate) mod a;\nmod b {\n}\nuse crate::a::{b, c::{d, self}};\nuse super::super::z;\nuse self::q as r;\n// use crate::nope;\nuse std::{\n    io,\n    fs,\n};\n";
    let imps = extract_imports("rust", src);
    assert_eq!(imps[0], ImportRef::Relative("x/y.rs".into()));
    assert_eq!(imps[1], ImportRef::RustMod("a".into()));
    assert_eq!(imps[2], ImportRef::RustPath { kind: RustKind::Crate, segs: vec!["a".into(), "b".into()] });
    assert_eq!(imps[3], ImportRef::RustPath { kind: RustKind::Crate, segs: vec!["a".into(), "c".into(), "d".into()] });
    assert_eq!(imps[4], ImportRef::RustPath { kind: RustKind::Crate, segs: vec!["a".into(), "c".into()] });
    assert_eq!(imps[5], ImportRef::RustPath { kind: RustKind::Super(2), segs: vec!["z".into()] });
    assert_eq!(imps[6], ImportRef::RustPath { kind: RustKind::SelfMod, segs: vec!["q".into()] });
    assert_eq!(imps[7], ImportRef::RustPath { kind: RustKind::Bare, segs: vec!["std".into(), "io".into()] });
    assert_eq!(imps[8], ImportRef::RustPath { kind: RustKind::Bare, segs: vec!["std".into(), "fs".into()] });
    assert_eq!(imps.len(), 9);
}

#[test]
fn extracts_js_python_c_imports() {
    let js = extract_imports("typescript", "import x from './a';\nimport 'react';\n// import y from './no';\nexport { z } from \"../b\";\nconst q = require('./c');\nconst w = lazy(() => import('./d'));\nconst v = require(name);\n");
    let rel = |s: &str| ImportRef::Relative(s.into());
    assert_eq!(js, vec![rel("./a"), ImportRef::External("react".into()), rel("../b"), rel("./c"), rel("./d")]);
    let py = extract_imports("python", "from ..a.b import c, d as e\nimport x.y, z\nfrom . import f\n");
    assert_eq!(py[0], ImportRef::Py { levels: 2, segs: vec!["a".into(), "b".into()], names: vec!["c".into(), "d".into()] });
    assert_eq!(py[1], ImportRef::Py { levels: 0, segs: vec!["x".into(), "y".into()], names: vec![] });
    assert_eq!(py[2], ImportRef::Py { levels: 0, segs: vec!["z".into()], names: vec![] });
    assert_eq!(py[3], ImportRef::Py { levels: 1, segs: vec![], names: vec!["f".into()] });
    let c = extract_imports("c", "#include \"a.h\"\n#  include <b.h>\n# include \"c/d.h\"\n");
    assert_eq!(c, vec![rel("a.h"), ImportRef::External("b.h".into()), rel("c/d.h")]);
    let go = extract_imports("go", "import \"fmt\"\nimport (\n\t\"os\"\n\tx \"example.com/m/pkg\"\n)\n");
    assert_eq!(go, vec![ImportRef::GoPkg("fmt".into()), ImportRef::GoPkg("os".into()), ImportRef::GoPkg("example.com/m/pkg".into())]);
    assert!(extract_imports("markdown", "import x from './a';").is_empty());
}

#[test]
fn scan_builds_nodes_and_resolves_edges() {
    let d = fixture("scan");
    let g = scan(&d, 5000);
    assert!(!g.collapsed);
    assert_eq!(g.nodes[0].rel, "");
    assert!(g.by_rel("target/junk.rs").is_none(), "gitignored files are skipped");
    assert!(g.by_rel("assets/blob.bin").is_some(), "binary files are still nodes");
    assert_eq!(g.nodes[idx(&g, "assets/blob.bin")].loc, 0);
    assert_eq!(g.nodes[idx(&g, "src/main.rs")].loc, 7);
    assert_eq!(g.nodes[idx(&g, "src")].loc, g.nodes[idx(&g, "src/main.rs")].loc + g.nodes[idx(&g, "src/app.rs")].loc + g.nodes[idx(&g, "src/app/panels.rs")].loc + g.nodes[idx(&g, "src/config.rs")].loc + g.nodes[idx(&g, "src/validate/app_tests.rs")].loc);
    assert_eq!(g.nodes[idx(&g, "src/app/panels.rs")].top, Some(idx(&g, "src")));
    assert_eq!(g.nodes[idx(&g, "Cargo.toml")].top, None);

    // Rust
    assert!(has_edge(&g, "src/main.rs", "src/app.rs"), "mod app;");
    assert!(has_edge(&g, "src/main.rs", "src/config.rs"), "mod config;");
    assert!(has_edge(&g, "src/app.rs", "src/app/panels.rs"), "mod panels; from a non-mod.rs file");
    assert!(has_edge(&g, "src/app.rs", "src/config.rs"), "use super::config");
    assert!(has_edge(&g, "src/app.rs", "src/validate/app_tests.rs"), "#[path]");
    assert!(has_edge(&g, "src/app/panels.rs", "src/app.rs"), "use super::*");
    assert!(has_edge(&g, "src/app/panels.rs", "src/config.rs"), "use crate::config");
    assert_eq!(g.nodes[idx(&g, "src/main.rs")].external, 1, "std::io is external");
    // JS/TS
    assert!(has_edge(&g, "web/index.ts", "web/util.ts"));
    assert!(has_edge(&g, "web/index.ts", "web/components/index.tsx"));
    assert!(has_edge(&g, "web/index.ts", "web/legacy.ts"), ".js import of a .ts file");
    assert!(has_edge(&g, "web/index.ts", "web/data.json"));
    assert_eq!(g.nodes[idx(&g, "web/index.ts")].external, 1, "react");
    // Python
    assert!(has_edge(&g, "py/pkg/a.py", "py/pkg/b.py"));
    assert!(has_edge(&g, "py/pkg/a.py", "py/pkg/c.py"), "from . import c");
    assert!(has_edge(&g, "py/pkg/a.py", "py/pkg/d.py"), "absolute import from the package top");
    // C
    assert!(has_edge(&g, "c/main.c", "c/util.h"));
    assert!(has_edge(&g, "c/main.c", "include/api.h"));
    // Edge weights are per distinct pair and degrees add up.
    assert!(g.edges.iter().all(|e| e.from != e.to));
    assert_eq!(g.in_deg.iter().sum::<u32>(), g.edges.len() as u32);
    assert_eq!(g.out_deg[idx(&g, "web/index.ts")], 4);
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn scan_descriptions_and_hashes() {
    let d = fixture("desc");
    let g = scan(&d, 5000);
    assert_eq!(g.nodes[idx(&g, "src/main.rs")].desc.as_deref(), Some("Entry point. Starts the app."));
    assert_eq!(g.nodes[idx(&g, "src/app.rs")].desc.as_deref(), Some("The application."));
    assert_eq!(g.nodes[idx(&g, "src/config.rs")].desc.as_deref(), Some("Config loading."));
    assert_eq!(g.nodes[idx(&g, "web/util.ts")].desc.as_deref(), Some("Helpers for the web part."));
    assert_eq!(g.nodes[idx(&g, "py/pkg/__init__.py")].desc.as_deref(), Some("The package."));
    assert_eq!(g.nodes[idx(&g, "py/pkg/b.py")].desc.as_deref(), Some("Does b things. Second line."));
    assert_eq!(g.nodes[idx(&g, "py/pkg/c.py")].desc, None);
    assert_eq!(g.nodes[0].desc.as_deref(), Some("A tiny example project used by the graph tests."), "root gets the README");
    assert_eq!(g.nodes[0].desc_src, DescSource::Heuristic);
    // Directory hashes change when a file inside changes.
    let before = g.nodes[idx(&g, "src")].hash;
    std::fs::write(d.join("src/config.rs"), "/* Changed. */\n").unwrap();
    let g2 = scan(&d, 5000);
    assert_ne!(g2.nodes[idx(&g2, "src")].hash, before);
    assert_eq!(g2.nodes[idx(&g2, "web")].hash, g.nodes[idx(&g, "web")].hash);
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn scan_collapses_big_projects_to_directories() {
    let d = fixture("collapse");
    let g = scan(&d, 3);
    assert!(g.collapsed);
    assert!(g.nodes.iter().all(|n| n.is_dir()));
    assert!(g.file_count > 3);
    assert!(has_edge(&g, "src", "src/app"), "file edges are folded into directory edges");
    assert!(has_edge(&g, "c", "include"));
    assert!(g.edges.iter().all(|e| e.from != e.to));
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn layout_settles_and_keeps_clusters_together() {
    let d = fixture("layout");
    let mut g = scan(&d, 5000);
    let mut l = Layout::default();
    l.seed(&mut g);
    let mut converged = false;
    for _ in 0..40 {
        if l.step(&mut g, 50) {
            converged = true;
            break;
        }
    }
    assert!(converged, "did not settle in 2000 iterations");
    for n in &g.nodes {
        assert!(n.pos.x.is_finite() && n.pos.y.is_finite(), "{}", n.rel);
    }
    let files: Vec<usize> = (0..g.nodes.len()).filter(|&i| !g.nodes[i].is_dir()).collect();
    for (k, &i) in files.iter().enumerate() {
        for &j in &files[k + 1..] {
            assert!((g.nodes[i].pos - g.nodes[j].pos).length() > 1.0, "{} overlaps {}", g.nodes[i].rel, g.nodes[j].rel);
        }
    }
    // Files sit inside their directory's hull, and sibling hulls don't swallow each other's centres.
    for &dir in &g.dirs {
        let Some(&r) = l.hull.get(&dir) else { continue };
        for &i in &files {
            if g.nodes[i].dir == Some(dir) {
                assert!((g.nodes[i].pos - g.nodes[dir].pos).length() <= r, "{} outside {}", g.nodes[i].rel, g.nodes[dir].rel);
            }
        }
    }
    let (src, web) = (idx(&g, "src"), idx(&g, "web"));
    let dist = (g.nodes[src].pos - g.nodes[web].pos).length();
    assert!(dist > l.hull[&src].min(l.hull[&web]), "src and web clusters overlap: {dist}");
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn heuristics_skip_boilerplate() {
    assert_eq!(heuristic(Some("rust"), "// Copyright 2024 Someone\n// SPDX-License-Identifier: MIT\n\n// Parses things.\nfn x() {}\n").as_deref(), Some("Parses things."));
    assert_eq!(heuristic(Some("shellscript"), "#!/bin/sh\n# Builds the release.\necho\n").as_deref(), Some("Builds the release."));
    assert_eq!(heuristic(Some("rust"), "fn x() {}\n"), None);
    assert_eq!(heuristic(Some("python"), "#!/usr/bin/env python\n\"\"\"Multi\nline doc.\"\"\"\n").as_deref(), Some("Multi line doc."));
    let long = format!("//! {}\n", "word ".repeat(100));
    assert!(heuristic(Some("rust"), &long).unwrap().ends_with('…'));
    assert_eq!(readme_summary("# Title\n\n![badge](x)\n\nFirst para\ncontinues.\n\nSecond.\n").as_deref(), Some("First para continues."));
    assert_eq!(readme_summary("# Only a title\n"), None);
}

#[test]
fn cache_roundtrip_and_invalidation() {
    let d = fixture("cache");
    let mut c = Cache::load(&d);
    assert!(c.entries.is_empty());
    c.put("src/main.rs", 42, "Runs the app.".into(), Some("sonnet".into()));
    c.save(&d).unwrap();
    let c2 = Cache::load(&d);
    assert_eq!(c2.get("src/main.rs", 42), Some("Runs the app."));
    assert_eq!(c2.get("src/main.rs", 43), None, "a changed file drops its summary");
    assert_eq!(c2.get("src/other.rs", 42), None);
    assert_eq!(c2.entries["src/main.rs"].model.as_deref(), Some("sonnet"));
    let mut g = scan(&d, 5000);
    let i = idx(&g, "src/main.rs");
    let mut c3 = Cache::default();
    c3.put("src/main.rs", g.nodes[i].hash, "Cached.".into(), None);
    c3.apply(&mut g);
    assert_eq!(g.nodes[i].desc.as_deref(), Some("Cached."));
    assert_eq!(g.nodes[i].desc_src, DescSource::Claude);
    // The cache file itself is not part of the graph.
    let g2 = scan(&d, 5000);
    assert!(g2.by_rel(".rim/graph.json").is_none());
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn prompts_include_context() {
    let d = fixture("prompt");
    let g = scan(&d, 5000);
    let r = Request::for_node(&g, idx(&g, "src/main.rs"));
    let p = r.prompt();
    assert!(p.contains("Path: src/main.rs"));
    assert!(p.contains("Imports: ") && p.contains("src/app.rs"));
    assert!(p.contains("fn main() {}"));
    let r = Request::for_node(&g, idx(&g, "src"));
    let p = r.prompt();
    assert!(p.contains("Directory: src"));
    assert!(p.contains("- main.rs: Entry point. Starts the app."));
    assert!(p.contains("- app/"));
    let root = Request::for_node(&g, 0).prompt();
    assert!(root.contains("(project root)") && root.contains("README:"));
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn ask_once_runs_the_cli_headlessly() {
    let dir = std::env::temp_dir().join(format!("rim-validate-graph-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(
        &script,
        "#!/usr/bin/env python3\nimport sys\nif sys.argv[1:] == ['--help']:\n    print('Usage: claude [options]\\n  --tools <tools...>  tools'); sys.exit(0)\nprompt = sys.stdin.read()\nprint('ARGS=' + ' '.join(sys.argv[1:]))\nprint('LEN=%d' % len(prompt))\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = crate::claude::ask_once(&script, &dir, &"x".repeat(5000), Some("haiku"), std::time::Duration::from_secs(10)).unwrap();
    assert!(out.contains("ARGS=-p --output-format text --max-turns 1 --tools  --model haiku"), "{out}");
    assert!(out.contains("LEN=5000"));
    assert_eq!(crate::claude::ask_once_args(None, false), ["-p", "--output-format", "text", "--max-turns", "1"]);
    let err = crate::claude::ask_once(Path::new("/nonexistent/claude"), &dir, "x", None, std::time::Duration::from_secs(1)).unwrap_err();
    assert!(err.contains("could not start"));
    std::fs::remove_dir_all(dir).ok();
}

/// Prints layout statistics for this repository (a tuning aid, not a check).
#[test]
#[ignore]
fn layout_stats_for_rim() {
    let mut g = scan(Path::new(env!("CARGO_MANIFEST_DIR")), 5000);
    let mut l = Layout::default();
    l.seed(&mut g);
    let mut iters = 0;
    while !l.step(&mut g, 10) {
        iters += 10;
        if iters > 5000 {
            break;
        }
    }
    let files: Vec<usize> = (0..g.nodes.len()).filter(|&i| !g.nodes[i].is_dir()).collect();
    let mut nn = Vec::new();
    for &i in &files {
        let d = files.iter().filter(|&&j| j != i).map(|&j| (g.nodes[i].pos - g.nodes[j].pos).length()).fold(f32::MAX, f32::min);
        nn.push(d);
    }
    nn.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let edge: Vec<f32> = g.edges.iter().map(|e| (g.nodes[e.from].pos - g.nodes[e.to].pos).length()).collect();
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
    println!("iters={iters} temp={:.2} files={} nn: min={:.1} median={:.1} mean={:.1}  edge mean={:.1}  bounds={:?}", l.temp, files.len(), nn[0], nn[nn.len() / 2], mean(&nn), mean(&edge), super::layout::bounds(&g, &l).size());
    for &d in &g.dirs {
        println!("  {:12} hull={:.0} pos={:?}", if g.nodes[d].rel.is_empty() { "." } else { &g.nodes[d].rel }, l.hull.get(&d).copied().unwrap_or(0.0), g.nodes[d].pos);
    }
}
