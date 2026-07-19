//! The Prisma adapter (spec §4a): `model` / `enum` / `view` blocks in
//! `.prisma` schema files, plus the `datasource` block that names the
//! engine. Prisma's DSL is line-oriented (the official formatter normalizes
//! one field per line), so this is a deterministic hand parser — no grammar
//! dependency, no LLM, and an unrecognized line contributes nothing rather
//! than garbage.
//!
//! What it captures:
//! - model → relation (`@@map` names the table; `@@schema` the namespace);
//!   `datasource provider = "mongodb"` turns models into collections in the
//!   implicit mongo layer.
//! - scalar fields → columns: type as written, `?` = nullable, `@id` /
//!   `@unique` / `@default(…)` / `@map(…)` / `@db.X(…)`; `@@id([…])` and
//!   `@@unique([…])` fold onto the named columns.
//! - relation fields (model-typed) are not columns; their
//!   `@relation(fields: […], references: […])` marks the named scalar
//!   columns as foreign keys to the target model's table.
//! - `///` doc comments harvest as meaning (spec §4b), model- and
//!   field-level. `enum` blocks become [`RelationKind::EnumType`] with their
//!   variants.

use std::collections::HashMap;

use super::{
    DEFAULT_MONGO_LAYER, DEFAULT_NAMESPACE, FieldDef, RelationDef, RelationKind, StorageExtract,
};

pub(super) fn extract(source: &str) -> StorageExtract {
    let mut out = StorageExtract::default();
    let blocks = split_blocks(source);

    let mongo = blocks.iter().any(|b| {
        b.keyword == "datasource"
            && b.lines
                .iter()
                .any(|(_, l)| assignment_value(l, "provider") == Some("mongodb"))
    });
    let layer_hint = mongo.then(|| DEFAULT_MONGO_LAYER.to_string());

    // Pass 1: names. Models map to their `@@map` table name (else the model
    // name); enums carry their variants — both are needed to classify field
    // types in pass 2.
    let mut table_names: HashMap<String, String> = HashMap::new();
    let mut enums: HashMap<String, Vec<String>> = HashMap::new();
    for block in &blocks {
        match block.keyword.as_str() {
            "model" | "view" => {
                table_names.insert(block.name.clone(), block_map_name(block));
            }
            "enum" => {
                let values = block
                    .lines
                    .iter()
                    .map(|(_, l)| l.trim())
                    .filter(|l| {
                        !l.is_empty()
                            && !l.starts_with("//")
                            && !l.starts_with("@@")
                            && l.chars().all(|c| c.is_alphanumeric() || c == '_')
                    })
                    .map(|l| l.to_string())
                    .collect();
                enums.insert(block.name.clone(), values);
            }
            _ => {}
        }
    }

    // Pass 2: build relations.
    for block in &blocks {
        match block.keyword.as_str() {
            "model" | "view" => {
                let kind = match (mongo, block.keyword.as_str()) {
                    (_, "view") => RelationKind::View,
                    (true, _) => RelationKind::Collection,
                    (false, _) => RelationKind::Table,
                };
                out.relations.push(decode_model(
                    block,
                    kind,
                    &table_names,
                    &enums,
                    layer_hint.clone(),
                ));
            }
            "enum" => {
                out.relations.push(RelationDef {
                    name: block_map_name(block),
                    namespace: DEFAULT_NAMESPACE.to_string(),
                    kind: RelationKind::EnumType,
                    fields: Vec::new(),
                    enum_values: enums.get(&block.name).cloned().unwrap_or_default(),
                    comment: block.doc.clone(),
                    layer_hint: layer_hint.clone(),
                    start_line: block.start_line,
                    end_line: block.end_line,
                });
            }
            _ => {}
        }
    }
    out
}

/// One top-level `keyword Name { … }` block: its body lines (with 1-based
/// line numbers) and any `///` doc text immediately above the header.
struct Block {
    keyword: String,
    name: String,
    doc: Option<String>,
    lines: Vec<(u32, String)>,
    start_line: u32,
    end_line: u32,
}

fn split_blocks(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut current: Option<Block> = None;
    let mut depth = 0i32;
    let mut doc: Vec<String> = Vec::new();
    for (i, line) in source.lines().enumerate() {
        let number = i as u32 + 1;
        let trimmed = line.trim();
        if depth == 0 {
            if let Some(text) = trimmed.strip_prefix("///") {
                doc.push(text.trim().to_string());
                continue;
            }
            if let Some((keyword, name)) = block_header(trimmed) {
                current = Some(Block {
                    keyword,
                    name,
                    doc: (!doc.is_empty()).then(|| doc.join(" ")),
                    lines: Vec::new(),
                    start_line: number,
                    end_line: number,
                });
            }
            if !trimmed.is_empty() {
                doc.clear();
            }
        } else if let Some(block) = &mut current {
            // Body line, recorded before depth accounting so a same-line
            // `}` still keeps everything before it.
            if trimmed != "}" {
                block.lines.push((number, trimmed.to_string()));
            }
        }
        depth += trimmed.matches('{').count() as i32;
        depth -= trimmed.matches('}').count() as i32;
        if depth <= 0 {
            depth = 0;
            if let Some(mut block) = current.take() {
                block.end_line = number;
                blocks.push(block);
            }
        }
    }
    blocks
}

