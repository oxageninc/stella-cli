//! The storage map's canonical model (`docs/design/storage-map.md` §3) and
//! the SQL adapter: vendor-neutral entities (layer / namespace / relation /
//! field), stable addresses, name normalization, and deep DDL extraction —
//! per-column types, nullability, defaults, constraints, foreign keys,
//! `ALTER TABLE ... ADD COLUMN`, and `COMMENT ON` harvesting.
//!
//! Extraction here is **shared** by the indexer ([`crate::store`]) and the
//! pre-write gate (`stella-tools`), so the gate and the index cannot drift
//! apart. Structure only: intent/boundary meaning comes from the committed
//! manifest ([`crate::manifest`]) and is merged at snapshot time, never
//! persisted in the rebuildable store (spec §6 rebuild invariant).

use tree_sitter::Node;

use crate::parse::Grammars;

/// What a relation is. Stored as its lowercase [`Self::tag`] in
/// `code_graph_storage_objects.kind`; TEXT in the store, so future kinds
/// (collection, key-pattern, stream, …) cost no schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
    EnumType,
}

impl RelationKind {
    pub fn tag(self) -> &'static str {
        match self {
            RelationKind::Table => "table",
            RelationKind::View => "view",
            RelationKind::EnumType => "enum",
        }
    }

    pub fn from_tag(tag: &str) -> RelationKind {
        match tag {
            "view" => RelationKind::View,
            "enum" => RelationKind::EnumType,
            _ => RelationKind::Table,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RelationKind::Table => "Table",
            RelationKind::View => "View",
            RelationKind::EnumType => "Type",
        }
    }
}

/// One field (column) extracted from a relation definition. `data_type`,
/// `default_value`, and `constraints` keep the vendor-literal spelling —
/// the map describes, it does not translate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FieldDef {
    pub name: String,
    pub data_type: Option<String>,
    pub nullable: bool,
    pub default_value: Option<String>,
    pub constraints: Vec<String>,
    /// Bare (unqualified, lowercased) name of the relation a `REFERENCES`
    /// clause points at, when present.
    pub references: Option<String>,
    /// Harvested `COMMENT ON COLUMN` text, when present in the same source.
    pub comment: Option<String>,
    pub line: u32,
}

/// One relation (table / view / enum type) extracted from source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationDef {
    pub name: String,
    /// Schema qualifier as written (`public.users` → `public`); relations
    /// written unqualified land in the implicit `default` namespace.
    pub namespace: String,
    pub kind: RelationKind,
    pub fields: Vec<FieldDef>,
    /// Enum variants for [`RelationKind::EnumType`]; empty otherwise.
    pub enum_values: Vec<String>,
    /// Harvested `COMMENT ON TABLE` text, when present in the same source.
    pub comment: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
}

/// A column added to an *existing* relation via `ALTER TABLE … ADD COLUMN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldAddition {
    /// Bare (unqualified) relation name as written in the ALTER statement.
    pub relation: String,
    pub namespace: String,
    pub field: FieldDef,
}

/// Everything the storage adapter extracts from one source file.
#[derive(Debug, Clone, Default)]
pub struct StorageExtract {
    pub relations: Vec<RelationDef>,
    pub additions: Vec<FieldAddition>,
}

impl StorageExtract {
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty() && self.additions.is_empty()
    }
}

/// Standalone extraction handle for callers without a [`crate::CodeGraph`]
/// (the pre-write gate parses *proposed* content before any write lands).
/// Compiles the grammars once; reuse across calls.
pub struct StorageExtractor {
    grammars: Grammars,
}

impl StorageExtractor {
    pub fn new() -> Result<StorageExtractor, crate::GraphError> {
        Ok(StorageExtractor {
            grammars: Grammars::load()?,
        })
    }

    /// Extract relations and column additions from SQL source text.
    pub fn extract_sql(&self, source: &str) -> StorageExtract {
        extract_sql(&self.grammars, source)
    }
}

/// The implicit namespace for objects written without a schema qualifier.
pub const DEFAULT_NAMESPACE: &str = "default";

/// The implicit layer for SQL files no manifest layer claims.
pub const DEFAULT_SQL_LAYER: &str = "sql";

/// Whether a path is a storage-definition file the adapter understands.
/// The single membership test shared by the indexer and the pre-write gate.
/// Grows with each adapter (spec §4a); SQL DDL today.
pub fn is_storage_file(path: &str) -> bool {
    path.ends_with(".sql")
}

// ---- Names and addresses -------------------------------------------------

