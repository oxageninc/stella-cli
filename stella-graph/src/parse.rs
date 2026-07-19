//! Tree-sitter parsing: turn a source file into [`Symbol`]s and raw
//! [`ImportSpec`]s. Pure and synchronous (logic is
//! sync, easy to test); the indexer ([`crate::store`]) is the only thing that
//! touches I/O around it.
//!
//! **Skip-with-record, never abort** (task quality bar,
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
/// ( L-L2).
pub(crate) struct Grammars {
    rust: LangPack,
    python: LangPack,
    javascript: LangPack,
    typescript: LangPack,
    tsx: LangPack,
    sql: LangPack,
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
            sql: LangPack::load(Language::Sql)?,
        })
    }

    fn pack(&self, lang: Language) -> &LangPack {
        match lang {
            Language::Rust => &self.rust,
            Language::Python => &self.python,
            Language::JavaScript => &self.javascript,
            Language::TypeScript => &self.typescript,
            Language::Tsx => &self.tsx,
            Language::Sql => &self.sql,
        }
    }
}

/// Everything extracted from one file.
pub(crate) struct Parsed {
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportSpec>,
}

/// Parse source into a raw tree for a storage adapter's structural walk
/// ([`crate::storage`]). `None` = un-armable grammar or wholly unparseable
/// input, same contract as [`parse_file`].
pub(crate) fn parse_tree(
    grammars: &Grammars,
    lang: Language,
    source: &str,
) -> Option<tree_sitter::Tree> {
    let pack = grammars.pack(lang);
    let mut parser = Parser::new();
    if parser.set_language(&pack.language).is_err() {
        return None;
    }
    parser.parse(source.as_bytes(), None)
}

/// Parse SQL source into a raw tree for the SQL adapter.
pub(crate) fn parse_sql_tree(grammars: &Grammars, source: &str) -> Option<tree_sitter::Tree> {
    parse_tree(grammars, Language::Sql, source)
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

    let mut symbols = extract_symbols(&pack.symbols, root, src);

    // ORM pattern detection: scan the AST for table-like definitions
    // (Diesel `table!` macros, Django/SQLAlchemy model classes) and add
    // them as Table symbols. SQL DDL is the ground truth; these are hints.
    match lang {
        Language::Rust => symbols.extend(extract_rust_orm_tables(root, src)),
        Language::Python => symbols.extend(extract_python_orm_tables(root, src)),
        _ => {}
    }

    let imports = match lang {
        Language::Rust => extract_rust_imports(&pack.imports, root, src),
        Language::Python => extract_python_imports(&pack.imports, root, src),
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            extract_ts_imports(&pack.imports, root, src)
        }
        Language::Sql => Vec::new(), // SQL has no imports
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

/// Detect Diesel `table!` macro invocations and extract them as Table symbols.
/// The macro looks like: `diesel::table! { users (id) { ... } }` or `table! { ... }`.
/// We extract the first identifier inside the token_tree as the table name.
fn extract_rust_orm_tables(root: Node, src: &[u8]) -> Vec<Symbol> {
    let mut out = Vec::new();

    fn walk(node: Node, src: &[u8], out: &mut Vec<Symbol>) {
        if node.kind() == "macro_invocation"
            && let Some(macro_id) = node.child_by_field_name("macro")
            && let Ok(name) = macro_id.utf8_text(src)
        {
            let name_lower = name.to_ascii_lowercase();
            if name_lower == "table" || name_lower.ends_with("::table") {
                let mut tc = node.walk();
                for child in node.children(&mut tc) {
                    if child.kind() == "token_tree" {
                        let mut inner_tc = child.walk();
                        for inner in child.children(&mut inner_tc) {
                            if inner.kind() == "identifier"
                                && let Ok(table_name) = inner.utf8_text(src)
                                && !table_name.is_empty()
                            {
                                out.push(Symbol {
                                    name: table_name.to_string(),
                                    kind: SymbolKind::Table,
                                    start_line: node.start_position().row as u32 + 1,
                                    end_line: node.end_position().row as u32 + 1,
                                });
                            }
                        }
                    }
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, src, out);
        }
    }

    walk(root, src, &mut out);
    out
}

/// Detect Django/SQLAlchemy model classes and extract them as Table symbols.
/// Django: `class Payment(models.Model):` — superclass contains `Model`.
/// SQLAlchemy: `class Payment(Base):` with `__tablename__ = "payments"`.
fn extract_python_orm_tables(root: Node, src: &[u8]) -> Vec<Symbol> {
    let mut out = Vec::new();

    fn walk(node: Node, src: &[u8], out: &mut Vec<Symbol>) {
        if node.kind() == "class_definition" {
            let tablename = python_tablename_value(node, src);
            let is_model = tablename.is_some() || check_python_orm_superclass(node, src);
            if is_model
                && let Some(name_node) = node.child_by_field_name("name")
                && let Ok(name) = name_node.utf8_text(src)
            {
                let table_name = tablename.unwrap_or_else(|| python_class_to_table_name(name));
                out.push(Symbol {
                    name: table_name,
                    kind: SymbolKind::Table,
                    start_line: node.start_position().row as u32 + 1,
                    end_line: node.end_position().row as u32 + 1,
                });
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, src, out);
        }
    }

    walk(root, src, &mut out);
    out
}

/// Check if a Python class inherits from a known ORM base class.
/// Matches the base expression's innermost identifier exactly (`Model`,
/// `models.Model`, `Base`, `sqlalchemy.orm.Base`, `declarative_base()`) so
/// unrelated types that merely end in the same substring — `ViewModel`,
/// `DatabaseModel` — are not mistaken for ORM bases.
fn check_python_orm_superclass(class_node: Node, src: &[u8]) -> bool {
    let Some(superclasses) = class_node.child_by_field_name("superclasses") else {
        return false;
    };
    let mut cursor = superclasses.walk();
    for arg in superclasses.children(&mut cursor) {
        if let Ok(text) = arg.utf8_text(src) {
            let ident = python_base_identifier(text.trim());
            if ident == "Model" || ident == "Base" || ident == "declarative_base" {
                return true;
            }
        }
    }
    false
}

/// The innermost identifier of a (possibly qualified, possibly called) base
/// class expression: `models.Model` -> `Model`, `declarative_base()` ->
/// `declarative_base`.
fn python_base_identifier(text: &str) -> &str {
    let text = text.strip_suffix("()").unwrap_or(text);
    text.rsplit('.').next().unwrap_or(text)
}

/// Extract the string value of `__tablename__ = "..."` from a Python class
/// body, if present.
fn python_tablename_value(class_node: Node, src: &[u8]) -> Option<String> {
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for stmt in body.children(&mut cursor) {
        // Class-body statements are wrapped in `expression_statement`; the
        // `assignment` itself is an unnamed child of that wrapper.
        let assignment = if stmt.kind() == "assignment" {
            Some(stmt)
        } else if stmt.kind() == "expression_statement" {
            let mut inner = stmt.walk();
            stmt.children(&mut inner).find(|c| c.kind() == "assignment")
        } else {
            None
        };
        let Some(assignment) = assignment else {
            continue;
        };
        if let Some(left) = assignment.child_by_field_name("left")
            && let Ok(text) = left.utf8_text(src)
            && text == "__tablename__"
            && let Some(right) = assignment.child_by_field_name("right")
        {
            return python_string_literal_value(right, src);
        }
    }
    None
}

/// The inner text of a Python string literal node, quotes stripped.
fn python_string_literal_value(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string_content" {
            return child.utf8_text(src).ok().map(|s| s.to_string());
        }
    }
    None
}

