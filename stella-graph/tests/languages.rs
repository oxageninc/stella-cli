//! End-to-end indexing over real fixture files in tempdirs: symbol extraction
//! per language, plus the two import-resolution cases the spec calls out —
//! Python relative imports (`from . import x`, `from ..pkg import y`) and TS
//! `index.ts` resolution (`03-plan.md` Phase 3 item 3).

use std::fs;
use std::path::Path;

use stella_graph::{CodeGraph, ContextFrame};
use tempfile::TempDir;

/// A fixture workspace plus a db kept in a *separate* tempdir, so the store's
/// own files never appear in the indexed tree.
struct Fixture {
    _ws: TempDir,
    _dbdir: TempDir,
    graph: CodeGraph,
}

impl Fixture {
    fn build(files: &[(&str, &str)]) -> Fixture {
        let ws = TempDir::new().unwrap();
        let dbdir = TempDir::new().unwrap();
        for (rel, content) in files {
            let path = ws.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();
        graph.index_all().unwrap();
        Fixture {
            _ws: ws,
            _dbdir: dbdir,
            graph,
        }
    }

    fn def_labels(&self, name: &str) -> Vec<String> {
        labels(&self.graph.definitions(name).unwrap())
    }

    /// Resolved + unresolved target labels of a file's imports.
    fn import_targets(&self, rel: &str) -> Vec<String> {
        self.graph
            .imports_of(Path::new(rel))
            .unwrap()
            .into_iter()
            .flat_map(|f| f.relations)
            .filter_map(|r| r.display_name)
            .collect()
    }
}

fn labels(frames: &[ContextFrame]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|f| f.citation_label.clone())
        .collect()
}

fn has_label_starting(labels: &[String], prefix: &str) -> bool {
    labels.iter().any(|l| l.starts_with(prefix))
}

#[test]
fn rust_symbols_indexed() {
    let fx = Fixture::build(&[(
        "src/lib.rs",
        "pub struct Widget { id: u32 }\n\
         pub enum Color { Red }\n\
         pub trait Draw { fn draw(&self); }\n\
         impl Widget { pub fn new() -> Self { Widget { id: 0 } } }\n\
         pub fn run() {}\n",
    )]);
    assert!(has_label_starting(
        &fx.def_labels("Widget"),
        "struct Widget ("
    ));
    assert!(has_label_starting(&fx.def_labels("Color"), "enum Color ("));
    assert!(has_label_starting(&fx.def_labels("Draw"), "trait Draw ("));
    assert!(has_label_starting(&fx.def_labels("run"), "fn run ("));
    assert!(has_label_starting(&fx.def_labels("new"), "fn new ("));
}

#[test]
fn python_relative_imports_resolve_to_files() {
    // `from . import sibling` and `from ..pkg import y` must resolve to actual
    // files (the spec's explicitly-requested fix).
    let fx = Fixture::build(&[
        ("a/__init__.py", ""),
        ("a/pkg.py", "def y():\n    return 1\n"),
        ("a/b/__init__.py", ""),
        ("a/b/sibling.py", "def s():\n    return 2\n"),
        (
            "a/b/mod.py",
            "from ..pkg import y\nfrom . import sibling\nimport os\n\nclass Handler:\n    def handle(self):\n        pass\n",
        ),
    ]);

    assert!(has_label_starting(
        &fx.def_labels("Handler"),
        "class Handler ("
    ));
    assert!(has_label_starting(&fx.def_labels("handle"), "fn handle ("));

    let targets = fx.import_targets("a/b/mod.py");
    assert!(
        targets.iter().any(|t| t == "a/pkg.py"),
        "`from ..pkg import y` should resolve to a/pkg.py; got {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t == "a/b/sibling.py"),
        "`from . import sibling` should resolve to a/b/sibling.py; got {targets:?}"
    );
    // `import os` is absolute → recorded unresolved (as its specifier).
    assert!(targets.iter().any(|t| t == "os"));

    // Reverse edge: pkg.py knows a/b/mod.py imports it.
    let importers: Vec<String> = fx
        .graph
        .importers_of(Path::new("a/pkg.py"))
        .unwrap()
        .into_iter()
        .flat_map(|f| f.relations)
        .filter_map(|r| r.display_name)
        .collect();
    assert!(importers.iter().any(|i| i == "a/b/mod.py"));
}

