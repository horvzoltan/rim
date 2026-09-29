//! Import extraction from tree-sitter parse trees (for the languages with a bundled grammar):
//! comments, strings and odd formatting can't fool it the way a line scan can.

use tree_sitter::{Node, Parser};

use super::scan::{ImportRef, is_js_relative, rust_path, rust_use_paths};
use crate::highlight::ts;

/// True when the language id has a grammar here.
pub fn supports(lang: &str) -> bool {
    grammar_name(lang).is_some()
}

fn grammar_name(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "rust" => "rust",
        "javascript" | "javascriptreact" => "javascript",
        "typescript" => "typescript",
        "typescriptreact" => "tsx",
        "python" => "python",
        "c" => "c",
        "cpp" => "cpp",
        "go" => "go",
        _ => return None,
    })
}

/// Imports of `text` in document order.
pub fn extract(lang: &str, text: &str, out: &mut Vec<ImportRef>) {
    let Some(name) = grammar_name(lang) else { return };
    let mut parser = Parser::new();
    parser.set_language(&ts::language(name).expect("bundled grammar")).expect("grammar matches tree-sitter");
    let Some(tree) = parser.parse(text, None) else { return };
    let src = text.as_bytes();
    // Pre-order walk, so imports come out in the order they appear.
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match name {
            "rust" => rust(n, src, out),
            "javascript" | "typescript" | "tsx" => js(n, src, out),
            "python" => python(n, src, out),
            "c" | "cpp" => c(n, src, out),
            "go" => go(n, src, out),
            _ => {}
        }
        for i in (0..n.named_child_count()).rev() {
            if let Some(c) = n.named_child(i as u32) {
                stack.push(c);
            }
        }
    }
}

/// Imports of the `<script>` blocks of a single-file component (Vue, Svelte), parsed as TSX.
pub fn extract_script_blocks(text: &str, out: &mut Vec<ImportRef>) {
    let mut rest = text;
    while let Some(open) = rest.find("<script") {
        let after = &rest[open..];
        let Some(gt) = after.find('>') else { break };
        let body = &after[gt + 1..];
        let end = body.find("</script").unwrap_or(body.len());
        extract("typescriptreact", &body[..end], out);
        rest = &body[end..];
    }
}

fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

/// The contents of a string literal node (its inner fragment, or the text without the quotes).
fn string_contents<'a>(n: Node, src: &'a [u8]) -> &'a str {
    let t = text(n, src);
    t.trim_matches(|c| c == '"' || c == '\'' || c == '`')
}

fn rust(n: Node, src: &[u8], out: &mut Vec<ImportRef>) {
    match n.kind() {
        "mod_item" if n.child_by_field_name("body").is_none() => {
            let Some(name) = n.child_by_field_name("name") else { return };
            // `#[path = "…"]` right before the declaration names the file.
            let path = n.prev_named_sibling().filter(|p| p.kind() == "attribute_item").and_then(|p| {
                let attr = p.named_child(0)?;
                let key = attr.named_child(0)?;
                (text(key, src) == "path").then(|| attr.child_by_field_name("value")).flatten()
            });
            match path {
                Some(v) => out.push(ImportRef::Relative(string_contents(v, src).to_string())),
                None => out.push(ImportRef::RustMod(text(name, src).to_string())),
            }
        }
        "use_declaration" => {
            let Some(arg) = n.child_by_field_name("argument") else { return };
            for p in rust_use_paths(text(arg, src)) {
                if let Some(r) = rust_path(p) {
                    out.push(r);
                }
            }
        }
        _ => {}
    }
}

fn push_js(spec: &str, out: &mut Vec<ImportRef>) {
    if is_js_relative(spec) { out.push(ImportRef::Relative(spec.to_string())) } else { out.push(ImportRef::External(spec.to_string())) }
}

fn js(n: Node, src: &[u8], out: &mut Vec<ImportRef>) {
    match n.kind() {
        "import_statement" | "export_statement" => {
            let source = n.child_by_field_name("source").or_else(|| {
                // `import x = require("…")`
                (0..n.named_child_count()).filter_map(|i| n.named_child(i as u32)).find(|c| c.kind() == "import_require_clause").and_then(|c| c.child_by_field_name("source"))
            });
            if let Some(s) = source {
                push_js(string_contents(s, src), out);
            }
        }
        "call_expression" => {
            let Some(f) = n.child_by_field_name("function") else { return };
            let is_require = f.kind() == "identifier" && text(f, src) == "require";
            if !(is_require || f.kind() == "import") {
                return;
            }
            // Only a string literal directly inside the parens.
            if let Some(arg) = n.child_by_field_name("arguments").and_then(|a| a.named_child(0))
                && arg.kind() == "string"
            {
                push_js(string_contents(arg, src), out);
            }
        }
        _ => {}
    }
}