/// Naive Django-style class→table conversion: CamelCase → snake_case + plural.
/// `Payment` → `payments`, `UserProfile` → `user_profiles`.
fn python_class_to_table_name(name: &str) -> String {
    let mut snake = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            snake.push('_');
        }
        snake.push(ch.to_ascii_lowercase());
    }
    // Naive pluralization
    if snake.ends_with('s') {
        format!("{snake}es")
    } else {
        format!("{snake}s")
    }
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

    #[test]
    fn sql_create_table_extracts_table_and_columns() {
        let src = "\
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    created_at TIMESTAMP DEFAULT NOW()
);

CREATE TABLE payments (
    id SERIAL PRIMARY KEY,
    amount NUMERIC(10,2) NOT NULL,
    user_id INTEGER REFERENCES users(id)
);

CREATE TYPE payment_status AS ENUM ('pending', 'completed', 'failed');

CREATE VIEW active_payments AS SELECT * FROM payments WHERE amount > 0;
";
        let parsed = parse(Language::Sql, src);

        // Tables
        assert_eq!(kinds(&parsed, "users"), vec![SymbolKind::Table]);
        assert_eq!(kinds(&parsed, "payments"), vec![SymbolKind::Table]);

        // Columns
        assert!(
            parsed
                .symbols
                .iter()
                .any(|s| { s.name == "email" && s.kind == SymbolKind::Column })
        );
        assert!(
            parsed
                .symbols
                .iter()
                .any(|s| { s.name == "amount" && s.kind == SymbolKind::Column })
        );

        // Custom enum type
        assert_eq!(
            kinds(&parsed, "payment_status"),
            vec![SymbolKind::SchemaEnum]
        );

        // View
        assert!(
            parsed
                .symbols
                .iter()
                .any(|s| { s.name == "active_payments" && s.kind == SymbolKind::View })
        );

        // SQL has no imports.
        assert!(parsed.imports.is_empty());
    }

    #[test]
    fn sql_schema_qualified_names_index_the_bare_object_name() {
        // `object_reference` carries qualifiers in grammar fields
        // (`database:`/`schema:`/`name:`); only the `name:` field is the
        // object's identifier. Capturing any other child would index the
        // schema (`public`) as a table and miss lookups by bare name.
        let src = "\
CREATE TABLE public.users (
    id SERIAL PRIMARY KEY
);

