//! Tree-sitter parsing: turn a source file into [`Symbol`]s and raw
//! [`ImportSpec`]s. Pure and synchronous (`02-architecture.md` §1.3 — logic is
//! sync, easy to test); the indexer ([`crate::store`]) is the only thing that
//! touches I/O around it.
//!
//! **Skip-with-record, never abort** (task quality bar, `09-lessons-learned.md`
//! L-L1): [`parse_file`] returns `None` when a grammar cannot be armed or the
//! source cannot be parsed at all, and the indexer records that as a parse
//! failure and moves on. Tree-sitter is error-tolerant, so a *syntactically
//! broken* file still yields a tree with `ERROR` nodes from which whatever
//! parsed is extracted best-effort — a broken file loses only its broken
//! regions, not the whole index batch.

use std::collections::HashMap;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::error::GraphError;
use crate::import::ImportSpec;
use crate::lang::Language;
use crate::symbol::{Symbol, SymbolKind};

/// Compiled grammars + queries for every supported language, built once and
/// shared by reference across the whole index (`Send + Sync`, so it lives
/// behind the [`crate::graph::CodeGraph`] handle and is reused by the
/// background watcher). Compiling the queries here — not per file — keeps
/// re-indexing cheap while still sourcing them from compile-time data
/// (`09-lessons-learned.md` L-L2).
pub(crate) struct Grammars {
    rust: LangPack,
    python: LangPack,
    javascript: LangPack,
    typescript: LangPack,
    tsx: LangPack,
}

struct LangPack {
    language: tree_sitter::Language,
    symbols: Query,
    imports: Query,
}

impl LangPack {
    fn load(lang: Language) -> Result<LangPack, GraphError> {
        let language = lang.ts_language();
        let symbols =
            Query::new(&language, lang.symbol_query()).map_err(|e| GraphError::Query {
                lang: lang.tag(),
                kind: "symbol",
                message: e.to_string(),
            })?;
        let imports =
            Query::new(&language, lang.import_query()).map_err(|e| GraphError::Query {
                lang: lang.tag(),
                kind: "import",
                message: e.to_string(),
            })?;
        Ok(LangPack {
            language,
            symbols,
            imports,
        })
    }
}

impl Grammars {
    /// Compile every grammar's query pair. Fails loudly only if one of the
    /// crate's own `.scm` strings does not compile — a programmer error the
    /// crate's tests catch.
    pub(crate) fn load() -> Result<Grammars, GraphError> {
        Ok(Grammars {
            rust: LangPack::load(Language::Rust)?,
            python: LangPack::load(Language::Python)?,
            javascript: LangPack::load(Language::JavaScript)?,
            typescript: LangPack::load(Language::TypeScript)?,
            tsx: LangPack::load(Language::Tsx)?,
        })
    }

    fn pack(&self, lang: Language) -> &LangPack {
        match lang {
            Language::Rust => &self.rust,
            Language::Python => &self.python,
            Language::JavaScript => &self.javascript,
            Language::TypeScript => &self.typescript,
            Language::Tsx => &self.tsx,
        }
    }
}

/// Everything extracted from one file.
pub(crate) struct Parsed {
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportSpec>,
}

/// Parse one file's `source`. `None` = un-armable grammar or wholly
/// unparseable input → the caller records a skip and continues.
pub(crate) fn parse_file(grammars: &Grammars, lang: Language, source: &str) -> Option<Parsed> {
    let pack = grammars.pack(lang);
    let mut parser = Parser::new();
    if parser.set_language(&pack.language).is_err() {
        return None;
    }
    let tree = parser.parse(source.as_bytes(), None)?;
    let root = tree.root_node();
    let src = source.as_bytes();

    let symbols = extract_symbols(&pack.symbols, root, src);
    let imports = match lang {
        Language::Rust => extract_rust_imports(&pack.imports, root, src),
        Language::Python => extract_python_imports(&pack.imports, root, src),
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            extract_ts_imports(&pack.imports, root, src)
        }
    };
    Some(Parsed { symbols, imports })
}