/// Normalize a display name for identity: lowercase, `camelCase` and
/// `kebab-case` folded to `snake_case`, every other character an underscore.
/// `userId`, `user-id`, and `USER_ID` all normalize to `user_id` — collisions
/// by construction (spec §3a).
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower = false;
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else if ch.is_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            prev_lower = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// The duplicate-detection key: [`normalize_name`] with a conservative
/// singular fold, so `payments`, `payment`, and `Payment` share one key.
/// `ies` → `y`; a single trailing `s` is stripped unless the name ends in
/// `ss` (`status`, `address` keep their tail consistent either way since
/// both sides of a comparison fold identically).
pub fn dedup_key(name: &str) -> String {
    normalize_name(name)
        .split('_')
        .map(fold_plural)
        .collect::<Vec<_>>()
        .join("_")
}

fn fold_plural(token: &str) -> String {
    if let Some(stem) = token.strip_suffix("ies")
        && !stem.is_empty()
    {
        return format!("{stem}y");
    }
    if token.ends_with("ss") || token.len() < 2 {
        return token.to_string();
    }
    token
        .strip_suffix('s')
        .map(|s| s.to_string())
        .unwrap_or_else(|| token.to_string())
}

/// Canonical address of a relation: `layer/namespace/relation`, every
/// segment normalized. Rendered to humans with the `store://` prefix.
pub fn relation_address(layer: &str, namespace: &str, relation: &str) -> String {
    format!(
        "{}/{}/{}",
        normalize_name(layer),
        normalize_name(namespace),
        normalize_name(relation)
    )
}

/// Canonical address of a field: `layer/namespace/relation/field`.
pub fn field_address(layer: &str, namespace: &str, relation: &str, field: &str) -> String {
    format!(
        "{}/{}",
        relation_address(layer, namespace, relation),
        normalize_name(field)
    )
}

/// Human rendering of an address (`store://…`, spec §3a).
pub fn display_address(address: &str) -> String {
    format!("store://{address}")
}

// ---- SQL extraction (tree-sitter walk + deterministic token parse) -------

/// Extract every relation definition and column addition from SQL source.
/// Tree-sitter finds the statement structure (proven node kinds:
/// `create_table` / `create_view` / `create_type` / `object_reference` /
/// `column_definition`); a deterministic token scan decodes each column's
/// type/constraint tail. `COMMENT ON` statements and `ALTER TABLE … ADD
/// COLUMN` are decoded by a statement-level text scan, which tolerates
/// dialects the grammar does not fully parse.
pub(crate) fn extract_sql(grammars: &Grammars, source: &str) -> StorageExtract {
    let mut out = StorageExtract::default();

    if let Some(root) = crate::parse::parse_sql_tree(grammars, source) {
        let src = source.as_bytes();
        walk_sql(root.root_node(), src, &mut out.relations);
    }

    out.additions = extract_alter_additions(source);
    apply_comments(source, &mut out.relations);
    out
}

fn walk_sql(node: Node, src: &[u8], out: &mut Vec<RelationDef>) {
    match node.kind() {
        "create_table" => {
            if let Some(mut rel) = relation_header(node, src, RelationKind::Table) {
                collect_columns(node, src, &mut rel);
                collect_table_constraints(node, src, &mut rel);
                out.push(rel);
            }
        }
        "create_view" => {
            if let Some(rel) = relation_header(node, src, RelationKind::View) {
                out.push(rel);
            }
        }
        "create_type" => {
            if let Some(mut rel) = relation_header(node, src, RelationKind::EnumType) {
                rel.enum_values = enum_values(node, src);
                out.push(rel);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_sql(child, src, out);
    }
}

/// Name + namespace from the statement's `object_reference` (grammar fields
/// `schema:` / `name:` — the same shape `queries.rs` relies on).
fn relation_header(node: Node, src: &[u8], kind: RelationKind) -> Option<RelationDef> {
    let reference = find_child_of_kind(node, "object_reference")?;
    let name = reference
        .child_by_field_name("name")?
        .utf8_text(src)
        .ok()?
        .trim_matches('"')
        .to_string();
    if name.is_empty() {
        return None;
    }
    let namespace = reference
        .child_by_field_name("schema")
        .and_then(|n| n.utf8_text(src).ok())
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
    Some(RelationDef {
        name,
        namespace,
        kind,
        fields: Vec::new(),
        enum_values: Vec::new(),
        comment: None,
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    })
}

fn find_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

fn collect_columns(table: Node, src: &[u8], rel: &mut RelationDef) {
    fn walk(node: Node, src: &[u8], rel: &mut RelationDef) {
        if node.kind() == "column_definition" {
            if let Some(field) = decode_column(node, src) {
                rel.fields.push(field);
            }
            return; // a column definition never nests another
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, src, rel);
        }
    }
    walk(table, src, rel);
}

/// One `column_definition` node → a [`FieldDef`]: the grammar pins the name;
/// the type/constraint tail is decoded from the node's own text by
/// [`parse_column_tail`].
fn decode_column(node: Node, src: &[u8]) -> Option<FieldDef> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.trim_matches('"').to_string();
    if name.is_empty() {
        return None;
    }
    let full = node.utf8_text(src).ok()?;
    // The tail starts after the name's span within the node text.
    let name_end = name_node.end_byte().saturating_sub(node.start_byte());
    let tail = full.get(name_end..).unwrap_or("");
    let mut field = parse_column_tail(tail);
    field.name = name;
    field.line = node.start_position().row as u32 + 1;
    Some(field)
}

