//! The pre-write storage gate — makes duplicate or misplaced schema hard to
//! write (`docs/design/storage-map.md` §8).
//!
//! When `write_file` / `edit_file` targets a storage-definition file, the
//! proposed content is parsed by the SAME adapter extraction the indexer
//! uses (`stella_graph::storage` — SQL DDL, Prisma, Drizzle, TypeORM,
//! Mongoose, DynamoDB, Django, SQLAlchemy), so the gate and the index
//! cannot drift apart. Three rings:
//!
//! - **Ring 1 (deterministic → block):** normalized-name duplicate relations
//!   (same namespace, and cross-layer), duplicate fields on an existing
//!   relation, orphaned foreign keys (SQL only — ORM reference targets are
//!   naming heuristics, and a heuristic may warn but never block).
//! - **Ring 2 (boundary redirects → block with a pointer):** a manifest
//!   boundary that names where the concept lives redirects the write there.
//! - **Ring 3 (similarity → fail once, pass with declared intent):** a
//!   proposed relation lexically close to an existing one fails once with
//!   the evidence; retrying with `storage_intent` (one sentence of purpose
//!   plus why the existing objects don't fit) passes, and the sentence is
//!   recorded in `stella.storage.toml`.
//!
//! **Cross-family materialization (spec §4a):** when SQL DDL and an ORM
//! model describe the same relation they are one entity described twice —
//! DDL wins field by field in the index, and the ORM definition is kept.
//! So ring 1's *same-layer* conflicts only fire within one source family:
//! writing `CREATE TABLE payments` for an indexed Prisma model `payments`
//! (or adopting a model over an existing table) passes clean, while a
//! second DDL definition, a second model, a near-name (ring 3), or the same
//! name in a *non-relational* layer (the Mongo-`refunds` case) still trips.
//!
//! Deterministic core: rings 1–2 are name/lookup logic; ring 3 is a token
//! comparison that can only *withhold once*, never permanently block.

use std::collections::HashSet;

use stella_graph::storage::{
    DEFAULT_DYNAMO_LAYER, DEFAULT_MONGO_LAYER, DEFAULT_SQL_LAYER, FieldAddition, FieldEntry,
    RelationDef, RelationEntry, StorageExtract, dedup_key, display_address, normalize_name,
    relation_address,
};
use stella_graph::{StorageExtractor, StorageSnapshot};

/// Ring-3 similarity threshold on name-token Jaccard overlap. `payments` vs
/// `payment_records` = {payment} ∩ {payment, record} → 0.5.
const SIMILARITY_THRESHOLD: f64 = 0.5;

/// The process-wide extractor (compiled tree-sitter grammars), built on
/// first gated write. `None` if the grammars fail to arm — the gate then
/// degrades to a no-op, matching the empty-index posture.
pub fn extractor() -> Option<&'static StorageExtractor> {
    static EXTRACTOR: std::sync::OnceLock<Option<StorageExtractor>> = std::sync::OnceLock::new();
    EXTRACTOR
        .get_or_init(|| StorageExtractor::new().ok())
        .as_ref()
}

/// Whether a path is a storage-definition candidate worth gating. Delegates
/// to the shared adapter membership test; extraction inside the adapters is
/// marker-gated, so a candidate with no schema costs one substring scan.
pub fn is_schema_file(path: &str) -> bool {
    stella_graph::storage::is_storage_file(path)
}

/// The manifest layer whose `paths` globs claim a workspace-relative path,
/// if any. `None` falls back to each extracted relation's own layer hint
/// (relational → `sql`, Mongoose → `mongo`, DynamoDB → `dynamodb`).
pub fn manifest_layer_for(root: &std::path::Path, rel_path: &str) -> Option<String> {
    stella_graph::manifest::StorageManifest::load(root)
        .ok()
        .flatten()
        .and_then(|m| m.layer_claim(rel_path))
}

/// Everything the gate needs to judge one proposed write.
pub struct GateRequest<'a> {
    /// Workspace-relative path of the target file. Determines the source
    /// family (`.sql` = DDL, everything else = schema-as-code) and stamps
    /// provenance on created objects.
    pub path: &'a str,
    /// Layer the manifest's `paths` globs claim for the target file, if any.
    /// Overrides every relation's own layer hint.
    pub manifest_layer: Option<&'a str>,
    /// Extraction of the *proposed* content (write_file `content` /
    /// edit_file's simulated post-edit file).
    pub proposed: &'a StorageExtract,
    /// Extraction of the target file's *current* content — objects the file
    /// already defines are exempt (rewriting a migration in place is not a
    /// duplicate), and field-level diffs against them catch column
    /// additions made by editing a model in place.
    pub own: &'a StorageExtract,
    /// The assembled storage map (persisted index + manifest + session
    /// overlay).
    pub snapshot: &'a StorageSnapshot,
    /// The model's declared intent, when the call carries `storage_intent`.
    pub storage_intent: Option<&'a str>,
}