#[test]
fn typescript_index_and_relative_imports_resolve() {
    let fx = Fixture::build(&[
        ("src/index.ts", "export const root = 1;\n"),
        ("src/util/index.ts", "export function u() {}\n"),
        (
            "src/app.ts",
            "import { u } from './util';\n\
             import { root } from './index';\n\
             import React from 'react';\n\
             export class App { run() {} }\n\
             export function boot() {}\n",
        ),
    ]);

    assert!(has_label_starting(&fx.def_labels("App"), "class App ("));
    assert!(has_label_starting(&fx.def_labels("boot"), "fn boot ("));

    let targets = fx.import_targets("src/app.ts");
    assert!(
        targets.iter().any(|t| t == "src/util/index.ts"),
        "'./util' should resolve to src/util/index.ts; got {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t == "src/index.ts"),
        "'./index' should resolve to src/index.ts; got {targets:?}"
    );
    // Bare package specifier stays unresolved-but-recorded.
    assert!(targets.iter().any(|t| t == "react"));
}

#[test]
fn javascript_require_resolves_relative() {
    let fx = Fixture::build(&[
        (
            "lib/math.js",
            "function add(a, b) { return a + b; }\nmodule.exports = { add };\n",
        ),
        (
            "lib/main.js",
            "const { add } = require('./math');\nclass Runner { go() {} }\n",
        ),
    ]);
    assert!(has_label_starting(
        &fx.def_labels("Runner"),
        "class Runner ("
    ));
    assert!(has_label_starting(&fx.def_labels("go"), "fn go ("));
    assert!(has_label_starting(&fx.def_labels("add"), "fn add ("));

    let targets = fx.import_targets("lib/main.js");
    assert!(
        targets.iter().any(|t| t == "lib/math.js"),
        "require('./math') should resolve to lib/math.js; got {targets:?}"
    );
}

#[test]
fn byte_compat_skip_across_two_index_passes() {
    let ws = TempDir::new().unwrap();
    let dbdir = TempDir::new().unwrap();
    fs::write(ws.path().join("a.rs"), "pub fn a() {}\n").unwrap();
    fs::write(ws.path().join("b.ts"), "export function b() {}\n").unwrap();
    let graph = CodeGraph::open(ws.path(), &dbdir.path().join("context.db")).unwrap();

    let first = graph.index_all().unwrap();
    assert_eq!(first.files_parsed, 2);

    let second = graph.index_all().unwrap();
    assert_eq!(
        second.files_parsed, 0,
        "unchanged files must not re-parse (L-C2)"
    );
    assert_eq!(second.files_skipped_unchanged, 2);
}

#[test]
fn ignored_directories_are_not_indexed() {
    let fx = Fixture::build(&[
        ("src/real.rs", "pub fn real() {}\n"),
        (
            "node_modules/pkg/index.js",
            "export function vendored() {}\n",
        ),
        ("target/debug/gen.rs", "pub fn generated() {}\n"),
        (".hidden/secret.py", "def secret():\n    pass\n"),
    ]);
    assert!(has_label_starting(&fx.def_labels("real"), "fn real ("));
    assert!(
        fx.def_labels("vendored").is_empty(),
        "node_modules must be skipped"
    );
    assert!(
        fx.def_labels("generated").is_empty(),
        "target/ must be skipped"
    );
    assert!(
        fx.def_labels("secret").is_empty(),
        "hidden dirs must be skipped"
    );
}
