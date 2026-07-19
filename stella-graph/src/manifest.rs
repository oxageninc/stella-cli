//! `stella.storage.toml` — the storage map's durable half (spec §5).
//!
//! Structure (types, columns, constraints) is parsed from source and lives in
//! the rebuildable index; this file holds what parsers cannot know: layers,
//! boundaries, intent sentences, redirects, and structural stubs for storage
//! no parser sees (Redis key patterns, blob prefixes). Committed to git, so
//! meaning survives a cache rebuild, a fresh clone, and shows up in review.
//!
//! The gate's declared-intent path appends entries **textually**
//! ([`append_meaning`]) instead of re-serializing, so human comments and
//! formatting in the file are never destroyed by an agent write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::storage::{
    DEFAULT_NAMESPACE, DEFAULT_SQL_LAYER, LayerEntry, RedirectEntry, RelationEntry,
    StorageSnapshot, normalize_name, relation_address,
};

/// File name at the workspace root.
pub const MANIFEST_FILE: &str = "stella.storage.toml";

pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST_FILE)
}

/// One declared storage layer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LayerDecl {
    pub engine: Option<String>,
    pub class: Option<String>,
    pub durability: Option<String>,
    pub boundary: Option<String>,
    pub intent: Option<String>,
    /// Path globs (`migrations/**`) claiming files for this layer; files no
    /// layer claims land in the implicit `sql` layer.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Structural stubs: relations no parser can see (spec §4a source 3).
    #[serde(default)]
    pub relations: Vec<StubDecl>,
}

/// A manifest-declared relation stub.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StubDecl {
    pub name: String,
    #[serde(default = "default_stub_kind")]
    pub kind: String,
    pub namespace: Option<String>,
    pub intent: Option<String>,
    pub boundary: Option<String>,
}

fn default_stub_kind() -> String {
    "relation".to_string()
}

/// Meaning attached to an addressed entity (`[relations."…"]` / `[fields."…"]`
/// / `[namespaces.layer.ns]` tables).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MeaningDecl {
    pub intent: Option<String>,
    pub boundary: Option<String>,
    #[serde(default)]
    pub redirects: Vec<RedirectDecl>,
    /// `declared` (default for human edits) | `harvested` | `inferred`.
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedirectDecl {
    pub pattern: String,
    pub target: String,
}

/// The parsed manifest. Every lookup normalizes address segments, so
/// `primary-pg/Billing/Payments` and `primary_pg/billing/payments` name the
/// same entity.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StorageManifest {
    #[allow(dead_code)]
    pub version: Option<u32>,
    #[serde(default)]
    pub layers: BTreeMap<String, LayerDecl>,
    #[serde(default)]
    pub namespaces: BTreeMap<String, BTreeMap<String, MeaningDecl>>,
    #[serde(default)]
    pub relations: BTreeMap<String, MeaningDecl>,
    #[serde(default)]
    pub fields: BTreeMap<String, MeaningDecl>,
}

impl StorageManifest {
    /// Load the manifest at `root`, if present. A malformed file is an
    /// `Err` (callers degrade to structure-only, never abort an index pass);
    /// a missing file is `Ok(None)` — the normal state for most projects.
    pub fn load(root: &Path) -> Result<Option<StorageManifest>, String> {
        let path = manifest_path(root);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The layer whose `paths` globs claim `rel_path` (first match in key
    /// order), or the implicit SQL layer.
    pub fn layer_for(&self, rel_path: &str) -> String {
        for (key, layer) in &self.layers {
            if layer.paths.iter().any(|p| glob_match(p, rel_path)) {
                return key.clone();
            }
        }
        DEFAULT_SQL_LAYER.to_string()
    }

    fn meaning_for<'a>(
        table: &'a BTreeMap<String, MeaningDecl>,
        address: &str,
    ) -> Option<&'a MeaningDecl> {
        table
            .iter()
            .find(|(key, _)| normalize_address(key) == address)
            .map(|(_, decl)| decl)
    }

    pub fn relation_meaning(&self, address: &str) -> Option<&MeaningDecl> {
        Self::meaning_for(&self.relations, address)
    }

    pub fn field_meaning(&self, address: &str) -> Option<&MeaningDecl> {
        Self::meaning_for(&self.fields, address)
    }