/// Decode symbol matches. A method is captured by both the general function
/// pattern and the enclosing-type pattern; dedup by the name node's byte
/// range and let the higher-[`SymbolKind::rank`] kind win.
fn extract_symbols(query: &Query, root: Node, src: &[u8]) -> Vec<Symbol> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut dedup: HashMap<(usize, usize), Symbol> = HashMap::new();

    let mut matches = cursor.matches(query, root, src);
    while let Some(m) = matches.next() {
        let mut name: Option<&str> = None;
        let mut name_range: Option<(usize, usize)> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut span: Option<(u32, u32)> = None;

        for cap in m.captures {
            let cap_name = names[cap.index as usize];
            if cap_name == "name" {
                name = cap.node.utf8_text(src).ok();
                name_range = Some((cap.node.start_byte(), cap.node.end_byte()));
            } else if let Some(k) = SymbolKind::from_capture(cap_name) {
                kind = Some(k);
                span = Some((
                    cap.node.start_position().row as u32 + 1,
                    cap.node.end_position().row as u32 + 1,
                ));
            }
        }

        if let (Some(name), Some(range), Some(kind), Some((start, end))) =
            (name, name_range, kind, span)
        {
            if name.is_empty() {
                continue;
            }
            let symbol = Symbol {
                name: name.to_string(),
                kind,
                start_line: start,
                end_line: end,
            };
            match dedup.get(&range) {
                Some(existing) if existing.kind.rank() >= kind.rank() => {}
                _ => {
                    dedup.insert(range, symbol);
                }
            }
        }
    }

    let mut out: Vec<Symbol> = dedup.into_values().collect();
    out.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn extract_rust_imports(query: &Query, root: Node, src: &[u8]) -> Vec<ImportSpec> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(query, root, src);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            if names[cap.index as usize] == "use"
                && let Ok(text) = cap.node.utf8_text(src)
            {
                out.push(ImportSpec::RustUse {
                    specifier: text.to_string(),
                });
            }
        }
    }
    out
}

fn extract_ts_imports(query: &Query, root: Node, src: &[u8]) -> Vec<ImportSpec> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(query, root, src);
    while let Some(m) = matches.next() {
        let mut source_text: Option<&str> = None;
        let mut callee: Option<&str> = None;
        for cap in m.captures {
            match names[cap.index as usize] {
                "source" => source_text = cap.node.utf8_text(src).ok(),
                "callee" => callee = cap.node.utf8_text(src).ok(),
                _ => {}
            }
        }
        let Some(specifier) = source_text else {
            continue;
        };
        // A match carrying a @callee is an arbitrary `f('str')` call — keep it
        // only when the callee is `require`; every other pattern (import /
        // export-from / dynamic import) carries no @callee and is always a
        // real specifier.
        if let Some(callee) = callee
            && callee != "require"
        {
            continue;
        }
        out.push(classify_ts_specifier(specifier));
    }
    out
}

fn classify_ts_specifier(specifier: &str) -> ImportSpec {
    if specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier == "."
        || specifier == ".."
    {
        ImportSpec::TsRelative {
            specifier: specifier.to_string(),
        }
    } else {
        ImportSpec::Bare {
            specifier: specifier.to_string(),
        }
    }
}

fn extract_python_imports(query: &Query, root: Node, src: &[u8]) -> Vec<ImportSpec> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(query, root, src);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            match names[cap.index as usize] {
                "import" => decode_py_import(cap.node, src, &mut out),
                "from_import" => decode_py_from_import(cap.node, src, &mut out),
                _ => {}
            }
        }
    }
    out
}

/// `import a`, `import a.b`, `import a as b`, `import a, b` — all absolute,
/// recorded unresolved.
fn decode_py_import(node: Node, src: &[u8], out: &mut Vec<ImportSpec>) {
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        if let Some(module) = py_module_name(child, src) {
            out.push(ImportSpec::PyAbsolute { specifier: module });
        }
    }
}