/// `model Payment {` → `("model", "Payment")`.
fn block_header(line: &str) -> Option<(String, String)> {
    let rest = line.strip_suffix('{')?.trim_end();
    let (keyword, name) = rest.split_once(char::is_whitespace)?;
    if !matches!(
        keyword,
        "model" | "enum" | "view" | "type" | "datasource" | "generator"
    ) {
        return None;
    }
    let name = name.trim();
    (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| (keyword.to_string(), name.to_string()))
}

/// The relation name a block indexes under: `@@map("payments")` wins over
/// the model name (that is Prisma's own table-naming rule).
fn block_map_name(block: &Block) -> String {
    block
        .lines
        .iter()
        .find_map(|(_, l)| attribute_string_arg(l.trim(), "@@map"))
        .unwrap_or_else(|| block.name.clone())
}

fn decode_model(
    block: &Block,
    kind: RelationKind,
    table_names: &HashMap<String, String>,
    enums: &HashMap<String, Vec<String>>,
    layer_hint: Option<String>,
) -> RelationDef {
    let mut rel = RelationDef {
        name: block_map_name(block),
        namespace: block
            .lines
            .iter()
            .find_map(|(_, l)| attribute_string_arg(l.trim(), "@@schema"))
            .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
        kind,
        fields: Vec::new(),
        enum_values: Vec::new(),
        comment: block.doc.clone(),
        layer_hint,
        start_line: block.start_line,
        end_line: block.end_line,
    };

    // Relation-field FK info to fold onto scalar columns after the field
    // pass: (local field name, target table name). Matching uses the
    // *Prisma* field names — `@relation(fields: [userId])` refers to the
    // field as declared, even when `@map` renames its column.
    let mut fk_marks: Vec<(String, String)> = Vec::new();
    let mut prisma_names: Vec<String> = Vec::new();
    let mut doc: Vec<String> = Vec::new();

    for (number, line) in &block.lines {
        let line = line.trim();
        if let Some(text) = line.strip_prefix("///") {
            doc.push(text.trim().to_string());
            continue;
        }
        if line.is_empty() || line.starts_with("//") {
            doc.clear();
            continue;
        }
        if line.starts_with("@@") {
            doc.clear();
            apply_block_attribute(line, &mut rel);
            continue;
        }
        let Some((name, type_token, attrs)) = field_parts(line) else {
            doc.clear();
            continue;
        };
        let comment = (!doc.is_empty()).then(|| doc.join(" "));
        doc.clear();

        let base_type = type_token.trim_end_matches(['?', '!']);
        let base_type = base_type.strip_suffix("[]").unwrap_or(base_type);
        if let Some(target) = table_names.get(base_type) {
            // Model-typed relation field: not a column. Its @relation
            // attribute names the scalar columns that ARE the FK.
            for local in relation_fields_arg(&attrs) {
                fk_marks.push((local, target.to_lowercase()));
            }
            continue;
        }

        let mut field = FieldDef {
            name: name.to_string(),
            data_type: Some(type_token.trim_end_matches('?').to_string()),
            nullable: type_token.ends_with('?'),
            comment,
            line: *number,
            ..FieldDef::default()
        };
        if let Some(values) = enums.get(base_type) {
            field
                .constraints
                .push(format!("enum: {}", values.join("|")));
        }
        for attr in split_attributes(&attrs) {
            apply_field_attribute(&attr, &mut field);
        }
        prisma_names.push(name.to_string());
        rel.fields.push(field);
    }

    for (local, target) in fk_marks {
        if let Some(at) = prisma_names.iter().position(|n| *n == local)
            && let Some(field) = rel.fields.get_mut(at)
        {
            field.constraints.push(format!("REFERENCES {target}"));
            field.references = Some(target);
        }
    }
    rel
}

/// `name Type attrs…` → the three parts, or `None` for non-field lines.
/// Prisma aligns columns with runs of spaces, so this walks byte offsets
/// rather than splitting (consecutive delimiters must not yield tokens).
fn field_parts(line: &str) -> Option<(&str, &str, String)> {
    let name_end = line.find(char::is_whitespace)?;
    let name = &line[..name_end];
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let after_name = &line[name_end..];
    let type_offset = name_end + after_name.find(|c: char| !c.is_whitespace())?;
    let type_end = line[type_offset..]
        .find(char::is_whitespace)
        .map(|i| type_offset + i)
        .unwrap_or(line.len());
    let type_token = &line[type_offset..type_end];
    if type_token.starts_with('@') {
        return None;
    }
    Some((name, type_token, line[type_end..].trim().to_string()))
}

