//! Tree-sitter S-expression queries, one pair (symbols + imports) per
//! language, as `const &str` **compile-time data** — never loaded from a file
//! at runtime ( L-L2: built-in assets that resolve
//! relative to the binary's install path broke the moment the artifact was
//! bundled differently; embedding them as module data is the fix).
//!
//! Naming convention shared by every symbol query so
//! [`crate::parse`] can decode matches uniformly:
//! - `@name` captures the identifier whose text becomes the symbol name.
//! - the *kind capture* (`@fn` / `@method` / `@struct` / `@enum` / `@trait` /
//!   `@class` / `@interface`) captures the whole definition node — its line
//!   span becomes the symbol span, and its capture name encodes the kind.
//!
//! Name fields are matched with the `(_)` wildcard rather than a concrete
//! node type (`identifier` vs `type_identifier` vs `property_identifier`
//! differ across and within these grammars); this makes the queries robust to
//! per-grammar naming quirks while still pinning the *statement* node type.
//!
//! Methods are intentionally double-captured (once by the general function
//! pattern, once by the enclosing-type pattern); [`crate::parse`] dedups by
//! the name node's byte range and lets the more specific `Method` kind win.

// ---- Rust ----------------------------------------------------------------

/// Functions (incl. bodyless trait-method signatures, which are
/// `function_signature_item` not `function_item`), methods (impl bodies),
/// structs, enums, traits.
pub const RUST_SYMBOLS: &str = r#"
(function_item name: (_) @name) @fn
(function_signature_item name: (_) @name) @fn
(struct_item name: (_) @name) @struct
(enum_item name: (_) @name) @enum
(trait_item name: (_) @name) @trait
(impl_item body: (declaration_list (function_item name: (_) @name) @method))
"#;

/// `use` declarations. Rust module→file resolution (the `mod` tree, `lib.rs`
/// vs `mod.rs`, re-exports) is genuinely non-trivial and out of scope for
/// this cut, so the raw `use` path is recorded as an unresolved edge; see
/// `crate::import::resolve`.
pub const RUST_IMPORTS: &str = r#"
(use_declaration argument: (_) @use)
"#;

// ---- Python --------------------------------------------------------------

/// Functions, methods (class bodies), classes.
pub const PYTHON_SYMBOLS: &str = r#"
(function_definition name: (_) @name) @fn
(class_definition name: (_) @name) @class
(class_definition body: (block (function_definition name: (_) @name) @method))
"#;

/// Both import statement forms; the relative-import decode (dots → package
/// directory) that the spec calls out ( Phase 3 item 3, "fix the
/// known thin Python import-edge resolution") happens structurally in
/// [`crate::parse`] against the captured statement nodes.
pub const PYTHON_IMPORTS: &str = r#"
(import_statement) @import
(import_from_statement) @from_import
"#;

// ---- JavaScript ----------------------------------------------------------

/// Functions (incl. generators + arrow/function-expression consts), classes,
/// methods. JavaScript has no interfaces/enums, so the query is a strict
/// subset of the TypeScript one and must be kept separate — compiling the TS
/// query against the JS grammar would reference nonexistent node types.
pub const JS_SYMBOLS: &str = r#"
(function_declaration name: (_) @name) @fn
(generator_function_declaration name: (_) @name) @fn
(class_declaration name: (_) @name) @class
(method_definition name: (_) @name) @method
(variable_declarator name: (_) @name value: (arrow_function)) @fn
(variable_declarator name: (_) @name value: (function_expression)) @fn
"#;

/// `import`/`export … from`, dynamic `import(...)`, and `require(...)`. The
/// `require` pattern captures any identifier-callee call with a string
/// argument; [`crate::parse`] keeps only those whose callee text is
/// `require` (avoids query-predicate machinery).
pub const JS_IMPORTS: &str = r#"
(import_statement source: (string (string_fragment) @source))
(export_statement source: (string (string_fragment) @source))
(call_expression function: (import) arguments: (arguments (string (string_fragment) @source)))
(call_expression function: (identifier) @callee arguments: (arguments (string (string_fragment) @source)))
"#;

// ---- TypeScript (also used for TSX) --------------------------------------

/// Adds interfaces, enums, and abstract classes on top of the JS symbol set.
/// Shared verbatim by the `typescript` and `tsx` grammars (TSX is a superset
/// carrying the same declaration node types).
pub const TS_SYMBOLS: &str = r#"
(function_declaration name: (_) @name) @fn
(generator_function_declaration name: (_) @name) @fn
(class_declaration name: (_) @name) @class
(abstract_class_declaration name: (_) @name) @class
(interface_declaration name: (_) @name) @interface
(enum_declaration name: (_) @name) @enum
(method_definition name: (_) @name) @method
(variable_declarator name: (_) @name value: (arrow_function)) @fn
(variable_declarator name: (_) @name value: (function_expression)) @fn
"#;