/// Keywords that end the data-type token run of a column definition.
const COLUMN_KEYWORDS: &[&str] = &[
    "NOT",
    "NULL",
    "PRIMARY",
    "UNIQUE",
    "DEFAULT",
    "REFERENCES",
    "CHECK",
    "CONSTRAINT",
    "COLLATE",
    "GENERATED",
    "AUTOINCREMENT",
    "AUTO_INCREMENT",
];

/// Decode the text after a column name: data type, nullability, default,
/// constraints, FK target. Deterministic token scan; parenthesized groups
/// (`NUMERIC(10,2)`, `CHECK (amount >= 0)`) stay attached as single tokens.
fn parse_column_tail(tail: &str) -> FieldDef {
    let tokens = tokenize_sql(tail);
    let mut field = FieldDef {
        nullable: true,
        ..FieldDef::default()
    };

    let mut type_tokens: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let upper = tokens[i].to_ascii_uppercase();
        if COLUMN_KEYWORDS.contains(&upper.as_str()) {
            break;
        }
        type_tokens.push(&tokens[i]);
        i += 1;
    }
    if !type_tokens.is_empty() {
        field.data_type = Some(type_tokens.join(" "));
    }

    while i < tokens.len() {
        let upper = tokens[i].to_ascii_uppercase();
        match upper.as_str() {
            "NOT" if next_is(&tokens, i, "NULL") => {
                field.nullable = false;
                field.constraints.push("NOT NULL".into());
                i += 2;
            }
            "PRIMARY" if next_is(&tokens, i, "KEY") => {
                field.nullable = false;
                field.constraints.push("PRIMARY KEY".into());
                i += 2;
            }
            "UNIQUE" => {
                field.constraints.push("UNIQUE".into());
                i += 1;
            }
            "DEFAULT" => {
                let mut value_tokens: Vec<&str> = Vec::new();
                i += 1;
                while i < tokens.len() {
                    let up = tokens[i].to_ascii_uppercase();
                    if COLUMN_KEYWORDS.contains(&up.as_str()) {
                        break;
                    }
                    value_tokens.push(&tokens[i]);
                    i += 1;
                }
                if !value_tokens.is_empty() {
                    field.default_value = Some(value_tokens.join(" "));
                }
            }
            "REFERENCES" => {
                if let Some(target) = tokens.get(i + 1) {
                    let bare = target
                        .split('(')
                        .next()
                        .unwrap_or(target)
                        .rsplit('.')
                        .next()
                        .unwrap_or(target)
                        .trim_matches('"')
                        .to_lowercase();
                    if !bare.is_empty() {
                        field.constraints.push(format!("REFERENCES {target}"));
                        field.references = Some(bare);
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "CHECK" => {
                if let Some(body) = tokens.get(i + 1).filter(|t| t.starts_with('(')) {
                    field.constraints.push(format!("CHECK {body}"));
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    field
}

fn next_is(tokens: &[String], i: usize, word: &str) -> bool {
    tokens
        .get(i + 1)
        .is_some_and(|t| t.eq_ignore_ascii_case(word))
}

/// Split SQL text into tokens: whitespace/comma separated at paren depth 0;
/// a `(` opens a group that stays attached to the token being built (so
/// `NUMERIC(10,2)` and `CHECK (x > 0)`'s body each come out whole).
fn tokenize_sql(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            c if depth > 0 => current.push(c),
            c if c.is_whitespace() || c == ',' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    // A group may open after whitespace (`CHECK (…)`): splice a bare
    // `(...)` token onto nothing — it stands alone, which the callers handle.
    tokens
}

/// Table-level `PRIMARY KEY (…)` / `FOREIGN KEY (…) REFERENCES t (…)` /
/// `UNIQUE (…)` clauses, decoded from the statement text (grammar-shape
/// independent) and folded back onto the named columns.
fn collect_table_constraints(table: Node, src: &[u8], rel: &mut RelationDef) {
    let Ok(text) = table.utf8_text(src) else {
        return;
    };
    for clause in split_top_level_commas(text) {
        let clause = clause.trim();
        let upper = clause.to_ascii_uppercase();
        let after_constraint = if upper.starts_with("CONSTRAINT") {
            // `CONSTRAINT name PRIMARY KEY (…)` — skip the two lead tokens.
            clause
                .splitn(3, char::is_whitespace)
                .nth(2)
                .unwrap_or(clause)
        } else {
            clause
        };
        let upper = after_constraint.to_ascii_uppercase();
        if let Some(cols) = upper
            .strip_prefix("PRIMARY KEY")
            .and_then(|_| first_paren_group(after_constraint))
        {
            for col in cols {
                mark_column(rel, &col, "PRIMARY KEY", false);
            }
        } else if let Some(cols) = upper
            .strip_prefix("UNIQUE")
            .and_then(|_| first_paren_group(after_constraint))
        {
            for col in cols {
                mark_column(rel, &col, "UNIQUE", true);
            }
        } else if upper.starts_with("FOREIGN KEY") {
            let cols = first_paren_group(after_constraint).unwrap_or_default();
            let target = upper
                .find("REFERENCES")
                .map(|at| after_constraint[at + "REFERENCES".len()..].trim())
                .and_then(|rest| rest.split(|c: char| c.is_whitespace() || c == '(').next())
                .map(|t| {
                    t.rsplit('.')
                        .next()
                        .unwrap_or(t)
                        .trim_matches('"')
                        .to_lowercase()
                })
                .filter(|t| !t.is_empty());
            for col in cols {
                if let Some(field) = find_field_mut(rel, &col)
                    && let Some(target) = &target
                {
                    field.references = Some(target.clone());
                    field.constraints.push(format!("REFERENCES {target}"));
                }
            }
        }
    }
}

/// The comma-separated clauses inside the outermost paren group of a
/// statement (the CREATE TABLE body).
fn split_top_level_commas(text: &str) -> Vec<String> {
    let Some(open) = text.find('(') else {
        return Vec::new();
    };
    let body = &text[open + 1..];
    let mut clauses = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                if depth == 0 {
                    break; // end of the CREATE TABLE body
                }
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => clauses.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        clauses.push(current);
    }
    clauses
}

/// The identifiers inside the first `(…)` group of a clause.
fn first_paren_group(clause: &str) -> Option<Vec<String>> {
    let open = clause.find('(')?;
    let close = clause[open..].find(')')? + open;
    Some(
        clause[open + 1..close]
            .split(',')
            .map(|c| c.trim().trim_matches('"').to_string())
            .filter(|c| !c.is_empty())
            .collect(),
    )
}

fn find_field_mut<'a>(rel: &'a mut RelationDef, name: &str) -> Option<&'a mut FieldDef> {
    let key = normalize_name(name);
    rel.fields
        .iter_mut()
        .find(|f| normalize_name(&f.name) == key)
}

fn mark_column(rel: &mut RelationDef, name: &str, constraint: &str, keep_nullable: bool) {
    if let Some(field) = find_field_mut(rel, name) {
        if !field.constraints.iter().any(|c| c == constraint) {
            field.constraints.push(constraint.to_string());
        }
        if !keep_nullable {
            field.nullable = false;
        }
    }
}

/// Enum variant literals of a `CREATE TYPE … AS ENUM ('a', 'b')` statement.
fn enum_values(node: Node, src: &[u8]) -> Vec<String> {
    let Ok(text) = node.utf8_text(src) else {
        return Vec::new();
    };
    let upper = text.to_ascii_uppercase();
    let Some(at) = upper.find("ENUM") else {
        return Vec::new();
    };
    let rest = &text[at + 4..];
    let mut values = Vec::new();
    let mut current: Option<String> = None;
    for ch in rest.chars() {
        match (ch, &mut current) {
            ('\'', None) => current = Some(String::new()),
            ('\'', Some(value)) => {
                values.push(std::mem::take(value));
                current = None;
            }
            (c, Some(value)) => value.push(c),
            (')', None) if !values.is_empty() => break,
            _ => {}
        }
    }
    values
}

// ---- ALTER TABLE … ADD COLUMN (statement-level text scan) ----------------

/// Decode `ALTER TABLE t ADD [COLUMN] name type …` statements. Text-level
/// (split on `;`), so it works even where the grammar's ALTER coverage is
/// thin — the same conservative posture as the original gate's scan.
fn extract_alter_additions(source: &str) -> Vec<FieldAddition> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for statement in source.split(';') {
        let line = source[..offset].lines().count().max(1) as u32;
        offset += statement.len() + 1;
        let trimmed = statement.trim();
        let upper = trimmed.to_ascii_uppercase();
        let Some(rest_at) = upper
            .strip_prefix("ALTER TABLE")
            .map(|_| "ALTER TABLE".len())
        else {
            continue;
        };
        let rest = trimmed[rest_at..].trim_start();
        let mut tokens = rest.split_whitespace();
        let Some(mut table) = tokens.next() else {
            continue;
        };
        // Optional `IF EXISTS` / `ONLY` prefixes before the table name.
        while table.eq_ignore_ascii_case("if")
            || table.eq_ignore_ascii_case("exists")
            || table.eq_ignore_ascii_case("only")
        {
            match tokens.next() {
                Some(next) => table = next,
                None => break,
            }
        }
        let (namespace, bare) = match table.rsplit_once('.') {
            Some((ns, name)) => (
                ns.rsplit('.').next().unwrap_or(ns).trim_matches('"'),
                name.trim_matches('"'),
            ),
            None => (DEFAULT_NAMESPACE, table.trim_matches('"')),
        };
        if bare.is_empty() {
            continue;
        }
        // Everything after the table name; each `ADD [COLUMN] …` clause
        // (comma-separated in Postgres) contributes one addition.
        let after_table = rest
            .find(table)
            .map(|at| &rest[at + table.len()..])
            .unwrap_or("");
        for clause in after_table.split(',') {
            let clause = clause.trim();
            let upper = clause.to_ascii_uppercase();
            let Some(mut tail) = upper
                .strip_prefix("ADD")
                .map(|_| clause["ADD".len()..].trim_start())
            else {
                continue;
            };
            if tail.to_ascii_uppercase().starts_with("COLUMN") {
                tail = tail["COLUMN".len()..].trim_start();
            }
            let up = tail.to_ascii_uppercase();
            if up.starts_with("CONSTRAINT")
                || up.starts_with("PRIMARY")
                || up.starts_with("FOREIGN")
            {
                continue; // table-level additions, not columns
            }
            let mut parts = tail.splitn(2, char::is_whitespace);
            let Some(name) = parts.next().filter(|n| !n.is_empty()) else {
                continue;
            };
            let mut skip_leading = name;
            if skip_leading.eq_ignore_ascii_case("if") {
                // `ADD COLUMN IF NOT EXISTS name …`
                let rest = parts.next().unwrap_or("");
                let mut t = rest.split_whitespace();
                let (not, exists) = (t.next().unwrap_or(""), t.next().unwrap_or(""));
                if !not.eq_ignore_ascii_case("not") || !exists.eq_ignore_ascii_case("exists") {
                    continue;
                }
                let Some(real) = t.next() else { continue };
                skip_leading = real;
                let tail_at = rest.find(real).map(|at| at + real.len()).unwrap_or(0);
                let mut field = parse_column_tail(&rest[tail_at..]);
                field.name = skip_leading.trim_matches('"').to_string();
                field.line = line;
                out.push(FieldAddition {
                    relation: bare.to_string(),
                    namespace: namespace.to_string(),
                    field,
                });
                continue;
            }
            let mut field = parse_column_tail(parts.next().unwrap_or(""));
            field.name = skip_leading.trim_matches('"').to_string();
            field.line = line;
            out.push(FieldAddition {
                relation: bare.to_string(),
                namespace: namespace.to_string(),
                field,
            });
        }
    }
    out
}

// ---- COMMENT ON harvesting -----------------------------------------------

/// Fold `COMMENT ON TABLE t IS '…'` / `COMMENT ON COLUMN t.c IS '…'`
/// statements onto the extracted relations (spec §4b "harvested" meaning).
fn apply_comments(source: &str, relations: &mut [RelationDef]) {
    for statement in source.split(';') {
        let trimmed = statement.trim();
        let upper = trimmed.to_ascii_uppercase();
        let (is_column, rest) = if let Some(at) = upper.strip_prefix("COMMENT ON TABLE") {
            let _ = at;
            (false, trimmed["COMMENT ON TABLE".len()..].trim_start())
        } else if upper.starts_with("COMMENT ON COLUMN") {
            (true, trimmed["COMMENT ON COLUMN".len()..].trim_start())
        } else {
            continue;
        };
        let Some(is_at) = rest.to_ascii_uppercase().find(" IS ") else {
            continue;
        };
        let target = rest[..is_at].trim().trim_matches('"');
        let Some(text) = single_quoted(&rest[is_at + 4..]) else {
            continue;
        };
        if is_column {
            let mut parts: Vec<&str> = target.split('.').collect();
            let Some(column) = parts.pop() else { continue };
            let Some(table) = parts.pop() else { continue };
            let table_key = normalize_name(table);
            for rel in relations.iter_mut() {
                if normalize_name(&rel.name) == table_key
                    && let Some(field) = find_field_mut(rel, column)
                {
                    field.comment = Some(text.clone());
                }
            }
        } else {
            let table = target.rsplit('.').next().unwrap_or(target);
            let table_key = normalize_name(table);
            for rel in relations.iter_mut() {
                if normalize_name(&rel.name) == table_key {
                    rel.comment = Some(text.clone());
                }
            }
        }
    }
}

fn single_quoted(text: &str) -> Option<String> {
    let open = text.find('\'')?;
    let rest = &text[open + 1..];
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.peek() == Some(&'\'') {
                out.push('\'');
                chars.next();
            } else {
                return Some(out);
            }
        } else {
            out.push(ch);
        }
    }
    None
}