    pub fn namespace_meaning(&self, layer: &str, namespace: &str) -> Option<&MeaningDecl> {
        let layer_key = normalize_name(layer);
        let ns_key = normalize_name(namespace);
        self.namespaces
            .iter()
            .find(|(key, _)| normalize_name(key) == layer_key)
            .and_then(|(_, table)| {
                table
                    .iter()
                    .find(|(ns, _)| normalize_name(ns) == ns_key)
                    .map(|(_, decl)| decl)
            })
    }

    /// Declared layers as snapshot entries (engine/class default to
    /// `unknown` until a human fills them in).
    pub fn layer_entries(&self) -> Vec<LayerEntry> {
        self.layers
            .iter()
            .map(|(key, decl)| LayerEntry {
                key: normalize_name(key),
                engine: decl.engine.clone().unwrap_or_else(|| "unknown".into()),
                class: decl.class.clone().unwrap_or_else(|| "unknown".into()),
                durability: decl.durability.clone().unwrap_or_else(|| "unknown".into()),
                boundary: decl.boundary.clone(),
            })
            .collect()
    }

    /// Manifest stubs as snapshot relations (`source: None`).
    pub fn stub_relations(&self) -> Vec<RelationEntry> {
        let mut out = Vec::new();
        for (layer_key, layer) in &self.layers {
            for stub in &layer.relations {
                let namespace = stub.namespace.as_deref().unwrap_or(DEFAULT_NAMESPACE);
                out.push(RelationEntry {
                    address: relation_address(layer_key, namespace, &stub.name),
                    layer: normalize_name(layer_key),
                    namespace: normalize_name(namespace),
                    name: stub.name.clone(),
                    kind: stub.kind.clone(),
                    fields: Vec::new(),
                    enum_values: Vec::new(),
                    intent: stub.intent.clone(),
                    boundary: stub.boundary.clone(),
                    redirects: Vec::new(),
                    source: None,
                });
            }
        }
        out
    }
}

/// Normalize a manifest address key segment-wise (`primary-pg/Billing/X` →
/// `primary_pg/billing/x`), tolerating a `store://` prefix.
pub fn normalize_address(key: &str) -> String {
    key.trim_start_matches("store://")
        .split('/')
        .map(normalize_name)
        .collect::<Vec<_>>()
        .join("/")
}

/// Merge parsed structure with manifest meaning into the final snapshot
/// (spec §4 precedence: declared manifest meaning wins over harvested
/// comments; structure is untouched). Also surfaces orphaned meanings —
/// manifest entries whose parsed entity no longer exists (spec §5b: meaning
/// is never deleted, only flagged).
pub fn merge_snapshot(
    mut relations: Vec<RelationEntry>,
    manifest: Option<&StorageManifest>,
) -> StorageSnapshot {
    let mut layers: Vec<LayerEntry> = Vec::new();
    let mut orphaned: Vec<String> = Vec::new();

    if let Some(manifest) = manifest {
        layers = manifest.layer_entries();
        for rel in &mut relations {
            if let Some(meaning) = manifest.relation_meaning(&rel.address) {
                if meaning.intent.is_some() {
                    rel.intent = meaning.intent.clone();
                }
                if meaning.boundary.is_some() {
                    rel.boundary = meaning.boundary.clone();
                }
                rel.redirects = meaning
                    .redirects
                    .iter()
                    .map(|r| RedirectEntry {
                        pattern: r.pattern.clone(),
                        target: normalize_address(&r.target),
                    })
                    .collect();
            }
            for field in &mut rel.fields {
                let address = format!("{}/{}", rel.address, normalize_name(&field.name));
                if let Some(meaning) = manifest.field_meaning(&address)
                    && meaning.intent.is_some()
                {
                    field.intent = meaning.intent.clone();
                }
            }
        }
        let stub_addresses: Vec<String> = relations.iter().map(|r| r.address.clone()).collect();
        for stub in manifest.stub_relations() {
            if !stub_addresses.contains(&stub.address) {
                relations.push(stub);
            }
        }
        for key in manifest.relations.keys().chain(manifest.fields.keys()) {
            let address = normalize_address(key);
            let known = relations.iter().any(|r| {
                r.address == address
                    || r.fields
                        .iter()
                        .any(|f| format!("{}/{}", r.address, normalize_name(&f.name)) == address)
            });
            if !known {
                orphaned.push(address);
            }
        }
    }

    // Every layer referenced by a relation exists in the layer list, so the
    // implicit `sql` fallback renders alongside declared layers.
    for rel in &relations {
        if !layers.iter().any(|l| l.key == rel.layer) {
            layers.push(LayerEntry {
                key: rel.layer.clone(),
                engine: "sql".into(),
                class: "relational".into(),
                durability: "durable-truth".into(),
                boundary: None,
            });
        }
    }

    StorageSnapshot {
        layers,
        relations,
        orphaned_meanings: orphaned,
    }
}