/// `from <module> import <names>` — the relative-import decode the spec asked
/// for. Counts the leading dots (`import_prefix`) to a package level and
/// carries the optional dotted module path plus imported names to
/// [`crate::import::resolve`].
fn decode_py_from_import(node: Node, src: &[u8], out: &mut Vec<ImportSpec>) {
    let module = node.child_by_field_name("module_name");
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        if let Some(name) = py_module_name(child, src) {
            names.push(name);
        }
    }

    match module {
        Some(m) if m.kind() == "relative_import" => {
            let mut level = 0usize;
            let mut module_path: Option<String> = None;
            let mut c = m.walk();
            for child in m.children(&mut c) {
                match child.kind() {
                    "import_prefix" => {
                        level = child
                            .utf8_text(src)
                            .map(|t| t.chars().filter(|c| *c == '.').count())
                            .unwrap_or(1);
                    }
                    "dotted_name" => {
                        module_path = child.utf8_text(src).ok().map(|s| s.to_string());
                    }
                    _ => {}
                }
            }
            out.push(ImportSpec::PyRelative {
                level: level.max(1),
                module: module_path,
                names,
                text: m.utf8_text(src).unwrap_or(".").to_string(),
            });
        }
        Some(m) if m.kind() == "dotted_name" => {
            if let Ok(text) = m.utf8_text(src) {
                out.push(ImportSpec::PyAbsolute {
                    specifier: text.to_string(),
                });
            }
        }
        _ => {}
    }
}