impl GateRequest<'_> {
    fn is_ddl(&self) -> bool {
        self.path.ends_with(".sql")
    }

    /// The effective layer of one proposed relation: manifest claim, else
    /// the adapter's hint, else the implicit relational layer.
    fn relation_layer(&self, rel: &RelationDef) -> String {
        normalize_name(
            self.manifest_layer
                .or(rel.layer_hint.as_deref())
                .unwrap_or(DEFAULT_SQL_LAYER),
        )
    }

    fn addition_layer(&self) -> String {
        normalize_name(self.manifest_layer.unwrap_or(DEFAULT_SQL_LAYER))
    }
}

/// A passed gate: the objects this write creates (recorded into the session
/// overlay after the write lands) and, when an intent was declared, the
/// relation addresses it should be recorded against.
#[derive(Debug)]
pub struct GatePass {
    pub created: Vec<RelationEntry>,
    pub intent_addresses: Vec<String>,
}

/// Whether an existing entry came from SQL DDL, from schema-as-code, or is
/// source-less (manifest stub / session overlay without provenance —
/// conservatively conflicts with both families).
fn existing_is_ddl(existing: &RelationEntry) -> Option<bool> {
    existing.source.as_deref().map(|s| {
        let path = s.rsplit_once(':').map(|(p, _)| p).unwrap_or(s);
        path.ends_with(".sql")
    })
}

fn same_family(request_is_ddl: bool, existing: &RelationEntry) -> bool {
    existing_is_ddl(existing).is_none_or(|ddl| ddl == request_is_ddl)
}

/// Whether a layer key names relational storage. Implicit keys are known;
/// declared layers answer by their manifest `class` (an unset class is
/// treated as relational — prefer a missed exemption report to a false
/// block).
fn is_relational_layer(snapshot: &StorageSnapshot, key: &str) -> bool {
    if key == DEFAULT_SQL_LAYER {
        return true;
    }
    if key == DEFAULT_MONGO_LAYER || key == DEFAULT_DYNAMO_LAYER {
        return false;
    }
    snapshot
        .layers
        .iter()
        .find(|l| l.key == key)
        .map(|l| matches!(l.class.as_str(), "relational" | "unknown"))
        .unwrap_or(true)
}