/// Same import surface as JavaScript (the specifier node shapes are
/// identical); shared by `typescript` and `tsx`.
pub const TS_IMPORTS: &str = JS_IMPORTS;

// ---- SQL -----------------------------------------------------------------

/// SQL DDL: tables (with their columns), views, and custom enum/composite
/// types. SQL has no import concept, so there is no `SQL_IMPORTS`.
///
/// `tree-sitter-sequel` node types:
/// - `create_table` wraps `object_reference` (table name) and
///   `column_definitions` (a list of `column_definition` children).
/// - `column_definition` has a `name` field.
/// - `create_type` is used for `CREATE TYPE foo AS ENUM (...)` and composite
///   types.
/// - `create_view` wraps a `column_definitions` optional + `select`.
/// - `object_reference` qualifies names via grammar fields
///   (`database:`/`schema:`/`name:`) — the object's own identifier is always
///   the `name:` field, so schema qualifiers must not become symbol names
///   (`CREATE TABLE public.users` indexes `users`, never `public` or
///   `public.users`).
pub const SQL_SYMBOLS: &str = r#"
(create_table
  (object_reference name: (_) @name)) @table
(column_definition name: (_) @name) @column
(create_type
  (object_reference name: (_) @name)) @schema_enum
(create_view
  (object_reference name: (_) @name)) @view
"#;

/// SQL has no imports — this empty string keeps the `LangPack` two-query
/// shape uniform without introducing a conditional in `parse_file`.
pub const SQL_IMPORTS: &str = "";

// ---- Go ------------------------------------------------------------------

/// Functions, methods (receiver functions), and named types. Go's grammar is
/// regular enough that these three patterns cover the declaration surface.
pub const GO_SYMBOLS: &str = r#"
(function_declaration name: (_) @name) @fn
(method_declaration name: (_) @name) @method
(type_declaration (type_spec name: (_) @name type: (struct_type))) @struct
(type_declaration (type_spec name: (_) @name type: (interface_type))) @interface
"#;

/// Both the grouped `import ( … )` block and the single-line form; the
/// quoted path inside each spec is the edge target.
pub const GO_IMPORTS: &str = r#"
(import_declaration) @import
"#;

// ---- Java ----------------------------------------------------------------

/// Classes, interfaces, enums, records, and methods. Java carries no free
/// functions, so every callable is a method on a type.
pub const JAVA_SYMBOLS: &str = r#"
(class_declaration name: (_) @name) @class
(interface_declaration name: (_) @name) @interface
(enum_declaration name: (_) @name) @enum
(record_declaration name: (_) @name) @class
(method_declaration name: (_) @name) @method
"#;

/// `import a.b.C;` — a package path, which resolves to a file only under the
/// conventional package-as-directory layout.
pub const JAVA_IMPORTS: &str = r#"
(import_declaration) @import
"#;

// ---- C -------------------------------------------------------------------

/// Function definitions, plus the struct/union/enum and typedef surface.
/// The declarator nesting is why functions need their own pattern rather
/// than a bare name capture: the identifier sits under `function_declarator`.
pub const C_SYMBOLS: &str = r#"
(function_definition
  declarator: (function_declarator declarator: (identifier) @name)) @fn
(struct_specifier name: (type_identifier) @name) @struct
(union_specifier name: (type_identifier) @name) @struct
(enum_specifier name: (type_identifier) @name) @enum
(type_definition declarator: (type_identifier) @name) @struct
"#;

/// `#include` in both forms. Only the quoted form reliably names a file in
/// the tree; angle-bracket includes point at system headers and are captured
/// but will not resolve to a workspace path.
pub const C_IMPORTS: &str = r#"
(preproc_include) @import
"#;

// ---- PHP -----------------------------------------------------------------

/// Classes, interfaces, traits, enums, free functions, and methods.
pub const PHP_SYMBOLS: &str = r#"
(class_declaration name: (_) @name) @class
(interface_declaration name: (_) @name) @interface
(trait_declaration name: (_) @name) @trait
(enum_declaration name: (_) @name) @enum
(function_definition name: (_) @name) @fn
(method_declaration name: (_) @name) @method
"#;

/// Both mechanisms, which resolve differently: `require`/`include` name a
/// literal path and resolve like C's quoted include, while `use` names a
/// NAMESPACE — mapping that to a file needs composer.json's PSR-4 autoload
/// map, so those edges are captured but stay unresolved until that lands.
pub const PHP_IMPORTS: &str = r#"
(namespace_use_declaration) @import
(expression_statement (include_expression)) @include
(expression_statement (require_expression)) @require
"#;