/// The module name of an imported item: the aliased original for
/// `x as y`, otherwise the node's own text.
fn py_module_name(node: Node, src: &[u8]) -> Option<String> {
    let target = if node.kind() == "aliased_import" {
        node.child_by_field_name("name")?
    } else {
        node
    };
    target.utf8_text(src).ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::SymbolKind;

    fn parse(lang: Language, src: &str) -> Parsed {
        let grammars = Grammars::load().expect("grammars compile");
        parse_file(&grammars, lang, src).expect("source parses")
    }

    fn kinds(parsed: &Parsed, name: &str) -> Vec<SymbolKind> {
        parsed
            .symbols
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.kind)
            .collect()
    }

    #[test]
    fn all_queries_compile() {
        // Guards the compile-time .scm strings (L-L2): a mis-edit fails here,
        // not at a host's runtime.
        Grammars::load().expect("every language query compiles");
    }

    #[test]
    fn rust_symbols_with_method_precedence() {
        let src = "\
pub struct Widget { id: u32 }
pub enum Color { Red, Green }
pub trait Draw { fn draw(&self); }
impl Widget {
    pub fn new() -> Self { Widget { id: 0 } }
    fn helper(&self) -> u32 { self.id }
}
pub fn run() {}
";
        let parsed = parse(Language::Rust, src);
        assert_eq!(kinds(&parsed, "Widget"), vec![SymbolKind::Struct]);
        assert_eq!(kinds(&parsed, "Color"), vec![SymbolKind::Enum]);
        assert_eq!(kinds(&parsed, "Draw"), vec![SymbolKind::Trait]);
        assert_eq!(kinds(&parsed, "run"), vec![SymbolKind::Function]);
        // Impl methods are double-captured; Method wins over the general fn.
        assert_eq!(kinds(&parsed, "new"), vec![SymbolKind::Method]);
        assert_eq!(kinds(&parsed, "helper"), vec![SymbolKind::Method]);
        // A trait method *signature* is a plain function, not an impl method.
        assert_eq!(kinds(&parsed, "draw"), vec![SymbolKind::Function]);
        // Spans are 1-based and non-degenerate.
        let widget = parsed.symbols.iter().find(|s| s.name == "Widget").unwrap();
        assert_eq!(widget.start_line, 1);
    }

    #[test]
    fn python_symbols_and_relative_import_decode() {
        let src = "\
import os
from . import helper
from .util import thing
from ..pkg import y

class Widget:
    def method_a(self):
        pass

def top():
    pass
";
        let parsed = parse(Language::Python, src);
        assert_eq!(kinds(&parsed, "Widget"), vec![SymbolKind::Class]);
        assert_eq!(kinds(&parsed, "method_a"), vec![SymbolKind::Method]);
        assert_eq!(kinds(&parsed, "top"), vec![SymbolKind::Function]);

        // `import os` → absolute.
        assert!(
            parsed
                .imports
                .iter()
                .any(|i| matches!(i, ImportSpec::PyAbsolute { specifier } if specifier == "os"))
        );
        // `from . import helper` → level 1, no module path, name `helper`.
        assert!(parsed.imports.iter().any(|i| matches!(
            i,
            ImportSpec::PyRelative { level: 1, module: None, names, .. } if names.iter().any(|n| n == "helper")
        )));
        // `from .util import thing` → level 1, module `util`.
        assert!(parsed.imports.iter().any(|i| matches!(
            i,
            ImportSpec::PyRelative { level: 1, module: Some(m), .. } if m == "util"
        )));
        // `from ..pkg import y` → level 2, module `pkg` (the multi-dot case).
        assert!(parsed.imports.iter().any(|i| matches!(
            i,
            ImportSpec::PyRelative { level: 2, module: Some(m), .. } if m == "pkg"
        )));
    }

    #[test]
    fn typescript_symbols_and_imports() {
        let src = "\
import { a } from './util';
import React from 'react';
export function boot() {}
export const arrow = () => {};
export class App { run() {} }
export interface Shape { x: number }
export enum E { A }
";
        let parsed = parse(Language::TypeScript, src);
        assert_eq!(kinds(&parsed, "App"), vec![SymbolKind::Class]);
        assert_eq!(kinds(&parsed, "boot"), vec![SymbolKind::Function]);
        assert_eq!(kinds(&parsed, "arrow"), vec![SymbolKind::Function]);
        assert_eq!(kinds(&parsed, "run"), vec![SymbolKind::Method]);
        assert_eq!(kinds(&parsed, "Shape"), vec![SymbolKind::Interface]);
        assert_eq!(kinds(&parsed, "E"), vec![SymbolKind::Enum]);

        assert!(
            parsed.imports.iter().any(
                |i| matches!(i, ImportSpec::TsRelative { specifier } if specifier == "./util")
            )
        );
        assert!(
            parsed
                .imports
                .iter()
                .any(|i| matches!(i, ImportSpec::Bare { specifier } if specifier == "react"))
        );
    }

    #[test]
    fn javascript_require_and_non_require_calls() {
        let src = "\
const { add } = require('./math');
function noise() { helper('not-a-require'); }
class R { go() {} }
";
        let parsed = parse(Language::JavaScript, src);
        // Only the real require() call is recorded as an import.
        let import_count = parsed
            .imports
            .iter()
            .filter(|i| matches!(i, ImportSpec::TsRelative { .. } | ImportSpec::Bare { .. }))
            .count();
        assert_eq!(
            import_count, 1,
            "helper('..') is not require and must be ignored"
        );
        assert!(
            parsed.imports.iter().any(
                |i| matches!(i, ImportSpec::TsRelative { specifier } if specifier == "./math")
            )
        );
        assert_eq!(kinds(&parsed, "go"), vec![SymbolKind::Method]);
    }

    #[test]
    fn a_syntactically_broken_file_still_extracts_what_parsed() {
        // Tree-sitter is error-tolerant: a broken region must not lose the
        // valid symbols around it (skip-with-record, never abort).
        let src = "pub fn good() {}\npub fn (((broken\npub struct Ok;\n";
        let parsed = parse(Language::Rust, src);
        assert!(parsed.symbols.iter().any(|s| s.name == "good"));
        assert!(parsed.symbols.iter().any(|s| s.name == "Ok"));
    }
}