/// Append a meaning entry to the manifest **textually** — never a
/// re-serialize, so human comments/formatting survive (spec §5b durability).
/// Creates the file with a header when absent. `section` is `relations` or
/// `fields`; `origin` is recorded so inferred/declared text is auditable.
pub fn append_meaning(
    root: &Path,
    section: &str,
    address: &str,
    intent: &str,
    origin: &str,
) -> std::io::Result<()> {
    let path = manifest_path(root);
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.is_empty() {
        text.push_str(
            "# stella.storage.toml — the storage map's durable half.\n\
             # Structure is parsed from source; this file holds layers,\n\
             # boundaries, intent, and redirects. See docs/design/storage-map.md.\n\
             version = 1\n",
        );
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let address = normalize_address(address);
    text.push_str(&format!(
        "\n[{section}.\"{address}\"]\nintent = \"{}\"\norigin = \"{}\"\n",
        toml_escape(intent.trim()),
        toml_escape(origin),
    ));
    std::fs::write(&path, text)
}

fn toml_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', "")
}

/// Minimal glob: `**` spans any number of path segments, `*` spans within a
/// segment. Enough for `migrations/**`, `db/*.sql`, `prisma/schema.prisma`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn segments(s: &str) -> Vec<&str> {
        s.split('/').filter(|s| !s.is_empty()).collect()
    }
    fn seg_match(pat: &str, seg: &str) -> bool {
        // Single-segment `*` wildcard match, case-sensitive, non-recursive.
        let parts: Vec<&str> = pat.split('*').collect();
        if parts.len() == 1 {
            return pat == seg;
        }
        let mut rest = seg;
        if !parts[0].is_empty() {
            match rest.strip_prefix(parts[0]) {
                Some(r) => rest = r,
                None => return false,
            }
        }
        let last = parts[parts.len() - 1];
        if !last.is_empty() {
            match rest.strip_suffix(last) {
                Some(r) => rest = r,
                None => return false,
            }
        }
        for part in &parts[1..parts.len() - 1] {
            if part.is_empty() {
                continue;
            }
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
        true
    }
    fn matches(pat: &[&str], path: &[&str]) -> bool {
        match (pat.first(), path.first()) {
            (None, None) => true,
            (Some(&"**"), _) => {
                matches(&pat[1..], path) || (!path.is_empty() && matches(pat, &path[1..]))
            }
            (Some(p), Some(s)) if seg_match(p, s) => matches(&pat[1..], &path[1..]),
            _ => false,
        }
    }
    matches(&segments(pattern), &segments(path))
}