/// Judge a proposed write. `Err(message)` is the tool error the model sees
/// (ring 1/2 block, or ring 3 challenge); `Ok` carries what to record.
pub fn check(request: &GateRequest<'_>) -> Result<GatePass, String> {
    let is_ddl = request.is_ddl();
    let own_relations: HashSet<(String, String)> = request
        .own
        .relations
        .iter()
        .map(|r| (normalize_name(&r.namespace), dedup_key(&r.name)))
        .collect();
    let own_additions: HashSet<(String, String)> = request
        .own
        .additions
        .iter()
        .map(|a| (dedup_key(&a.relation), dedup_key(&a.field.name)))
        .collect();

    // New relations this write introduces, each with its effective layer.
    let fresh_relations: Vec<(&RelationDef, String)> = request
        .proposed
        .relations
        .iter()
        .filter(|r| !own_relations.contains(&(normalize_name(&r.namespace), dedup_key(&r.name))))
        .map(|r| (r, request.relation_layer(r)))
        .collect();

    // New columns: explicit `ALTER TABLE … ADD COLUMN` statements, plus
    // fields that appear on a relation this file already defines (editing a
    // model in place adds columns without any ALTER).
    let mut fresh_additions: Vec<(FieldAddition, String)> = request
        .proposed
        .additions
        .iter()
        .filter(|a| !own_additions.contains(&(dedup_key(&a.relation), dedup_key(&a.field.name))))
        .cloned()
        .map(|a| (a, request.addition_layer()))
        .collect();
    for rel in &request.proposed.relations {
        let key = (normalize_name(&rel.namespace), dedup_key(&rel.name));
        let Some(own_rel) = request.own.relations.iter().find(|o| {
            (normalize_name(&o.namespace), dedup_key(&o.name)) == key && o.kind == rel.kind
        }) else {
            continue;
        };
        for field in &rel.fields {
            let field_key = dedup_key(&field.name);
            if !own_rel
                .fields
                .iter()
                .any(|f| dedup_key(&f.name) == field_key)
            {
                fresh_additions.push((
                    FieldAddition {
                        relation: rel.name.clone(),
                        namespace: rel.namespace.clone(),
                        field: field.clone(),
                    },
                    request.relation_layer(rel),
                ));
            }
        }
    }

    if fresh_relations.is_empty() && fresh_additions.is_empty() {
        return Ok(GatePass {
            created: Vec::new(),
            intent_addresses: Vec::new(),
        });
    }

    let mut blocks: Vec<String> = Vec::new();

    // ---- Ring 1: deterministic conflicts ---------------------------------
    // Kinds that hold data. A table, a collection, and a view holding the
    // same concept conflict with each other; an enum sharing a table's name
    // is a type definition, not a second home for the data.
    let container = |kind: &str| matches!(kind, "table" | "collection" | "view");
    for (rel, layer) in &fresh_relations {
        let ns = normalize_name(&rel.namespace);
        let key = dedup_key(&rel.name);
        for existing in &request.snapshot.relations {
            let comparable = existing.kind == rel.kind.tag()
                || (container(&existing.kind) && container(rel.kind.tag()));
            if dedup_key(&existing.name) != key || !comparable {
                continue;
            }
            if existing.layer == *layer && existing.namespace == ns {
                // Same address, other source family: an ORM model and the
                // DDL that materializes it (either direction) are the same
                // entity described twice — spec-sanctioned, not a duplicate.
                if !same_family(is_ddl, existing) {
                    continue;
                }
                blocks.push(format!(
                    "  CONFLICT: {} `{}` already exists as {}\n{}",
                    rel.kind.label(),
                    rel.name,
                    display_address(&existing.address),
                    existing_summary(existing),
                ));
            } else if existing.layer != *layer {
                // The same concept in two storage layers is drift — except
                // when both layers are relational and the families differ
                // (schema-as-code in the implicit layer vs. its own DDL in
                // a manifest-claimed layer: materialization again).
                if !same_family(is_ddl, existing)
                    && is_relational_layer(request.snapshot, layer)
                    && is_relational_layer(request.snapshot, &existing.layer)
                {
                    continue;
                }
                blocks.push(format!(
                    "  CONFLICT: {} `{}` already exists in ANOTHER storage layer as {}\n\
                     {}            Splitting one concept across storage layers is drift — use \
                     the existing home or rename.",
                    rel.kind.label(),
                    rel.name,
                    display_address(&existing.address),
                    existing_summary(existing),
                ));
            }
        }
    }

    for (addition, layer) in &fresh_additions {
        let rel_key = dedup_key(&addition.relation);
        let field_key = dedup_key(&addition.field.name);
        for existing in &request.snapshot.relations {
            if existing.layer != *layer
                || dedup_key(&existing.name) != rel_key
                || !same_family(is_ddl, existing)
            {
                continue;
            }
            if let Some(field) = existing
                .fields
                .iter()
                .find(|f| dedup_key(&f.name) == field_key)
            {
                blocks.push(format!(
                    "  CONFLICT: column `{}` already exists on {} as `{}`{}\n{}",
                    addition.field.name,
                    display_address(&existing.address),
                    field.name,
                    field
                        .data_type
                        .as_deref()
                        .map(|t| format!(" ({t})"))
                        .unwrap_or_default(),
                    field
                        .intent
                        .as_deref()
                        .map(|i| format!("            purpose: {i}\n"))
                        .unwrap_or_default(),
                ));
            }
        }
    }

    // Orphaned FKs — SQL only: DDL `REFERENCES` names an exact relation, so
    // a missing target is a hard fact. ORM reference targets are derived by
    // naming heuristics (a Mongoose `ref` model vs. its real collection
    // name), and a heuristic must never hard-block (spec §12).
    if is_ddl {
        let target_visible = |layer: &str, target: &str| {
            let key = dedup_key(target);
            request
                .snapshot
                .relations
                .iter()
                .any(|r| r.layer == layer && dedup_key(&r.name) == key)
                || request
                    .proposed
                    .relations
                    .iter()
                    .any(|r| request.relation_layer(r) == layer && dedup_key(&r.name) == key)
                || request
                    .own
                    .relations
                    .iter()
                    .any(|r| request.relation_layer(r) == layer && dedup_key(&r.name) == key)
        };
        let proposed_fields = fresh_relations
            .iter()
            .flat_map(|(r, layer)| r.fields.iter().map(move |f| (r.name.as_str(), f, layer)))
            .chain(
                fresh_additions
                    .iter()
                    .map(|(a, layer)| (a.relation.as_str(), &a.field, layer)),
            );
        for (owner, field, layer) in proposed_fields {
            if let Some(target) = &field.references
                && !target_visible(layer, target)
            {
                blocks.push(format!(
                    "  CONFLICT: `{}.{}` REFERENCES `{}`, which does not exist in layer `{}` — \
                     orphaned foreign key (create the target first, or fix the name)",
                    owner, field.name, target, layer,
                ));
            }
        }
    }

    // ---- Ring 2: boundary redirects --------------------------------------
    let proposed_names = fresh_relations
        .iter()
        .map(|(r, layer)| {
            (
                r.name.clone(),
                layer.clone(),
                relation_address(layer, &r.namespace, &r.name),
            )
        })
        .chain(fresh_additions.iter().map(|(a, layer)| {
            (
                a.field.name.clone(),
                layer.clone(),
                relation_address(layer, &a.namespace, &a.relation),
            )
        }));
    for (name, layer, target_address) in proposed_names {
        let normalized = normalize_name(&name);
        for existing in &request.snapshot.relations {
            if existing.layer != layer {
                continue;
            }
            for redirect in &existing.redirects {
                if stella_graph::manifest::name_glob_match(&redirect.pattern, &normalized)
                    // Writing into the redirect's own target is the point.
                    && redirect.target != target_address
                {
                    blocks.push(format!(
                        "  BOUNDARY: `{}` does not belong here — {} declares `{}` \u{2192} {}\n{}",
                        name,
                        display_address(&existing.address),
                        redirect.pattern,
                        display_address(&redirect.target),
                        existing
                            .boundary
                            .as_deref()
                            .map(|b| format!("            boundary: {b}\n"))
                            .unwrap_or_default(),
                    ));
                }
            }
        }
    }

    if !blocks.is_empty() {
        let mut message = String::from("Storage conflict detected before write:\n\n");
        for block in &blocks {
            message.push_str(block);
            message.push('\n');
        }
        message.push_str(
            "\nDid you mean to ALTER the existing object instead? Or use a different name?\n\
             If a boundary itself is wrong, edit stella.storage.toml — that change is \
             visible in review.",
        );
        return Err(message);
    }

    // ---- Ring 3: similarity challenge ------------------------------------
    if request.storage_intent.is_none() {
        let mut challenges: Vec<String> = Vec::new();
        for (rel, layer) in &fresh_relations {
            let key = dedup_key(&rel.name);
            let mut similar: Vec<&RelationEntry> = request
                .snapshot
                .relations
                .iter()
                .filter(|existing| {
                    // Same-layer relations that are either name-identical in
                    // another namespace (multi-tenant twins — legitimate,
                    // but worth a declared intent) or token-similar. The
                    // exact same-namespace same-kind duplicate is ring 1's
                    // territory: blocked above within a family, sanctioned
                    // as materialization across families.
                    let same_ns = existing.namespace == normalize_name(&rel.namespace);
                    let same_key = dedup_key(&existing.name) == key;
                    existing.layer == *layer
                        && !(same_ns && same_key && existing.kind == rel.kind.tag())
                        && (same_key
                            || token_jaccard(&key, &dedup_key(&existing.name))
                                >= SIMILARITY_THRESHOLD)
                })
                .collect();
            similar.sort_by(|a, b| a.address.cmp(&b.address));
            if !similar.is_empty() {
                let mut entry = format!(
                    "  proposed {} `{}` resembles:\n",
                    rel.kind.label().to_lowercase(),
                    rel.name
                );
                for existing in similar {
                    entry.push_str(&format!(
                        "    {} ({}){}\n{}",
                        display_address(&existing.address),
                        existing.kind,
                        existing
                            .intent
                            .as_deref()
                            .map(|i| format!(" — {i}"))
                            .unwrap_or_default(),
                        existing
                            .source
                            .as_deref()
                            .map(|s| format!("      defined in {s}\n"))
                            .unwrap_or_default(),
                    ));
                }
                challenges.push(entry);
            }
        }
        if !challenges.is_empty() {
            let mut message =
                String::from("Similar storage objects already exist — write withheld (once):\n\n");
            for challenge in &challenges {
                message.push_str(challenge);
                message.push('\n');
            }
            message.push_str(
                "If one of these is the right home, use it instead (ALTER / add a column / \
                 reuse the relation). If this is genuinely new, retry the SAME call with a \
                 `storage_intent` argument: one sentence of purpose plus why the existing \
                 objects don't fit. The sentence is recorded in stella.storage.toml.",
            );
            return Err(message);
        }
    }

    // ---- Pass: report what this write creates ----------------------------
    let mut created: Vec<RelationEntry> = Vec::new();
    let mut intent_addresses: Vec<String> = Vec::new();
    for (rel, layer) in &fresh_relations {
        let address = relation_address(layer, &rel.namespace, &rel.name);
        intent_addresses.push(address.clone());
        created.push(RelationEntry {
            address,
            layer: layer.clone(),
            namespace: normalize_name(&rel.namespace),
            name: rel.name.clone(),
            kind: rel.kind.tag().to_string(),
            fields: rel
                .fields
                .iter()
                .map(|f| FieldEntry {
                    name: f.name.clone(),
                    data_type: f.data_type.clone(),
                    nullable: f.nullable,
                    default_value: f.default_value.clone(),
                    constraints: f.constraints.clone(),
                    references: f.references.clone(),
                    intent: f.comment.clone(),
                    line: f.line,
                })
                .collect(),
            enum_values: rel.enum_values.clone(),
            intent: rel.comment.clone(),
            boundary: None,
            redirects: Vec::new(),
            source: Some(format!("{}:{}", request.path, rel.start_line)),
        });
    }
    for (addition, layer) in &fresh_additions {
        let address = relation_address(layer, &addition.namespace, &addition.relation);
        created.push(RelationEntry {
            address: address.clone(),
            layer: layer.clone(),
            namespace: normalize_name(&addition.namespace),
            name: addition.relation.clone(),
            kind: "table".into(),
            fields: vec![FieldEntry {
                name: addition.field.name.clone(),
                data_type: addition.field.data_type.clone(),
                nullable: addition.field.nullable,
                default_value: addition.field.default_value.clone(),
                constraints: addition.field.constraints.clone(),
                references: addition.field.references.clone(),
                intent: None,
                line: addition.field.line,
            }],
            enum_values: Vec::new(),
            intent: None,
            boundary: None,
            redirects: Vec::new(),
            source: Some(format!("{}:{}", request.path, addition.field.line)),
        });
    }
    Ok(GatePass {
        created,
        intent_addresses,
    })
}

