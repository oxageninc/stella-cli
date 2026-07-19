//! The pre-write storage gate — makes duplicate or misplaced schema hard to
//! write (`docs/design/storage-map.md` §8).
//!
//! When `write_file` / `edit_file` targets a storage-definition file, the
//! proposed content is parsed by the SAME adapter extraction the indexer
//! uses (`stella_graph::storage`), so the gate and the index cannot drift
//! apart. Three rings:
//!
//! - **Ring 1 (deterministic → block):** normalized-name duplicate relations
//!   (same namespace, and cross-layer), duplicate fields on an existing
//!   relation, orphaned foreign keys.
//! - **Ring 2 (boundary redirects → block with a pointer):** a manifest
//!   boundary that names where the concept lives redirects the write there.
//! - **Ring 3 (similarity → fail once, pass with declared intent):** a
//!   proposed relation lexically close to an existing one fails once with
//!   the evidence; retrying with `storage_intent` (one sentence of purpose
//!   plus why the existing objects don't fit) passes, and the sentence is
//!   recorded in `stella.storage.toml` — duplication costs a durable,
//!   reviewable justification instead of being the path of least
//!   resistance.
//!
//! Deterministic core: rings 1–2 are name/lookup logic; ring 3 is a token
//! comparison that can only *withhold once*, never permanently block.

use std::collections::HashSet;

use stella_graph::storage::{
    FieldEntry, RelationEntry, StorageExtract, dedup_key, display_address, normalize_name,
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

/// Whether a path is a storage-definition file worth gating. Delegates to
/// the shared adapter membership test.
pub fn is_schema_file(path: &str) -> bool {
    stella_graph::storage::is_storage_file(path)
}

/// The storage layer a workspace-relative path belongs to, per the
/// manifest's `paths` globs (or the implicit SQL layer).
pub fn layer_for(root: &std::path::Path, rel_path: &str) -> String {
    stella_graph::manifest::StorageManifest::load(root)
        .ok()
        .flatten()
        .map(|m| m.layer_for(rel_path))
        .unwrap_or_else(|| stella_graph::storage::DEFAULT_SQL_LAYER.to_string())
}

/// Everything the gate needs to judge one proposed write.
pub struct GateRequest<'a> {
    /// Layer the target file belongs to.
    pub layer: &'a str,
    /// Extraction of the *proposed* content (write_file `content` /
    /// edit_file `new_string`).
    pub proposed: &'a StorageExtract,
    /// Extraction of the target file's *current* content — objects the file
    /// already defines are exempt (rewriting a migration in place is not a
    /// duplicate).
    pub own: &'a StorageExtract,
    /// The assembled storage map (persisted index + manifest + session
    /// overlay).
    pub snapshot: &'a StorageSnapshot,
    /// The model's declared intent, when the call carries `storage_intent`.
    pub storage_intent: Option<&'a str>,
}

/// A passed gate: the objects this write creates (recorded into the session
/// overlay after the write lands) and, when an intent was declared, the
/// relation addresses it should be recorded against.
#[derive(Debug)]
pub struct GatePass {
    pub created: Vec<RelationEntry>,
    pub intent_addresses: Vec<String>,
}