/// Single-segment glob for redirect patterns (`refund*` against a
/// normalized name). The literal runs between stars are normalized the same
/// way names are, so `Refund-*` still matches `refund_status`.
pub fn name_glob_match(pattern: &str, name: &str) -> bool {
    let normalized = pattern
        .split('*')
        .map(normalize_name)
        .collect::<Vec<_>>()
        .join("*");
    glob_match(&normalized, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SAMPLE: &str = r#"
version = 1

[layers.primary-pg]
engine = "postgres"
class = "relational"
durability = "durable-truth"
boundary = "All transactional application state."
paths = ["migrations/**"]

[layers.session-redis]
engine = "redis"
class = "key-value"
durability = "ephemeral"

[[layers.session-redis.relations]]
name = "sess:*"
kind = "key-pattern"
intent = "One key per active browser session."

[namespaces.primary-pg.billing]
intent = "Money movement."
boundary = "No user-profile data."

[relations."primary-pg/billing/payments"]
intent = "One row per charge attempt."
boundary = "Refund state lives in refunds, not here."
redirects = [{ pattern = "refund*", target = "store://primary-pg/billing/refunds" }]

[fields."primary-pg/billing/payments/amount"]
intent = "Gross amount charged."
"#;

    fn manifest() -> StorageManifest {
        toml::from_str(SAMPLE).expect("sample parses")
    }

    #[test]
    fn parses_layers_stubs_and_meanings() {
        let m = manifest();
        assert_eq!(m.layers.len(), 2);
        assert_eq!(m.layers["primary-pg"].engine.as_deref(), Some("postgres"));
        assert_eq!(m.layers["session-redis"].relations.len(), 1);
        assert!(
            m.relation_meaning("primary_pg/billing/payments")
                .and_then(|d| d.intent.as_deref())
                .is_some()
        );
        assert!(
            m.field_meaning("primary_pg/billing/payments/amount")
                .is_some()
        );
        assert!(m.namespace_meaning("primary-pg", "billing").is_some());
    }

    #[test]
    fn layer_matching_uses_paths_globs() {
        let m = manifest();
        assert_eq!(m.layer_for("migrations/001_init.sql"), "primary-pg");
        assert_eq!(m.layer_for("src/schema.sql"), DEFAULT_SQL_LAYER);
    }

    #[test]
    fn glob_semantics() {
        assert!(glob_match("migrations/**", "migrations/2024/001.sql"));
        assert!(glob_match("db/*.sql", "db/schema.sql"));
        assert!(!glob_match("db/*.sql", "db/sub/schema.sql"));
        assert!(glob_match("**/schema.prisma", "prisma/schema.prisma"));
        assert!(name_glob_match("refund*", "refund_status"));
        assert!(!name_glob_match("refund*", "payment"));
    }

    #[test]
    fn merge_applies_meaning_and_flags_orphans() {
        let m = manifest();
        let parsed = vec![RelationEntry {
            address: "primary_pg/billing/payments".into(),
            layer: "primary_pg".into(),
            namespace: "billing".into(),
            name: "payments".into(),
            kind: "table".into(),
            fields: vec![crate::storage::FieldEntry {
                name: "amount".into(),
                data_type: Some("NUMERIC".into()),
                nullable: false,
                default_value: None,
                constraints: vec![],
                references: None,
                intent: None,
                line: 2,
            }],
            enum_values: vec![],
            intent: None,
            boundary: None,
            redirects: vec![],
            source: Some("migrations/001.sql:1".into()),
        }];
        let snap = merge_snapshot(parsed, Some(&m));
        let rel = snap.relation_at("primary_pg/billing/payments").unwrap();
        assert_eq!(rel.intent.as_deref(), Some("One row per charge attempt."));
        assert_eq!(rel.redirects.len(), 1);
        assert_eq!(rel.redirects[0].target, "primary_pg/billing/refunds");
        assert_eq!(
            rel.fields[0].intent.as_deref(),
            Some("Gross amount charged.")
        );
        // The redis stub is present as a relation with no source.
        assert!(
            snap.relations
                .iter()
                .any(|r| r.layer == "session_redis" && r.source.is_none())
        );
        // Nothing orphaned: every meaning key has a live target.
        assert!(
            snap.orphaned_meanings.is_empty(),
            "{:?}",
            snap.orphaned_meanings
        );
    }

    #[test]
    fn orphaned_meaning_is_flagged_not_dropped() {
        let m = manifest();
        let snap = merge_snapshot(Vec::new(), Some(&m));
        assert!(
            snap.orphaned_meanings
                .contains(&"primary_pg/billing/payments".to_string())
        );
    }

    #[test]
    fn append_meaning_is_textual_and_preserves_content() {
        let dir = tempdir().unwrap();
        std::fs::write(
            manifest_path(dir.path()),
            "# hand-written comment\nversion = 1\n",
        )
        .unwrap();
        append_meaning(
            dir.path(),
            "relations",
            "sql/default/payment_records",
            "Ledger of imported legacy charges — distinct from payments.",
            "declared",
        )
        .unwrap();
        let text = std::fs::read_to_string(manifest_path(dir.path())).unwrap();
        assert!(text.starts_with("# hand-written comment"), "comment lost");
        let parsed: StorageManifest = toml::from_str(&text).expect("still valid TOML");
        assert!(
            parsed
                .relation_meaning("sql/default/payment_records")
                .is_some()
        );
    }

    #[test]
    fn append_meaning_creates_the_file_with_header() {
        let dir = tempdir().unwrap();
        append_meaning(
            dir.path(),
            "relations",
            "sql/default/orders",
            "Orders.",
            "declared",
        )
        .unwrap();
        let text = std::fs::read_to_string(manifest_path(dir.path())).unwrap();
        assert!(text.contains("version = 1"));
        let parsed: StorageManifest = toml::from_str(&text).expect("valid TOML");
        assert!(parsed.relation_meaning("sql/default/orders").is_some());
    }
}