/// One existing relation, summarized for a conflict message: provenance
/// plus its column list, so the model can decide without another read.
fn existing_summary(existing: &RelationEntry) -> String {
    let mut out = String::new();
    if let Some(source) = &existing.source {
        out.push_str(&format!("            defined in {source}\n"));
    }
    if let Some(intent) = &existing.intent {
        out.push_str(&format!("            purpose: {intent}\n"));
    }
    if !existing.fields.is_empty() {
        let columns: Vec<String> = existing
            .fields
            .iter()
            .map(|f| match &f.data_type {
                Some(ty) => format!("{} {ty}", f.name),
                None => f.name.clone(),
            })
            .collect();
        out.push_str(&format!("            columns: {}\n", columns.join(", ")));
    }
    out
}

/// Jaccard overlap of the `_`-separated token sets of two dedup keys.
fn token_jaccard(a: &str, b: &str) -> f64 {
    let set_a: HashSet<&str> = a.split('_').filter(|t| !t.is_empty()).collect();
    let set_b: HashSet<&str> = b.split('_').filter(|t| !t.is_empty()).collect();
    if set_a.is_empty() || set_b.is_empty() {
        return 0.0;
    }
    let intersection = set_a.intersection(&set_b).count() as f64;
    let union = set_a.union(&set_b).count() as f64;
    intersection / union
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_graph::storage::RedirectEntry;

    fn extract(path: &str, source: &str) -> StorageExtract {
        extractor().expect("grammars arm").extract(path, source)
    }

    fn table(layer: &str, ns: &str, name: &str, fields: &[&str]) -> RelationEntry {
        RelationEntry {
            address: relation_address(layer, ns, name),
            layer: normalize_name(layer),
            namespace: normalize_name(ns),
            name: name.to_string(),
            kind: "table".into(),
            fields: fields
                .iter()
                .map(|f| FieldEntry {
                    name: f.to_string(),
                    data_type: Some("TEXT".into()),
                    nullable: true,
                    default_value: None,
                    constraints: vec![],
                    references: None,
                    intent: None,
                    line: 1,
                })
                .collect(),
            enum_values: vec![],
            intent: None,
            boundary: None,
            redirects: vec![],
            source: Some("migrations/001.sql:1".into()),
        }
    }

    fn snapshot(relations: Vec<RelationEntry>) -> StorageSnapshot {
        StorageSnapshot {
            layers: vec![],
            relations,
            orphaned_meanings: vec![],
        }
    }

    fn run_at(
        snapshot: &StorageSnapshot,
        path: &str,
        proposed_src: &str,
        own_src: &str,
        intent: Option<&str>,
    ) -> Result<GatePass, String> {
        let proposed = extract(path, proposed_src);
        let own = extract(path, own_src);
        check(&GateRequest {
            path,
            manifest_layer: None,
            proposed: &proposed,
            own: &own,
            snapshot,
            storage_intent: intent,
        })
    }

    fn run(
        snapshot: &StorageSnapshot,
        proposed_sql: &str,
        own_sql: &str,
        intent: Option<&str>,
    ) -> Result<GatePass, String> {
        run_at(
            snapshot,
            "migrations/002.sql",
            proposed_sql,
            own_sql,
            intent,
        )
    }

    #[test]
    fn ring1_blocks_normalized_name_duplicate() {
        let snap = snapshot(vec![table("sql", "default", "payment_records", &["id"])]);
        // Different case, different plurality — still the same relation.
        let err = run(&snap, "CREATE TABLE PaymentRecord (id INT);", "", None).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("store://sql/default/payment_records"), "{err}");
    }

    #[test]
    fn ring1_blocks_duplicate_column_via_alter() {
        let snap = snapshot(vec![table("sql", "default", "payments", &["id", "amount"])]);
        let err = run(
            &snap,
            "ALTER TABLE payments ADD COLUMN Amounts NUMERIC;",
            "",
            None,
        )
        .unwrap_err();
        assert!(err.contains("column `Amounts` already exists"), "{err}");
    }

    #[test]
    fn ring1_blocks_cross_layer_duplicate() {
        let mut other = table("docs-mongo", "app", "refunds", &["id"]);
        other.layer = "docs_mongo".into();
        let snap = snapshot(vec![other]);
        let err = run(&snap, "CREATE TABLE refunds (id INT);", "", None).unwrap_err();
        assert!(err.contains("ANOTHER storage layer"), "{err}");
    }

    #[test]
    fn ring1_blocks_orphaned_foreign_key() {
        let snap = snapshot(vec![]);
        let err = run(
            &snap,
            "CREATE TABLE payments (user_id INT REFERENCES users(id));",
            "",
            None,
        )
        .unwrap_err();
        assert!(err.contains("orphaned foreign key"), "{err}");
    }

    #[test]
    fn fk_to_a_table_created_in_the_same_write_is_fine() {
        let snap = snapshot(vec![]);
        let pass = run(
            &snap,
            "CREATE TABLE users (id INT PRIMARY KEY);\n\
             CREATE TABLE payments (user_id INT REFERENCES users(id));",
            "",
            None,
        )
        .unwrap();
        assert_eq!(pass.created.len(), 2);
    }

    #[test]
    fn ring2_redirect_blocks_misplaced_column_with_pointer() {
        let mut payments = table("sql", "default", "payments", &["id", "amount"]);
        payments.boundary = Some("Refund state lives in refunds, not here.".into());
        payments.redirects = vec![RedirectEntry {
            pattern: "refund*".into(),
            target: "sql/default/refunds".into(),
        }];
        let snap = snapshot(vec![payments, table("sql", "default", "refunds", &["id"])]);
        let err = run(
            &snap,
            "ALTER TABLE payments ADD COLUMN refund_status TEXT;",
            "",
            None,
        )
        .unwrap_err();
        assert!(err.contains("BOUNDARY"), "{err}");
        assert!(err.contains("store://sql/default/refunds"), "{err}");
    }

    #[test]
    fn ring2_allows_writes_into_the_redirect_target_itself() {
        let mut payments = table("sql", "default", "payments", &["id"]);
        payments.redirects = vec![RedirectEntry {
            pattern: "refund*".into(),
            target: "sql/default/refunds".into(),
        }];
        let snap = snapshot(vec![payments, table("sql", "default", "refunds", &["id"])]);
        let pass = run(
            &snap,
            "ALTER TABLE refunds ADD COLUMN refund_reason TEXT;",
            "",
            None,
        );
        assert!(pass.is_ok(), "{:?}", pass.err());
    }

    #[test]
    fn ring3_challenges_similar_name_then_passes_with_intent() {
        let mut payments = table("sql", "default", "payments", &["id"]);
        payments.intent = Some("One row per charge attempt.".into());
        let snap = snapshot(vec![payments]);

        // First attempt: withheld with the evidence.
        let err = run(&snap, "CREATE TABLE payment_records (id INT);", "", None).unwrap_err();
        assert!(err.contains("write withheld"), "{err}");
        assert!(err.contains("One row per charge attempt."), "{err}");
        assert!(err.contains("storage_intent"), "{err}");

        // Retry with a declared intent: passes, and reports the address the
        // intent should be recorded against.
        let pass = run(
            &snap,
            "CREATE TABLE payment_records (id INT);",
            "",
            Some("Immutable ledger of imported legacy charges; payments holds live charges."),
        )
        .unwrap();
        assert_eq!(pass.intent_addresses, vec!["sql/default/payment_records"]);
    }

    #[test]
    fn dissimilar_new_table_passes_without_intent() {
        let snap = snapshot(vec![table("sql", "default", "payments", &["id"])]);
        let pass = run(&snap, "CREATE TABLE audit_log (id INT);", "", None).unwrap();
        assert_eq!(pass.created.len(), 1);
        assert_eq!(pass.created[0].address, "sql/default/audit_log");
        assert_eq!(
            pass.created[0].source.as_deref(),
            Some("migrations/002.sql:1"),
            "created objects carry provenance for family checks"
        );
    }

    #[test]
    fn own_file_rewrite_is_exempt() {
        let snap = snapshot(vec![table("sql", "default", "users", &["id"])]);
        let pass = run(
            &snap,
            "CREATE TABLE users (id INT, email TEXT);",
            "CREATE TABLE users (id INT);",
            None,
        );
        assert!(
            pass.is_ok(),
            "same-file rewrite must pass: {:?}",
            pass.err()
        );
    }

    #[test]
    fn multi_tenant_same_name_in_other_namespace_is_challenged_not_blocked() {
        // Same layer, different namespace: legitimate in multi-tenant
        // schemas, so ring 1 stays silent — but it IS name-identical, so
        // ring 3 asks for intent.
        let snap = snapshot(vec![table("sql", "tenant_a", "orders", &["id"])]);
        let err = run(&snap, "CREATE TABLE tenant_b.orders (id INT);", "", None).unwrap_err();
        assert!(err.contains("write withheld"), "{err}");
        let pass = run(
            &snap,
            "CREATE TABLE tenant_b.orders (id INT);",
            "",
            Some("Tenant B's orders; tenants are namespace-isolated."),
        );
        assert!(pass.is_ok());
    }

    #[test]
    fn token_jaccard_scores() {
        assert!(token_jaccard("payment", "payment_record") >= 0.5);
        assert!(token_jaccard("payment", "refund") < 0.5);
        assert_eq!(token_jaccard("invoice_line", "line_item"), 1.0 / 3.0);
    }

    // ---- Cross-adapter behavior ------------------------------------------

    const PAYMENTS_PRISMA: &str =
        "model Payment {\n  id Int @id\n  amount Decimal\n  @@map(\"payments\")\n}\n";

    #[test]
    fn ddl_materializing_an_orm_model_passes_clean() {
        // The index knows `payments` from a Prisma schema; writing the SQL
        // migration Prisma generates for it is the same entity described
        // twice (spec §4a) — not a duplicate, not even a challenge.
        let mut model = table("sql", "default", "payments", &["id", "amount"]);
        model.source = Some("prisma/schema.prisma:3".into());
        let snap = snapshot(vec![model]);
        let pass = run(
            &snap,
            "CREATE TABLE payments (id INT PRIMARY KEY, amount NUMERIC);",
            "",
            None,
        );
        assert!(pass.is_ok(), "{:?}", pass.err());
    }

    #[test]
    fn orm_model_adopting_an_existing_table_passes_clean() {
        // The reverse direction: writing a Prisma model over an indexed SQL
        // table of the same name.
        let snap = snapshot(vec![table("sql", "default", "payments", &["id", "amount"])]);
        let pass = run_at(&snap, "prisma/schema.prisma", PAYMENTS_PRISMA, "", None);
        assert!(pass.is_ok(), "{:?}", pass.err());
    }

    #[test]
    fn second_orm_model_for_the_same_table_still_blocks() {
        // Same family (schema-as-code vs schema-as-code) → ring 1 fires.
        let mut model = table("sql", "default", "payments", &["id"]);
        model.source = Some("src/db/schema.ts:10".into());
        let snap = snapshot(vec![model]);
        let err = run_at(&snap, "prisma/schema.prisma", PAYMENTS_PRISMA, "", None).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn orm_near_name_still_gets_the_ring3_challenge() {
        let mut payments = table("sql", "default", "payments", &["id"]);
        payments.intent = Some("One row per charge attempt.".into());
        let snap = snapshot(vec![payments]);
        let err = run_at(
            &snap,
            "prisma/schema.prisma",
            "model PaymentRecord {\n  id Int @id\n}\n",
            "",
            None,
        )
        .unwrap_err();
        assert!(err.contains("write withheld"), "{err}");
        assert!(err.contains("One row per charge attempt."), "{err}");
    }

    #[test]
    fn mongoose_collection_duplicating_a_sql_table_blocks_cross_layer() {
        // The spec §1 turn-160 case: refunds exists in Postgres; an agent
        // writes a Mongoose `refunds` collection. Different layer by
        // construction → cross-layer conflict, with the existing address.
        let snap = snapshot(vec![table("sql", "default", "refunds", &["id"])]);
        let err = run_at(
            &snap,
            "src/models/refund.js",
            "const mongoose = require('mongoose');\n\
             const s = new mongoose.Schema({ amount: Number });\n\
             mongoose.model('Refund', s);\n",
            "",
            None,
        )
        .unwrap_err();
        assert!(err.contains("ANOTHER storage layer"), "{err}");
        assert!(err.contains("store://sql/default/refunds"), "{err}");
    }

    #[test]
    fn editing_a_model_in_place_gates_the_added_field() {
        // No ALTER statement anywhere: the field diff between the proposed
        // and current model is the addition, and ring 2's redirect fires.
        let mut payments = table("sql", "default", "payments", &["id", "amount"]);
        payments.source = Some("prisma/schema.prisma:1".into());
        payments.boundary = Some("Refund state lives in refunds, not here.".into());
        payments.redirects = vec![RedirectEntry {
            pattern: "refund*".into(),
            target: "sql/default/refunds".into(),
        }];
        let snap = snapshot(vec![payments, table("sql", "default", "refunds", &["id"])]);
        let own = "model Payment {\n  id Int @id\n  amount Decimal\n  @@map(\"payments\")\n}\n";
        let proposed = "model Payment {\n  id Int @id\n  amount Decimal\n  refundStatus String\n  @@map(\"payments\")\n}\n";
        let err = run_at(&snap, "prisma/schema.prisma", proposed, own, None).unwrap_err();
        assert!(err.contains("BOUNDARY"), "{err}");
        assert!(err.contains("store://sql/default/refunds"), "{err}");
    }

    #[test]
    fn orm_reference_heuristics_never_block_as_orphaned_fks() {
        // A Mongoose `ref` points at a model whose collection name is a
        // naming guess — heuristics warn (via the index), never block.
        let snap = snapshot(vec![]);
        let pass = run_at(
            &snap,
            "src/models/order.js",
            "const mongoose = require('mongoose');\n\
             const s = new mongoose.Schema({ user: { type: mongoose.Schema.Types.ObjectId, ref: 'User' } });\n\
             mongoose.model('Order', s);\n",
            "",
            None,
        );
        assert!(pass.is_ok(), "{:?}", pass.err());
    }

    #[test]
    fn manifest_layer_claim_overrides_the_adapter_hint() {
        // A mongoose file claimed by a manifest layer gates in THAT layer.
        let existing = {
            let mut t = table("docs-mongo", "default", "orders", &["id"]);
            t.layer = "docs_mongo".into();
            t.source = Some("src/models/other.js:1".into());
            t
        };
        let snap = snapshot(vec![existing]);
        let proposed = extract(
            "src/models/order.js",
            "const mongoose = require('mongoose');\n\
             const s = new mongoose.Schema({ n: Number });\n\
             mongoose.model('Order', s);\n",
        );
        let own = StorageExtract::default();
        let err = check(&GateRequest {
            path: "src/models/order.js",
            manifest_layer: Some("docs-mongo"),
            proposed: &proposed,
            own: &own,
            snapshot: &snap,
            storage_intent: None,
        })
        .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }
}