CREATE TYPE public.payment_status AS ENUM ('pending', 'completed');

CREATE VIEW public.active_users AS SELECT * FROM public.users;

CREATE TABLE warehouse.analytics.events (id BIGINT);
";
        let parsed = parse(Language::Sql, src);

        assert_eq!(kinds(&parsed, "users"), vec![SymbolKind::Table]);
        assert_eq!(
            kinds(&parsed, "payment_status"),
            vec![SymbolKind::SchemaEnum]
        );
        assert_eq!(kinds(&parsed, "active_users"), vec![SymbolKind::View]);
        assert_eq!(kinds(&parsed, "events"), vec![SymbolKind::Table]);

        // Neither the qualifiers nor the dotted reference may become symbols.
        assert!(
            parsed.symbols.iter().all(|s| !s.name.contains('.')
                && s.name != "public"
                && s.name != "warehouse"
                && s.name != "analytics"),
            "schema qualifiers leaked into the symbol index: {:?}",
            parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn diesel_table_macro_detected_as_table() {
        let src = "\
diesel::table! {
    users (id) {
        id -> Int4,
        email -> Varchar,
        created_at -> Timestamptz,
    }
}

pub struct User {
    id: i32,
    email: String,
}
";
        let parsed = parse(Language::Rust, src);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|s| { s.name == "users" && s.kind == SymbolKind::Table }),
            "Diesel table! not detected: {:?}",
            parsed.symbols
        );
        assert_eq!(kinds(&parsed, "User"), vec![SymbolKind::Struct]);
    }

    #[test]
    fn django_model_detected_as_table() {
        let src = "\
from django.db import models

class Payment(models.Model):
    amount = models.DecimalField()
    status = models.CharField(max_length=20)

class Helper:
    pass
";
        let parsed = parse(Language::Python, src);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|s| s.name == "payments" && s.kind == SymbolKind::Table),
            "Django model not detected: {:?}",
            parsed.symbols
        );
        assert_eq!(kinds(&parsed, "Helper"), vec![SymbolKind::Class]);
    }

    #[test]
    fn sqlalchemy_tablename_detected_as_table() {
        let src = "\
from sqlalchemy.orm import declarative_base
Base = declarative_base()

class Order(Base):
    __tablename__ = \"orders\"
    id = Column(Integer, primary_key=True)
";
        let parsed = parse(Language::Python, src);
        assert!(
            parsed.symbols.iter().any(|s| s.kind == SymbolKind::Table),
            "SQLAlchemy model not detected: {:?}",
            parsed.symbols
        );
    }

    #[test]
    fn sqlalchemy_explicit_tablename_overrides_class_name_convention() {
        // `__tablename__` deliberately does not match the naive
        // CamelCase->snake_case->pluralize conversion of the class name, so
        // this only passes if the literal value is read rather than derived.
        let src = "\
from sqlalchemy.orm import declarative_base
Base = declarative_base()

class Order(Base):
    __tablename__ = \"customer_orders\"
    id = Column(Integer, primary_key=True)
";
        let parsed = parse(Language::Python, src);
        assert!(
            parsed
                .symbols
                .iter()
                .any(|s| s.name == "customer_orders" && s.kind == SymbolKind::Table),
            "explicit __tablename__ value not used: {:?}",
            parsed.symbols
        );
        assert!(
            !parsed.symbols.iter().any(|s| s.name == "orders"),
            "table name should not fall back to class-name convention: {:?}",
            parsed.symbols
        );
    }

    #[test]
    fn python_unrelated_view_model_not_detected_as_table() {
        let src = "\
class OrderViewModel(ViewModel):
    total = 0
";
        let parsed = parse(Language::Python, src);
        assert!(
            !parsed.symbols.iter().any(|s| s.kind == SymbolKind::Table),
            "unrelated ViewModel base should not be indexed as a table: {:?}",
            parsed.symbols
        );
    }

    #[test]
    fn rust_unrelated_table_suffixed_macro_not_detected() {
        let src = "\
render_table! {
    users (id) {
        id -> Int4,
    }
}
";
        let parsed = parse(Language::Rust, src);
        assert!(
            !parsed.symbols.iter().any(|s| s.kind == SymbolKind::Table),
            "unrelated render_table! macro should not be indexed as a table: {:?}",
            parsed.symbols
        );
    }
}