fn apply_field_attribute(attr: &str, field: &mut FieldDef) {
    if attr == "@id" {
        field.nullable = false;
        field.constraints.push("PRIMARY KEY".into());
    } else if attr == "@unique" || attr.starts_with("@unique(") {
        field.constraints.push("UNIQUE".into());
    } else if let Some(value) = paren_arg(attr, "@default") {
        field.default_value = Some(value);
    } else if let Some(mapped) = attribute_string_arg(attr, "@map") {
        // The column's database name; the map indexes the real column.
        field.name = mapped;
    } else if attr.starts_with("@db.") {
        field.constraints.push(attr.to_string());
    }
}

fn apply_block_attribute(line: &str, rel: &mut RelationDef) {
    let mark = |rel: &mut RelationDef, names: Vec<String>, constraint: &str, not_null: bool| {
        for name in names {
            if let Some(field) = rel.fields.iter_mut().find(|f| f.name == name) {
                if !field.constraints.iter().any(|c| c == constraint) {
                    field.constraints.push(constraint.to_string());
                }
                if not_null {
                    field.nullable = false;
                }
            }
        }
    };
    if let Some(arg) = paren_arg(line, "@@id") {
        mark(rel, bracket_list(&arg), "PRIMARY KEY", true);
    } else if let Some(arg) = paren_arg(line, "@@unique") {
        mark(rel, bracket_list(&arg), "UNIQUE", false);
    }
}

/// The `fields: [a, b]` list of an `@relation(...)` attribute.
fn relation_fields_arg(attrs: &str) -> Vec<String> {
    split_attributes(attrs)
        .iter()
        .find_map(|a| paren_arg(a, "@relation"))
        .and_then(|args| args.split("fields:").nth(1).map(bracket_list))
        .unwrap_or_default()
}