// ---- Snapshot (the read-side shape the gate and CLI consume) -------------

/// One layer in a [`StorageSnapshot`] — manifest-declared or the implicit
/// SQL fallback.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LayerEntry {
    pub key: String,
    pub engine: String,
    pub class: String,
    pub durability: String,
    pub boundary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedirectEntry {
    /// Glob over proposed (normalized) names: `refund*`.
    pub pattern: String,
    /// Canonical address that owns the concept.
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FieldEntry {
    pub name: String,
    pub data_type: Option<String>,
    pub nullable: bool,
    pub default_value: Option<String>,
    pub constraints: Vec<String>,
    pub references: Option<String>,
    /// Meaning: manifest-declared wins over harvested comment (spec §4b).
    pub intent: Option<String>,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelationEntry {
    /// `layer/namespace/relation`, normalized. Render with [`display_address`].
    pub address: String,
    pub layer: String,
    pub namespace: String,
    /// Display name as written in source.
    pub name: String,
    pub kind: String,
    pub fields: Vec<FieldEntry>,
    pub enum_values: Vec<String>,
    pub intent: Option<String>,
    pub boundary: Option<String>,
    pub redirects: Vec<RedirectEntry>,
    /// `path:line` provenance; `None` for manifest stubs.
    pub source: Option<String>,
}

/// The assembled storage map: parsed structure from the index merged with
/// manifest meaning. This is what the pre-write gate and the CLI consume.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StorageSnapshot {
    pub layers: Vec<LayerEntry>,
    pub relations: Vec<RelationEntry>,
    /// Manifest meaning entries whose parsed entity no longer exists —
    /// flagged for the drift report, never deleted (spec §5b).
    #[serde(default)]
    pub orphaned_meanings: Vec<String>,
}

impl StorageSnapshot {
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }

    pub fn relation_at(&self, address: &str) -> Option<&RelationEntry> {
        self.relations.iter().find(|r| r.address == address)
    }
}