/// Judge a proposed write. `Err(message)` is the tool error the model sees
/// (ring 1/2 block, or ring 3 challenge); `Ok` carries what to record.
pub fn check(request: &GateRequest<'_>) -> Result<GatePass, String> {
    let layer = normalize_name(request.layer);
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

    let fresh_relations: Vec<_> = request
        .proposed
        .relations
        .iter()
        .filter(|r| !own_relations.contains(&(normalize_name(&r.namespace), dedup_key(&r.name))))
        .collect();
    let fresh_additions: Vec<_> = request
        .proposed
        .additions
        .iter()
        .filter(|a| !own_additions.contains(&(dedup_key(&a.relation), dedup_key(&a.field.name))))
        .collect();

    if fresh_relations.is_empty() && fresh_additions.is_empty() {
        return Ok(GatePass {
            created: Vec::new(),
            intent_addresses: Vec::new(),
        });
    }

    let mut blocks: Vec<String> = Vec::new();

    // ---- Ring 1: deterministic conflicts ---------------------------------
    for rel in &fresh_relations {
        let ns = normalize_name(&rel.namespace);
        let key = dedup_key(&rel.name);
        for existing in &request.snapshot.relations {
            if dedup_key(&existing.name) != key || existing.kind != rel.kind.tag() {
                continue;
            }
            if existing.layer == layer && existing.namespace == ns {
                blocks.push(format!(
                    "  CONFLICT: {} `{}` already exists as {}\n{}",
                    rel.kind.label(),
                    rel.name,
                    display_address(&existing.address),
                    existing_summary(existing),
                ));
            } else if existing.layer != layer {
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

    for addition in &fresh_additions {
        let rel_key = dedup_key(&addition.relation);
        let field_key = dedup_key(&addition.field.name);
        for existing in &request.snapshot.relations {
            if existing.layer != layer || dedup_key(&existing.name) != rel_key {
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

    // Orphaned FKs: a REFERENCES target must exist somewhere the write can
    // see — the snapshot (same layer), this file's own objects, or another
    // relation proposed in the same write.
    let visible_targets: HashSet<String> = request
        .snapshot
        .relations
        .iter()
        .filter(|r| r.layer == layer)
        .map(|r| dedup_key(&r.name))
        .chain(
            request
                .proposed
                .relations
                .iter()
                .map(|r| dedup_key(&r.name)),
        )
        .chain(request.own.relations.iter().map(|r| dedup_key(&r.name)))
        .collect();
    let proposed_fields = fresh_relations
        .iter()
        .flat_map(|r| r.fields.iter().map(move |f| (r.name.as_str(), f)))
        .chain(
            fresh_additions
                .iter()
                .map(|a| (a.relation.as_str(), &a.field)),
        );
    for (owner, field) in proposed_fields {
        if let Some(target) = &field.references
            && !visible_targets.contains(&dedup_key(target))
        {
            blocks.push(format!(
                "  CONFLICT: `{}.{}` REFERENCES `{}`, which does not exist in layer `{}` — \
                 orphaned foreign key (create the target first, or fix the name)",
                owner, field.name, target, layer,
            ));
        }
    }

    // ---- Ring 2: boundary redirects --------------------------------------
    let proposed_names = fresh_relations
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                relation_address(&layer, &r.namespace, &r.name),
            )
        })
        .chain(fresh_additions.iter().map(|a| {
            (
                a.field.name.clone(),
                relation_address(&layer, &a.namespace, &a.relation),
            )
        }));
    for (name, target_address) in proposed_names {
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
        for rel in &fresh_relations {
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
                    // territory and was already blocked above.
                    let same_ns = existing.namespace == normalize_name(&rel.namespace);
                    let same_key = dedup_key(&existing.name) == key;
                    existing.layer == layer
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
    for rel in &fresh_relations {
        let address = relation_address(&layer, &rel.namespace, &rel.name);
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
            source: None,
        });
    }
    for addition in &fresh_additions {
        let address = relation_address(&layer, &addition.namespace, &addition.relation);
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
            source: None,
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

    fn extract(sql: &str) -> StorageExtract {
        extractor().expect("grammars arm").extract_sql(sql)
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

    fn run(
        snapshot: &StorageSnapshot,
        proposed_sql: &str,
        own_sql: &str,
        intent: Option<&str>,
    ) -> Result<GatePass, String> {
        let proposed = extract(proposed_sql);
        let own = extract(own_sql);
        check(&GateRequest {
            layer: "sql",
            proposed: &proposed,
            own: &own,
            snapshot,
            storage_intent: intent,
        })
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
}