/// `[a, b]` (or the prefix of one) → the identifier list.
fn bracket_list(text: &str) -> Vec<String> {
    let Some(open) = text.find('[') else {
        return Vec::new();
    };
    let Some(close) = text[open..].find(']') else {
        return Vec::new();
    };
    text[open + 1..open + close]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split an attribute tail into `@…` tokens, respecting parens and quotes
/// (`@default("a@b") @unique` is two attributes, not three).
fn split_attributes(attrs: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_string = false;
    for ch in attrs.chars() {
        match ch {
            '"' => in_string = !in_string,
            '(' | '[' if !in_string => depth += 1,
            ')' | ']' if !in_string => depth -= 1,
            '@' if !in_string && depth == 0 && !current.is_empty() => {
                out.push(std::mem::take(&mut current).trim().to_string());
            }
            _ => {}
        }
        current.push(ch);
    }
    let tail = current.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out.retain(|a| a.starts_with('@'));
    out
}

/// The balanced-paren argument of `prefix(...)`, e.g.
/// `paren_arg("@default(now())", "@default")` → `now()`.
fn paren_arg(text: &str, prefix: &str) -> Option<String> {
    let rest = text.strip_prefix(prefix)?.trim_start();
    let rest = rest.strip_prefix('(')?;
    let mut depth = 1i32;
    let mut in_string = false;
    for (i, ch) in rest.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[..i].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// The string literal inside `prefix("…")`, e.g. `@@map("payments")`.
fn attribute_string_arg(text: &str, prefix: &str) -> Option<String> {
    let arg = paren_arg(text, prefix)?;
    let arg = arg.trim();
    arg.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .map(|s| s.to_string())
}

/// `provider = "postgresql"` → the quoted value, when `line` assigns `key`.
fn assignment_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (lhs, rhs) = line.split_once('=')?;
    if lhs.trim() != key {
        return None;
    }
    rhs.trim().strip_prefix('"')?.strip_suffix('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"
datasource db {
  provider = "postgresql"
  url      = env("DATABASE_URL")
}

generator client {
  provider = "prisma-client-js"
}

/// One row per charge attempt, successful or not.
model Payment {
  id             Int      @id @default(autoincrement())
  amount         Decimal  @db.Decimal(10, 2)
  /// ISO currency code the amount is denominated in.
  currency       String?  @default("USD")
  userId         Int      @map("user_id")
  user           User     @relation(fields: [userId], references: [id])
  status         PaymentStatus @default(PENDING)
  idempotencyKey String   @unique

  @@map("payments")
  @@schema("billing")
}

model User {
  id    Int    @id
  email String @unique

  @@map("users")
}

enum PaymentStatus {
  PENDING
  COMPLETED
  FAILED
}
"#;

    fn relation<'a>(out: &'a StorageExtract, name: &str) -> &'a RelationDef {
        out.relations
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("relation {name} missing: {:?}", out.relations))
    }

    #[test]
    fn models_map_to_tables_with_map_and_schema() {
        let out = extract(SCHEMA);
        let payments = relation(&out, "payments");
        assert_eq!(payments.kind, RelationKind::Table);
        assert_eq!(payments.namespace, "billing");
        assert_eq!(
            payments.comment.as_deref(),
            Some("One row per charge attempt, successful or not.")
        );
        assert!(payments.layer_hint.is_none(), "relational → sql layer");
    }

    #[test]
    fn scalar_fields_carry_types_nullability_defaults_constraints() {
        let out = extract(SCHEMA);
        let payments = relation(&out, "payments");

        let id = payments.fields.iter().find(|f| f.name == "id").unwrap();
        assert!(!id.nullable);
        assert!(id.constraints.contains(&"PRIMARY KEY".to_string()));
        assert_eq!(id.default_value.as_deref(), Some("autoincrement()"));

        let amount = payments.fields.iter().find(|f| f.name == "amount").unwrap();
        assert_eq!(amount.data_type.as_deref(), Some("Decimal"));
        assert!(!amount.nullable, "required scalar is NOT NULL");
        assert!(
            amount.constraints.iter().any(|c| c.contains("@db.Decimal")),
            "{:?}",
            amount.constraints
        );

        let currency = payments
            .fields
            .iter()
            .find(|f| f.name == "currency")
            .unwrap();
        assert!(currency.nullable, "`String?` is nullable");
        assert_eq!(currency.default_value.as_deref(), Some("\"USD\""));
        assert_eq!(
            currency.comment.as_deref(),
            Some("ISO currency code the amount is denominated in.")
        );

        let key = payments
            .fields
            .iter()
            .find(|f| f.name == "idempotencyKey")
            .unwrap();
        assert!(key.constraints.contains(&"UNIQUE".to_string()));
    }

    #[test]
    fn relation_fields_become_fks_on_the_scalar_column_not_columns() {
        let out = extract(SCHEMA);
        let payments = relation(&out, "payments");
        // `user User @relation(...)` is not a column…
        assert!(payments.fields.iter().all(|f| f.name != "user"));
        // …but `userId` (mapped to user_id) carries the FK to users.
        let user_id = payments
            .fields
            .iter()
            .find(|f| f.name == "user_id")
            .expect("@map renames the column");
        assert_eq!(user_id.references.as_deref(), Some("users"));
    }

    #[test]
    fn enums_carry_their_variants_and_type_columns_get_the_constraint() {
        let out = extract(SCHEMA);
        let status = relation(&out, "PaymentStatus");
        assert_eq!(status.kind, RelationKind::EnumType);
        assert_eq!(status.enum_values, vec!["PENDING", "COMPLETED", "FAILED"]);

        let payments = relation(&out, "payments");
        let column = payments.fields.iter().find(|f| f.name == "status").unwrap();
        assert_eq!(column.data_type.as_deref(), Some("PaymentStatus"));
        assert!(
            column
                .constraints
                .iter()
                .any(|c| c == "enum: PENDING|COMPLETED|FAILED"),
            "{:?}",
            column.constraints
        );
    }

    #[test]
    fn mongodb_provider_yields_collections_in_the_mongo_layer() {
        let out = extract(
            "datasource db {\n  provider = \"mongodb\"\n}\n\n\
             model Invoice {\n  id String @id @map(\"_id\")\n}\n",
        );
        let invoice = relation(&out, "Invoice");
        assert_eq!(invoice.kind, RelationKind::Collection);
        assert_eq!(invoice.layer_hint.as_deref(), Some(DEFAULT_MONGO_LAYER));
    }

    #[test]
    fn composite_id_folds_onto_named_columns() {
        let out = extract(
            "model LineItem {\n  orderId Int\n  sku String\n  qty Int\n  @@id([orderId, sku])\n}\n",
        );
        let rel = relation(&out, "LineItem");
        let order_id = rel.fields.iter().find(|f| f.name == "orderId").unwrap();
        assert!(!order_id.nullable);
        assert!(order_id.constraints.contains(&"PRIMARY KEY".to_string()));
        let qty = rel.fields.iter().find(|f| f.name == "qty").unwrap();
        assert!(!qty.constraints.contains(&"PRIMARY KEY".to_string()));
    }

    #[test]
    fn non_schema_blocks_and_broken_lines_contribute_nothing() {
        let out = extract(
            "generator client {\n  provider = \"prisma-client-js\"\n}\n\
             not a block at all {\n}\n",
        );
        assert!(out.relations.is_empty(), "{:?}", out.relations);
        assert!(out.additions.is_empty());
    }
}
