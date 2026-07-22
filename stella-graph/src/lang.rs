//! The set of languages the code graph indexes, and the mapping from a file
//! extension to its tree-sitter grammar + query pair.
//!
//! Grammars are **native** (not WASM)
//! (stella-graph: "tree-sitter parsers (native, not WASM)").

use std::path::Path;

use crate::queries;

/// A language the indexer understands. `Tsx` is split from `TypeScript`
/// because the two use different tree-sitter grammars (`LANGUAGE_TYPESCRIPT`
/// vs `LANGUAGE_TSX`) even though they share the same query strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Sql,
    Go,
    Java,
    C,
    Php,
}

impl Language {
    /// Classify a path by extension, or `None` if it is not an indexable
    /// source file. This is the single gate the directory walk
    /// ([`crate::walk`]) uses to decide what to open.
    pub fn from_path(path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "java" => Language::Java,
            // Headers index as C. A C++ project's `.h` still yields useful
            // struct/function symbols under the C grammar, and misreading a
            // header beats skipping every declaration in the tree.
            "c" | "h" => Language::C,
            "php" => Language::Php,
            "sql" => Language::Sql,
            _ => return None,
        })
    }

    /// Stable lowercase tag stored in `code_graph_files.language` and used in
    /// error messages. Never rename without a migration.
    pub fn tag(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Sql => "sql",
            Language::Go => "go",
            Language::Java => "java",
            Language::C => "c",
            Language::Php => "php",
        }
    }

    /// The native tree-sitter grammar for this language.
    pub(crate) fn ts_language(self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::Sql => tree_sitter_sequel::LANGUAGE.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Java => tree_sitter_java::LANGUAGE.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
            // The HTML-embedding grammar, not `LANGUAGE_PHP_ONLY`: real
            // `.php` files routinely open and close `<?php` around markup,
            // and the PHP-only grammar cannot parse those at all.
            Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        }
    }

    /// The compile-time symbol query source for this language.
    pub(crate) fn symbol_query(self) -> &'static str {
        match self {
            Language::Rust => queries::RUST_SYMBOLS,
            Language::Python => queries::PYTHON_SYMBOLS,
            Language::JavaScript => queries::JS_SYMBOLS,
            Language::TypeScript | Language::Tsx => queries::TS_SYMBOLS,
            Language::Sql => queries::SQL_SYMBOLS,
            Language::Go => queries::GO_SYMBOLS,
            Language::Java => queries::JAVA_SYMBOLS,
            Language::C => queries::C_SYMBOLS,
            Language::Php => queries::PHP_SYMBOLS,
        }
    }

    /// The compile-time import query source for this language.
    pub(crate) fn import_query(self) -> &'static str {
        match self {
            Language::Rust => queries::RUST_IMPORTS,
            Language::Python => queries::PYTHON_IMPORTS,
            Language::JavaScript => queries::JS_IMPORTS,
            Language::TypeScript | Language::Tsx => queries::TS_IMPORTS,
            Language::Sql => queries::SQL_IMPORTS,
            Language::Go => queries::GO_IMPORTS,
            Language::Java => queries::JAVA_IMPORTS,
            Language::C => queries::C_IMPORTS,
            Language::Php => queries::PHP_IMPORTS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn extensions_map_to_languages() {
        assert_eq!(
            Language::from_path(Path::new("a/b.rs")),
            Some(Language::Rust)
        );
        assert_eq!(
            Language::from_path(Path::new("m.py")),
            Some(Language::Python)
        );
        assert_eq!(
            Language::from_path(Path::new("m.pyi")),
            Some(Language::Python)
        );
        assert_eq!(
            Language::from_path(Path::new("x.mjs")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            Language::from_path(Path::new("x.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(Language::from_path(Path::new("x.tsx")), Some(Language::Tsx));
        assert_eq!(
            Language::from_path(Path::new("migrations/001.sql")),
            Some(Language::Sql)
        );
        assert_eq!(Language::from_path(Path::new("README.md")), None);
        assert_eq!(Language::from_path(Path::new("noext")), None);
    }
}