fn dotted(n: Node, src: &[u8]) -> Vec<String> {
    text(n, src).split('.').filter(|s| !s.is_empty()).map(String::from).collect()
}

fn python(n: Node, src: &[u8], out: &mut Vec<ImportRef>) {
    let mut cursor = n.walk();
    match n.kind() {
        "import_statement" => {
            for c in n.children_by_field_name("name", &mut cursor) {
                let target = if c.kind() == "aliased_import" { c.child_by_field_name("name") } else { Some(c) };
                if let Some(t) = target {
                    out.push(ImportRef::Py { levels: 0, segs: dotted(t, src), names: vec![] });
                }
            }
        }
        "import_from_statement" => {
            let Some(module) = n.child_by_field_name("module_name") else { return };
            let (levels, segs) = if module.kind() == "relative_import" {
                let mut levels = 0;
                let mut segs = vec![];
                for i in 0..module.named_child_count() {
                    let Some(c) = module.named_child(i as u32) else { continue };
                    match c.kind() {
                        "import_prefix" => levels = text(c, src).chars().filter(|ch| *ch == '.').count(),
                        "dotted_name" => segs = dotted(c, src),
                        _ => {}
                    }
                }
                (levels, segs)
            } else {
                (0, dotted(module, src))
            };
            let names = n
                .children_by_field_name("name", &mut cursor)
                .filter_map(|c| if c.kind() == "aliased_import" { c.child_by_field_name("name") } else { Some(c) })
                .map(|c| text(c, src).to_string())
                .filter(|s| !s.is_empty())
                .collect();
            out.push(ImportRef::Py { levels, segs, names });
        }
        _ => {}
    }
}

fn c(n: Node, src: &[u8], out: &mut Vec<ImportRef>) {
    if n.kind() != "preproc_include" {
        return;
    }
    let Some(p) = n.child_by_field_name("path") else { return };
    match p.kind() {
        "string_literal" => out.push(ImportRef::Relative(string_contents(p, src).to_string())),
        "system_lib_string" => out.push(ImportRef::External(text(p, src).trim_matches(|c| c == '<' || c == '>').to_string())),
        _ => {}
    }
}

fn go(n: Node, src: &[u8], out: &mut Vec<ImportRef>) {
    if n.kind() == "import_spec"
        && let Some(p) = n.child_by_field_name("path")
    {
        out.push(ImportRef::GoPkg(string_contents(p, src).to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::super::scan::{RustKind, extract_imports};
    use super::*;

    #[test]
    fn parse_trees_beat_line_scans() {
        // A `use` inside a block comment, a string that looks like an import, and a
        // multi-line `mod` attribute are all handled by the grammar.
        let rust = "/* use crate::nope; */\nconst S: &str = \"mod fake;\";\n#[cfg(test)]\nmod tests;\nmod inline { use super::x; }\n";
        let r = extract_imports("rust", rust);
        assert_eq!(r, vec![ImportRef::RustMod("tests".into()), ImportRef::RustPath { kind: RustKind::Super(1), segs: vec!["x".into()] }]);
        let ts = "const s = `import a from './no'`;\n/* import b from './no' */\nimport type { T } from './t';\nimport z = require('./z');\nimport(\n  './lazy'\n);\n";
        let t = extract_imports("typescript", ts);
        let rel = |s: &str| ImportRef::Relative(s.into());
        assert_eq!(t, vec![rel("./t"), rel("./z"), rel("./lazy")]);
        let py = "s = 'from x import y'\nfrom .a import (b,\n    c as d)\n";
        assert_eq!(extract_imports("python", py), vec![ImportRef::Py { levels: 1, segs: vec!["a".into()], names: vec!["b".into(), "c".into()] }]);
        let cpp = "// #include \"no.h\"\n#include \"a.hpp\"\n#include <vector>\n";
        assert_eq!(extract_imports("cpp", cpp), vec![rel("a.hpp"), ImportRef::External("vector".into())]);
        let vue = "<template><p>import x from './no'</p></template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue';\nimport A from './A.vue';\n</script>\n";
        assert_eq!(extract_imports("vue", vue), vec![ImportRef::External("vue".into()), rel("./A.vue")]);
    }
}
