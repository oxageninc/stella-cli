//! The storage map's canonical model (`docs/design/storage-map.md` §3) and
//! its source adapters: vendor-neutral entities (layer / namespace /
//! relation / field), stable addresses, name normalization, and per-adapter
//! structural extraction (spec §4a):
//!
//! - [`sql`] — deep DDL: types, nullability, defaults, constraints, FKs,
//!   `ALTER TABLE … ADD COLUMN`, `COMMENT ON` harvesting.
//! - [`prisma`] — `.prisma` schemas: models, enums, `@map`/`@@map`,
//!   `@relation` FKs, `///` doc harvesting, Mongo-provider detection.
//! - [`ts`] — TypeScript/JavaScript: Drizzle `pgTable`-family calls,
//!   TypeORM `@Entity`/`@Column` decorators, Mongoose schemas (document
//!   paths included), DynamoDB CDK/SDK table definitions.
//! - [`py`] — Python: Django `models.Model` classes, SQLAlchemy declarative
//!   and core `Table(...)` definitions.
//!
//! Extraction here is **shared** by the indexer ([`crate::store`]) and the
//! pre-write gate (`stella-tools`), so the gate and the index cannot drift
//! apart. Structure only: intent/boundary meaning comes from the committed
//! manifest ([`crate::manifest`]) and is merged at snapshot time, never
//! persisted in the rebuildable store (spec §6 rebuild invariant).
//!
//! An unrecognized pattern yields a false negative repaired by a manifest
//! stub, never garbage and never a false block (spec §12) — every adapter
//! extracts only what it can prove from the source shape.

mod prisma;
mod py;
mod sql;
mod ts;

pub(crate) use sql::extract_sql;

use crate::lang::Language;
use crate::parse::Grammars;

/// What a relation is. Stored as its lowercase [`Self::tag`] in
/// `code_graph_storage_objects.kind`; TEXT in the store, so future kinds
/// (collection, key-pattern, stream, …) cost no schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
    EnumType,
    /// A document-store collection (Mongoose, Prisma-on-Mongo).
    Collection,
}

impl RelationKind {
    pub fn tag(self) -> &'static str {
        match self {
            RelationKind::Table => "table",
            RelationKind::View => "view",
            RelationKind::EnumType => "enum",
            RelationKind::Collection => "collection",
        }
    }

    pub fn from_tag(tag: &str) -> RelationKind {
        match tag {
            "view" => RelationKind::View,
            "enum" => RelationKind::EnumType,
            "collection" => RelationKind::Collection,
            _ => RelationKind::Table,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RelationKind::Table => "Table",
            RelationKind::View => "View",
            RelationKind::EnumType => "Type",
            RelationKind::Collection => "Collection",
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

/// One relation (table / view / enum type / collection) extracted from
/// source.
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
    /// Harvested `COMMENT ON TABLE` / `///` doc-comment text, when present
    /// in the same source.
    pub comment: Option<String>,
    /// The implicit layer this relation belongs to when no manifest `paths`
    /// glob claims its file: `None` for relational schema-as-code (it lands
    /// in the shared [`DEFAULT_SQL_LAYER`], so an ORM model and the SQL
    /// migration it generates describe ONE address), `Some("mongo")` /
    /// `Some("dynamodb")` for technologies that are a different storage
    /// layer by construction. A manifest claim always wins (spec §4a).
    pub layer_hint: Option<String>,
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

    /// Extract storage entities from any adapter-recognized file, dispatched
    /// by `path`'s extension. Empty for unrecognized paths and for
    /// recognized files with no storage definitions.
    pub fn extract(&self, path: &str, source: &str) -> StorageExtract {
        extract_for_path(&self.grammars, path, source)
    }
}

/// The implicit namespace for objects written without a schema qualifier.
pub const DEFAULT_NAMESPACE: &str = "default";

/// The implicit layer for relational definitions (SQL DDL *and* relational
/// schema-as-code) no manifest layer claims. Shared deliberately: a Prisma
/// model and the migration Prisma generates from it must resolve to one
/// address, not a false cross-layer duplicate.
pub const DEFAULT_SQL_LAYER: &str = "sql";

/// The implicit layer for MongoDB definitions (Mongoose, Prisma-on-Mongo)
/// no manifest layer claims. Distinct from [`DEFAULT_SQL_LAYER`] by
/// construction, so a Mongo collection duplicating a relational table
/// trips the cross-layer conflict (spec §1 turn-160).
pub const DEFAULT_MONGO_LAYER: &str = "mongo";

/// The implicit layer for DynamoDB table definitions.
pub const DEFAULT_DYNAMO_LAYER: &str = "dynamodb";

/// Which adapter pass a path dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterFamily {
    Sql,
    Prisma,
    TypeScript,
    Python,
}

fn adapter_family(path: &str) -> Option<AdapterFamily> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    Some(match ext {
        "sql" => AdapterFamily::Sql,
        "prisma" => AdapterFamily::Prisma,
        "ts" | "mts" | "cts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => AdapterFamily::TypeScript,
        "py" | "pyi" => AdapterFamily::Python,
        _ => return None,
    })
}

/// Whether a path is a storage-definition *candidate* an adapter
/// understands. The single membership test shared by the indexer and the
/// pre-write gate (spec §4a). TS/JS/Python files are candidates whose
/// extraction is marker-gated inside their adapter — a file with no schema
/// markers costs one substring scan and yields an empty extract.
pub fn is_storage_file(path: &str) -> bool {
    adapter_family(path).is_some()
}

/// Whether a path should be indexed for storage even though no
/// [`Language`] grammar claims it (`.prisma` — its own DSL, parsed by the
/// [`prisma`] adapter). Used by the walker and watcher membership tests.
pub fn indexes_without_language(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "prisma")
}

/// Extract every storage entity one file defines, dispatched by extension.
/// Shared verbatim by the indexer and the gate so they cannot drift.
pub(crate) fn extract_for_path(grammars: &Grammars, path: &str, source: &str) -> StorageExtract {
    match adapter_family(path) {
        Some(AdapterFamily::Sql) => extract_sql(grammars, source),
        Some(AdapterFamily::Prisma) => prisma::extract(source),
        Some(AdapterFamily::TypeScript) => {
            let lang =
                Language::from_path(std::path::Path::new(path)).unwrap_or(Language::TypeScript);
            ts::extract(grammars, lang, source)
        }
        Some(AdapterFamily::Python) => py::extract(grammars, path, source),
        None => StorageExtract::default(),
    }
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