// ---- Embed cards ---------------------------------------------------------

/// The deterministic textual rendering of a relation for embedding and for
/// `stella storage show` (spec §7a). Includes the parent chain's meaning so
/// the card is findable by purpose, not only by name. Byte-stable for
/// unchanged inputs — its content hash keys the embedding row.
pub fn embed_card(layer: Option<&LayerEntry>, relation: &RelationEntry) -> String {
    let mut card = String::new();
    card.push_str(&format!(
        "storage {} {}\n",
        relation.kind,
        display_address(&relation.address)
    ));
    if let Some(layer) = layer {
        card.push_str(&format!(
            "layer {}: {}, {}, {}\n",
            layer.key, layer.engine, layer.class, layer.durability
        ));
        if let Some(boundary) = &layer.boundary {
            card.push_str(&format!("layer boundary: {boundary}\n"));
        }
    }
    if let Some(intent) = &relation.intent {
        card.push_str(&format!("purpose: {intent}\n"));
    }
    if let Some(boundary) = &relation.boundary {
        card.push_str(&format!("boundary: {boundary}\n"));
    }
    for redirect in &relation.redirects {
        card.push_str(&format!(
            "redirect: {} -> {}\n",
            redirect.pattern,
            display_address(&redirect.target)
        ));
    }
    if !relation.enum_values.is_empty() {
        card.push_str(&format!("values: {}\n", relation.enum_values.join(" | ")));
    }
    for field in &relation.fields {
        let mut line = format!("field {}", field.name);
        if let Some(ty) = &field.data_type {
            line.push_str(&format!(" {ty}"));
        }
        if !field.nullable {
            line.push_str(" NOT NULL");
        }
        if let Some(default) = &field.default_value {
            line.push_str(&format!(" DEFAULT {default}"));
        }
        if let Some(target) = &field.references {
            line.push_str(&format!(" -> {target}"));
        }
        if let Some(intent) = &field.intent {
            line.push_str(&format!(" — {intent}"));
        }
        card.push_str(&line);
        card.push('\n');
    }
    if let Some(source) = &relation.source {
        card.push_str(&format!("defined in {source}\n"));
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::Grammars;

    fn extract(src: &str) -> StorageExtract {
        let grammars = Grammars::load().expect("grammars compile");
        extract_sql(&grammars, src)
    }

    #[test]
    fn normalize_folds_case_and_separators() {
        assert_eq!(normalize_name("userId"), "user_id");
        assert_eq!(normalize_name("UserID"), "user_id");
        assert_eq!(normalize_name("user-id"), "user_id");
        assert_eq!(normalize_name("USER_ID"), "user_id");
        assert_eq!(normalize_name("PaymentRecords"), "payment_records");
    }

    #[test]
    fn dedup_key_folds_plurals() {
        assert_eq!(dedup_key("payments"), dedup_key("payment"));
        assert_eq!(dedup_key("PaymentRecords"), dedup_key("payment_record"));
        assert_eq!(dedup_key("companies"), dedup_key("company"));
        // `ss` tails are preserved, not shredded.
        assert_eq!(dedup_key("address"), "address");
        // Distinct concepts stay distinct.
        assert_ne!(dedup_key("payments"), dedup_key("refunds"));
    }

    #[test]
    fn addresses_are_normalized_paths() {
        assert_eq!(
            relation_address("primary-pg", "Public", "Payments"),
            "primary_pg/public/payments"
        );
        assert_eq!(
            field_address("sql", "default", "payments", "userId"),
            "sql/default/payments/user_id"
        );
        assert_eq!(display_address("a/b/c"), "store://a/b/c");
    }

    #[test]
    fn deep_extraction_captures_types_constraints_defaults_fks() {
        let out = extract(
            "CREATE TABLE payments (\n\
                 id SERIAL PRIMARY KEY,\n\
                 amount NUMERIC(10,2) NOT NULL,\n\
                 currency TEXT DEFAULT 'USD',\n\
                 user_id INTEGER REFERENCES users(id),\n\
                 note TEXT CHECK (length(note) < 500)\n\
             );",
        );
        assert_eq!(out.relations.len(), 1);
        let rel = &out.relations[0];
        assert_eq!(rel.name, "payments");
        assert_eq!(rel.namespace, DEFAULT_NAMESPACE);
        assert_eq!(rel.kind, RelationKind::Table);
        assert_eq!(rel.fields.len(), 5);

        let id = &rel.fields[0];
        assert_eq!(id.data_type.as_deref(), Some("SERIAL"));
        assert!(!id.nullable);
        assert!(id.constraints.contains(&"PRIMARY KEY".to_string()));

        let amount = &rel.fields[1];
        assert_eq!(amount.data_type.as_deref(), Some("NUMERIC(10,2)"));
        assert!(!amount.nullable);

        let currency = &rel.fields[2];
        assert!(currency.nullable);
        assert_eq!(currency.default_value.as_deref(), Some("'USD'"));

        let user_id = &rel.fields[3];
        assert_eq!(user_id.references.as_deref(), Some("users"));

        let note = &rel.fields[4];
        assert!(
            note.constraints.iter().any(|c| c.starts_with("CHECK")),
            "CHECK constraint lost: {:?}",
            note.constraints
        );
    }

    #[test]
    fn table_level_constraints_fold_onto_columns() {
        let out = extract(
            "CREATE TABLE line_items (\n\
                 order_id INTEGER,\n\
                 sku TEXT,\n\
                 qty INTEGER,\n\
                 PRIMARY KEY (order_id, sku),\n\
                 FOREIGN KEY (order_id) REFERENCES orders(id)\n\
             );",
        );
        let rel = &out.relations[0];
        let order_id = rel.fields.iter().find(|f| f.name == "order_id").unwrap();
        assert!(!order_id.nullable);
        assert!(order_id.constraints.contains(&"PRIMARY KEY".to_string()));
        assert_eq!(order_id.references.as_deref(), Some("orders"));
        let sku = rel.fields.iter().find(|f| f.name == "sku").unwrap();
        assert!(sku.constraints.contains(&"PRIMARY KEY".to_string()));
    }

    #[test]
    fn qualified_names_set_the_namespace() {
        let out = extract("CREATE TABLE billing.payments (id INT);");
        let rel = &out.relations[0];
        assert_eq!(rel.namespace, "billing");
        assert_eq!(rel.name, "payments");
    }

    #[test]
    fn enum_types_capture_their_values() {
        let out = extract("CREATE TYPE payment_status AS ENUM ('pending', 'completed', 'failed');");
        let rel = &out.relations[0];
        assert_eq!(rel.kind, RelationKind::EnumType);
        assert_eq!(rel.enum_values, vec!["pending", "completed", "failed"]);
    }

    #[test]
    fn alter_table_add_column_is_extracted() {
        let out = extract(
            "ALTER TABLE payments ADD COLUMN refunded_at TIMESTAMP;\n\
             ALTER TABLE billing.invoices ADD paid BOOLEAN NOT NULL DEFAULT false;",
        );
        assert_eq!(out.additions.len(), 2);
        let first = &out.additions[0];
        assert_eq!(first.relation, "payments");
        assert_eq!(first.field.name, "refunded_at");
        assert_eq!(first.field.data_type.as_deref(), Some("TIMESTAMP"));
        let second = &out.additions[1];
        assert_eq!(second.relation, "invoices");
        assert_eq!(second.namespace, "billing");
        assert_eq!(second.field.name, "paid");
        assert!(!second.field.nullable);
        assert_eq!(second.field.default_value.as_deref(), Some("false"));
    }

    #[test]
    fn alter_add_column_if_not_exists() {
        let out = extract("ALTER TABLE t ADD COLUMN IF NOT EXISTS extra TEXT;");
        assert_eq!(out.additions.len(), 1);
        assert_eq!(out.additions[0].field.name, "extra");
        assert_eq!(out.additions[0].field.data_type.as_deref(), Some("TEXT"));
    }

    #[test]
    fn comment_on_harvests_table_and_column_meaning() {
        let out = extract(
            "CREATE TABLE payments (amount NUMERIC NOT NULL);\n\
             COMMENT ON TABLE payments IS 'One row per charge attempt.';\n\
             COMMENT ON COLUMN payments.amount IS 'Gross amount charged.';",
        );
        let rel = &out.relations[0];
        assert_eq!(rel.comment.as_deref(), Some("One row per charge attempt."));
        assert_eq!(
            rel.fields[0].comment.as_deref(),
            Some("Gross amount charged.")
        );
    }

    #[test]
    fn embed_card_is_deterministic_and_carries_meaning() {
        let layer = LayerEntry {
            key: "primary_pg".into(),
            engine: "postgres".into(),
            class: "relational".into(),
            durability: "durable-truth".into(),
            boundary: Some("All transactional state.".into()),
        };
        let relation = RelationEntry {
            address: "primary_pg/billing/payments".into(),
            layer: "primary_pg".into(),
            namespace: "billing".into(),
            name: "payments".into(),
            kind: "table".into(),
            fields: vec![FieldEntry {
                name: "amount".into(),
                data_type: Some("NUMERIC(10,2)".into()),
                nullable: false,
                default_value: None,
                constraints: vec!["NOT NULL".into()],
                references: None,
                intent: Some("Gross amount charged.".into()),
                line: 2,
            }],
            enum_values: vec![],
            intent: Some("One row per charge attempt.".into()),
            boundary: Some("Refund state lives in refunds.".into()),
            redirects: vec![RedirectEntry {
                pattern: "refund*".into(),
                target: "primary_pg/billing/refunds".into(),
            }],
            source: Some("migrations/001.sql:1".into()),
        };
        let card = embed_card(Some(&layer), &relation);
        assert!(card.contains("store://primary_pg/billing/payments"));
        assert!(card.contains("purpose: One row per charge attempt."));
        assert!(card.contains("field amount NUMERIC(10,2) NOT NULL — Gross amount charged."));
        assert!(card.contains("redirect: refund* -> store://primary_pg/billing/refunds"));
        // Deterministic: same input, same bytes.
        assert_eq!(card, embed_card(Some(&layer), &relation));
    }
}
