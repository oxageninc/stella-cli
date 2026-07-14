//! Extracted symbols: the function/method/type declarations the indexer
//! pulls out of a source file. Kinds are the cross-language superset the
//! spec names (`06-context-protocol.md` §2.2 "Symbol (function/type/module)";
//! task brief: "functions, methods, structs/classes/enums/traits/interfaces").

/// What kind of declaration a symbol is. Stored as its lowercase [`Self::tag`]
/// in `code_graph_symbols.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Trait,
    Interface,
    // Schema objects (SQL DDL + ORM models):
    Table,
    Column,
    SchemaEnum,
    View,
}
impl SymbolKind {
    /// Map a query kind-capture name (see [`crate::queries`]) to a kind.
    pub(crate) fn from_capture(capture: &str) -> Option<SymbolKind> {
        Some(match capture {
            "fn" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "struct" => SymbolKind::Struct,
            "class" => SymbolKind::Class,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "table" => SymbolKind::Table,
            "column" => SymbolKind::Column,
            "schema_enum" => SymbolKind::SchemaEnum,
            "view" => SymbolKind::View,
            _ => return None,
        })
    }

    /// Stable lowercase tag persisted in the store.
    pub fn tag(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Struct => "struct",
            SymbolKind::Class => "class",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Interface => "interface",
            SymbolKind::Table => "table",
            SymbolKind::Column => "column",
            SymbolKind::SchemaEnum => "schema_enum",
            SymbolKind::View => "view",
        }
    }

    /// Parse a persisted tag back into a kind (for query results).
    pub(crate) fn from_tag(tag: &str) -> SymbolKind {
        match tag {
            "method" => SymbolKind::Method,
            "struct" => SymbolKind::Struct,
            "class" => SymbolKind::Class,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "table" => SymbolKind::Table,
            "column" => SymbolKind::Column,
            "schema_enum" => SymbolKind::SchemaEnum,
            "view" => SymbolKind::View,
            // "function" and any unknown/forward-compat tag read back as a
            // plain function — the least-surprising, never-panicking default.
            _ => SymbolKind::Function,
        }
    }

    /// The human keyword used in citation labels (`09-lessons-learned.md`
    /// L-C4). Functions and methods both read as `fn`, matching the spec's
    /// worked example `fn run_turn (stella-core/src/driver.rs:160)`.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            SymbolKind::Function | SymbolKind::Method => "fn",
            SymbolKind::Struct => "struct",
            SymbolKind::Class => "class",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Interface => "interface",
            SymbolKind::Table => "table",
            SymbolKind::Column => "column",
            SymbolKind::SchemaEnum => "enum",
            SymbolKind::View => "view",
        }
    }

    /// Precedence for dedup when the same name node is captured by two
    /// patterns (a `Method` is also matched by the general function pattern);
    /// the higher rank wins. See [`crate::parse`].
    pub(crate) fn rank(self) -> u8 {
        match self {
            SymbolKind::Method => 2,
            _ => 1,
        }
    }
}

/// One extracted symbol. Lines are 1-based and inclusive, matching how
/// editors and the `path:line` citation convention count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
}
