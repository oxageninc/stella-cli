//! The TypeScript/JavaScript adapters (spec §4a), one tree parse shared by
//! four marker-gated passes:
//!
//! - **Drizzle** — `pgTable` / `sqliteTable` / `mysqlTable` calls (including
//!   `pgSchema("…").table(…)`), `pgEnum` definitions, builder-chain column
//!   decode (`.primaryKey()`, `.notNull()`, `.default(…)`, `.references(…)`).
//! - **TypeORM** — `@Entity()` classes: `@Column`-family decorators,
//!   `@PrimaryGeneratedColumn`, `@ManyToOne`/`@OneToOne`+`@JoinColumn` FKs.
//! - **Mongoose** — `new Schema({…})` objects joined to their
//!   `model("Name", schema)` registration; nested documents emit dotted
//!   field paths (`line_items.sku`, spec §3). Collections land in the
//!   implicit mongo layer.
//! - **DynamoDB** — CDK `new Table(…, { partitionKey: … })` constructs and
//!   SDK `CreateTable` parameter objects (`TableName` +
//!   `AttributeDefinitions`/`KeySchema`). Tables land in the implicit
//!   dynamodb layer.
//!
//! A file with none of the markers costs four substring scans and is never
//! parsed.

use std::collections::HashMap;

use tree_sitter::Node;

use crate::lang::Language;
use crate::parse::Grammars;

use super::{
    DEFAULT_DYNAMO_LAYER, DEFAULT_MONGO_LAYER, DEFAULT_NAMESPACE, FieldDef, RelationDef,
    RelationKind, StorageExtract,
};

pub(super) fn extract(grammars: &Grammars, lang: Language, source: &str) -> StorageExtract {
    let mut out = StorageExtract::default();
    let drizzle = [
        "pgTable(",
        "sqliteTable(",
        "mysqlTable(",
        "pgEnum(",
        "pgSchema(",
    ]
    .iter()
    .any(|m| source.contains(m));
    let typeorm = source.contains("@Entity");
    let mongoose = source.contains("mongoose");
    let dynamo = source.contains("partitionKey") || source.contains("AttributeDefinitions");
    if !drizzle && !typeorm && !mongoose && !dynamo {
        return out;
    }
    let Some(tree) = crate::parse::parse_tree(grammars, lang, source) else {
        return out;
    };
    let root = tree.root_node();
    let src = source.as_bytes();
    if drizzle {
        extract_drizzle(root, src, &mut out);
    }
    if typeorm {
        extract_typeorm(root, src, &mut out);
    }
    if mongoose {
        extract_mongoose(root, src, &mut out);
    }
    if dynamo {
        extract_dynamo(root, src, &mut out);
    }
    out
}

// ---- Drizzle --------------------------------------------------------------

const DRIZZLE_TABLE_FNS: &[&str] = &["pgTable", "sqliteTable", "mysqlTable"];

fn extract_drizzle(root: Node, src: &[u8], out: &mut StorageExtract) {
    // Pre-pass: `pgSchema("billing")` vars, `pgEnum("name", [...])` vars,
    // and table vars (for `.references(() => users.id)` resolution).
    let mut schema_vars: HashMap<String, String> = HashMap::new();
    let mut enum_vars: HashMap<String, (String, Vec<String>)> = HashMap::new();
    let mut table_vars: HashMap<String, String> = HashMap::new();
    for (var, value) in variable_declarators(root, src) {
        let Some((callee, args)) = call_with_args(value, src) else {
            continue;
        };
        let bare = callee.rsplit('.').next().unwrap_or(&callee);
        if bare == "pgSchema" || bare == "mysqlSchema" {
            if let Some(name) = first_string_arg(args, src) {
                schema_vars.insert(var, name);
            }
        } else if bare == "pgEnum" {
            if let Some(name) = first_string_arg(args, src) {
                let values = positional(args)
                    .get(1)
                    .map(|n| string_array(*n, src))
                    .unwrap_or_default();
                enum_vars.insert(var, (name, values));
            }
        } else if drizzle_table_call(&callee, &schema_vars).is_some()
            && let Some(name) = first_string_arg(args, src)
        {
            table_vars.insert(var, name);
        }
    }

    // Emit enums in source order.
    for (var, value) in variable_declarators(root, src) {
        if let Some((name, values)) = enum_vars.get(&var)
            && call_with_args(value, src).is_some()
        {
            out.relations.push(RelationDef {
                name: name.clone(),
                namespace: DEFAULT_NAMESPACE.to_string(),
                kind: RelationKind::EnumType,
                fields: Vec::new(),
                enum_values: values.clone(),
                comment: None,
                layer_hint: None,
                start_line: value.start_position().row as u32 + 1,
                end_line: value.end_position().row as u32 + 1,
            });
        }
    }

    for (_, value) in variable_declarators(root, src) {
        let Some((callee, args)) = call_with_args(value, src) else {
            continue;
        };
        let Some(namespace) = drizzle_table_call(&callee, &schema_vars) else {
            continue;
        };
        let Some(name) = first_string_arg(args, src) else {
            continue;
        };
        let Some(columns) = positional(args).into_iter().find(|n| n.kind() == "object") else {
            continue;
        };
        let mut rel = RelationDef {
            name,
            namespace,
            kind: RelationKind::Table,
            fields: Vec::new(),
            enum_values: Vec::new(),
            comment: None,
            layer_hint: None,
            start_line: value.start_position().row as u32 + 1,
            end_line: value.end_position().row as u32 + 1,
        };
        for (key, column_value, line) in object_pairs(columns, src) {
            if let Some(field) =
                decode_drizzle_column(&key, column_value, line, src, &enum_vars, &table_vars)
            {
                rel.fields.push(field);
            }
        }
        if !rel.fields.is_empty() {
            out.relations.push(rel);
        }
    }
}

/// `pgTable(...)` → implicit namespace; `billing.table(...)` → the
/// `pgSchema` name the variable was declared with.
fn drizzle_table_call(callee: &str, schema_vars: &HashMap<String, String>) -> Option<String> {
    if DRIZZLE_TABLE_FNS.contains(&callee) {
        return Some(DEFAULT_NAMESPACE.to_string());
    }
    let (object, method) = callee.rsplit_once('.')?;
    (method == "table")
        .then(|| schema_vars.get(object).cloned())
        .flatten()
}

/// One drizzle column: builder chain like
/// `serial("id").primaryKey()` / `numeric("amount").notNull().default("0")`.
fn decode_drizzle_column(
    key: &str,
    value: Node,
    line: u32,
    src: &[u8],
    enum_vars: &HashMap<String, (String, Vec<String>)>,
    table_vars: &HashMap<String, String>,
) -> Option<FieldDef> {
    let (base, methods) = unwrap_call_chain(value, src)?;
    let (builder, base_args) = base;
    let bare = builder.rsplit('.').next().unwrap_or(&builder).to_string();

    let mut field = FieldDef {
        name: base_args
            .and_then(|a| first_string_arg(a, src))
            .unwrap_or_else(|| key.to_string()),
        nullable: true, // drizzle columns are nullable unless .notNull()
        line,
        ..FieldDef::default()
    };
    if let Some((enum_name, values)) = enum_vars.get(&bare) {
        field.data_type = Some(enum_name.clone());
        field
            .constraints
            .push(format!("enum: {}", values.join("|")));
    } else {
        field.data_type = Some(bare);
    }

    for (method, args) in methods {
        match method.as_str() {
            "primaryKey" => {
                field.nullable = false;
                field.constraints.push("PRIMARY KEY".into());
            }
            "notNull" => {
                field.nullable = false;
                if !field.constraints.iter().any(|c| c == "NOT NULL") {
                    field.constraints.push("NOT NULL".into());
                }
            }
            "unique" => field.constraints.push("UNIQUE".into()),
            "default" | "defaultNow" | "$defaultFn" | "$default" => {
                field.default_value = Some(match args {
                    Some(a) => positional(a)
                        .first()
                        .and_then(|n| n.utf8_text(src).ok())
                        .unwrap_or("now()")
                        .to_string(),
                    None => "now()".to_string(),
                });
            }
            "references" => {
                if let Some(target) = args
                    .and_then(|a| positional(a).into_iter().next())
                    .and_then(|arrow| arrow_target_object(arrow, src))
                {
                    let table = table_vars
                        .get(&target)
                        .cloned()
                        .unwrap_or(target)
                        .to_lowercase();
                    field.constraints.push(format!("REFERENCES {table}"));
                    field.references = Some(table);
                }
            }
            _ => {}
        }
    }
    Some(field)
}

/// The object a `() => users.id` arrow points at (`users`).
fn arrow_target_object(arrow: Node, src: &[u8]) -> Option<String> {
    if arrow.kind() != "arrow_function" {
        return None;
    }
    let body = arrow.child_by_field_name("body")?;
    let member = if body.kind() == "member_expression" {
        body
    } else {
        find_descendant(body, "member_expression")?
    };
    let object = member.child_by_field_name("object")?;
    (object.kind() == "identifier")
        .then(|| object.utf8_text(src).ok().map(|t| t.to_string()))
        .flatten()
}

// ---- TypeORM --------------------------------------------------------------

const TYPEORM_COLUMN_DECORATORS: &[&str] = &[
    "Column",
    "PrimaryColumn",
    "PrimaryGeneratedColumn",
    "ObjectIdColumn",
    "CreateDateColumn",
    "UpdateDateColumn",
    "DeleteDateColumn",
    "VersionColumn",
];

fn extract_typeorm(root: Node, src: &[u8], out: &mut StorageExtract) {
    // Pre-pass: entity class → table name, for relation-decorator FKs.
    let mut entity_tables: HashMap<String, String> = HashMap::new();
    let mut classes: Vec<Node> = Vec::new();
    collect_kind(root, "class_declaration", &mut classes);
    for class in &classes {
        if let Some((class_name, table, _)) = entity_header(*class, src) {
            entity_tables.insert(class_name, table);
        }
    }

    for class in &classes {
        let Some((_, table, namespace)) = entity_header(*class, src) else {
            continue;
        };
        let Some(body) = class.child_by_field_name("body") else {
            continue;
        };
        let mut rel = RelationDef {
            name: table,
            namespace,
            kind: RelationKind::Table,
            fields: Vec::new(),
            enum_values: Vec::new(),
            comment: None,
            layer_hint: None,
            start_line: class.start_position().row as u32 + 1,
            end_line: class.end_position().row as u32 + 1,
        };
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            if !matches!(
                member.kind(),
                "public_field_definition" | "field_definition"
            ) {
                continue;
            }
            if let Some(field) = decode_typeorm_member(member, src, &entity_tables) {
                rel.fields.push(field);
            }
        }
        if !rel.fields.is_empty() {
            out.relations.push(rel);
        }
    }
}

/// `@Entity("payments", { schema: "billing" })` (or options-only, or bare)
/// → (class name, table name, namespace) when the class is an entity.
fn entity_header(class: Node, src: &[u8]) -> Option<(String, String, String)> {
    let class_name = class
        .child_by_field_name("name")?
        .utf8_text(src)
        .ok()?
        .to_string();
    let decorator = decorators(class)
        .into_iter()
        .find(|d| decorator_name(*d, src).as_deref() == Some("Entity"))?;
    let mut table = class_name.clone();
    let mut namespace = DEFAULT_NAMESPACE.to_string();
    if let Some(args) = decorator_args(decorator) {
        if let Some(name) = first_string_arg(args, src) {
            table = name;
        }
        if let Some(options) = positional(args).into_iter().find(|n| n.kind() == "object") {
            if let Some(name) = object_string_entry(options, src, "name") {
                table = name;
            }
            if let Some(schema) = object_string_entry(options, src, "schema") {
                namespace = schema;
            }
        }
    }
    Some((class_name, table, namespace))
}

fn decode_typeorm_member(
    member: Node,
    src: &[u8],
    entity_tables: &HashMap<String, String>,
) -> Option<FieldDef> {
    let name = member
        .child_by_field_name("name")?
        .utf8_text(src)
        .ok()?
        .to_string();
    let annotation = member
        .child_by_field_name("type")
        .and_then(|t| t.utf8_text(src).ok())
        .map(|t| t.trim_start_matches(':').trim().to_string());
    let decorator_list = decorators(member);
    let named = |wanted: &str| {
        decorator_list
            .iter()
            .find(|d| decorator_name(**d, src).as_deref() == Some(wanted))
            .copied()
    };

    let mut field = FieldDef {
        name,
        data_type: annotation,
        // TypeORM's default is NOT NULL; `nullable: true` opts out.
        nullable: false,
        line: member.start_position().row as u32 + 1,
        ..FieldDef::default()
    };

    if let Some(column) = TYPEORM_COLUMN_DECORATORS.iter().find_map(|d| named(d)) {
        let decorator_kind = decorator_name(column, src).unwrap_or_default();
        if decorator_kind.starts_with("Primary") {
            field.nullable = false;
            field.constraints.push("PRIMARY KEY".into());
        }
        if let Some(args) = decorator_args(column) {
            if let Some(type_name) = first_string_arg(args, src) {
                field.data_type = Some(type_name);
            }
            if let Some(options) = positional(args).into_iter().find(|n| n.kind() == "object") {
                apply_typeorm_options(options, src, &mut field);
            }
        }
        return Some(field);
    }

    // Relation decorators: @ManyToOne always owns the FK column;
    // @OneToOne only when it carries @JoinColumn.
    let relation =
        named("ManyToOne").or_else(|| named("OneToOne").filter(|_| named("JoinColumn").is_some()));
    if let Some(relation) = relation {
        field.nullable = true; // relation columns are nullable unless declared otherwise
        if let Some(target) = decorator_args(relation)
            .and_then(|args| positional(args).into_iter().next())
            .and_then(|arrow| arrow_identifier(arrow, src))
        {
            let table = entity_tables
                .get(&target)
                .cloned()
                .unwrap_or(target)
                .to_lowercase();
            field.constraints.push(format!("REFERENCES {table}"));
            field.references = Some(table);
        }
        if let Some(join) = named("JoinColumn")
            && let Some(args) = decorator_args(join)
            && let Some(options) = positional(args).into_iter().find(|n| n.kind() == "object")
            && let Some(column) = object_string_entry(options, src, "name")
        {
            field.name = column;
        }
        if let Some(relation_args) = decorator_args(relation)
            && let Some(options) = positional(relation_args)
                .into_iter()
                .find(|n| n.kind() == "object")
        {
            apply_typeorm_options(options, src, &mut field);
        }
        return Some(field);
    }
    None
}

fn apply_typeorm_options(options: Node, src: &[u8], field: &mut FieldDef) {
    for (key, value, _) in object_pairs(options, src) {
        let text = value.utf8_text(src).unwrap_or("").to_string();
        match key.as_str() {
            "type" => {
                field.data_type = string_of(value, src).or(Some(text));
            }
            "nullable" => field.nullable = text == "true",
            "unique" if text == "true" => field.constraints.push("UNIQUE".into()),
            "default" => field.default_value = Some(text),
            "name" => {
                if let Some(name) = string_of(value, src) {
                    field.name = name;
                }
            }
            _ => {}
        }
    }
}

/// The identifier a `() => User` arrow returns.
fn arrow_identifier(arrow: Node, src: &[u8]) -> Option<String> {
    if arrow.kind() != "arrow_function" {
        return None;
    }
    let body = arrow.child_by_field_name("body")?;
    (body.kind() == "identifier")
        .then(|| body.utf8_text(src).ok().map(|t| t.to_string()))
        .flatten()
}

// ---- Mongoose -------------------------------------------------------------

fn extract_mongoose(root: Node, src: &[u8], out: &mut StorageExtract) {
    // Pre-pass: schema variables (`const s = new Schema({...})`).
    let mut schema_vars: HashMap<String, Node> = HashMap::new();
    for (var, value) in variable_declarators(root, src) {
        if let Some(object) = mongoose_schema_object(value, src) {
            schema_vars.insert(var, object);
        }
    }

    // `model("Payment", schema)` / `mongoose.model("Payment", s, "charges")`.
    let mut calls: Vec<Node> = Vec::new();
    collect_kind(root, "call_expression", &mut calls);
    for call in calls {
        let Some((callee, args)) = call_with_args(call, src) else {
            continue;
        };
        if callee.rsplit('.').next().unwrap_or(&callee) != "model" {
            continue;
        }
        let positionals = positional(args);
        let Some(model_name) = positionals.first().and_then(|n| string_of(*n, src)) else {
            continue;
        };
        let Some(schema_arg) = positionals.get(1) else {
            continue;
        };
        let schema_object = match schema_arg.kind() {
            "identifier" => schema_arg
                .utf8_text(src)
                .ok()
                .and_then(|name| schema_vars.get(name).copied()),
            _ => mongoose_schema_object(*schema_arg, src),
        };
        let Some(schema_object) = schema_object else {
            continue;
        };
        let collection = positionals
            .get(2)
            .and_then(|n| string_of(*n, src))
            .unwrap_or_else(|| mongoose_collection_name(&model_name));

        let mut rel = RelationDef {
            name: collection,
            namespace: DEFAULT_NAMESPACE.to_string(),
            kind: RelationKind::Collection,
            fields: Vec::new(),
            enum_values: Vec::new(),
            comment: None,
            layer_hint: Some(DEFAULT_MONGO_LAYER.to_string()),
            start_line: schema_object.start_position().row as u32 + 1,
            end_line: schema_object.end_position().row as u32 + 1,
        };
        decode_mongoose_object(schema_object, src, "", &mut rel);
        if !rel.fields.is_empty() {
            out.relations.push(rel);
        }
    }
}

/// The `{…}` argument of a `new Schema({…})` / `new mongoose.Schema({…})`.
fn mongoose_schema_object<'a>(node: Node<'a>, src: &[u8]) -> Option<Node<'a>> {
    if node.kind() != "new_expression" {
        return None;
    }
    let constructor = node.child_by_field_name("constructor")?;
    let callee = constructor.utf8_text(src).ok()?;
    if callee.rsplit('.').next().unwrap_or(callee) != "Schema" {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    positional(args).into_iter().find(|n| n.kind() == "object")
}

/// Mongoose's collection naming: lowercase the model name and pluralize.
fn mongoose_collection_name(model: &str) -> String {
    let lower = model.to_lowercase();
    if let Some(stem) = lower.strip_suffix('y') {
        format!("{stem}ies")
    } else if lower.ends_with('s') {
        format!("{lower}es")
    } else {
        format!("{lower}s")
    }
}

/// Decode one schema object level; nested documents recurse with a dotted
/// prefix (`line_items.sku`).
fn decode_mongoose_object(object: Node, src: &[u8], prefix: &str, rel: &mut RelationDef) {
    for (key, value, line) in object_pairs(object, src) {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        decode_mongoose_value(value, src, &path, line, rel);
    }
}

fn decode_mongoose_value(value: Node, src: &[u8], path: &str, line: u32, rel: &mut RelationDef) {
    match value.kind() {
        "identifier" | "member_expression" => {
            let text = value.utf8_text(src).unwrap_or("");
            rel.fields.push(FieldDef {
                name: path.to_string(),
                data_type: Some(text.rsplit('.').next().unwrap_or(text).to_string()),
                nullable: true,
                line,
                ..FieldDef::default()
            });
        }
        "object" => {
            let has_type = object_pairs(value, src).iter().any(|(k, _, _)| k == "type");
            if has_type {
                let mut field = FieldDef {
                    name: path.to_string(),
                    nullable: true,
                    line,
                    ..FieldDef::default()
                };
                for (key, entry, _) in object_pairs(value, src) {
                    let text = entry.utf8_text(src).unwrap_or("");
                    match key.as_str() {
                        "type" => {
                            field.data_type =
                                Some(text.rsplit('.').next().unwrap_or(text).to_string());
                        }
                        "required" => {
                            // `required: true` or `[true, "message"]`.
                            if text == "true" || text.starts_with("[true") {
                                field.nullable = false;
                            }
                        }
                        "default" => field.default_value = Some(text.to_string()),
                        "unique" if text == "true" => field.constraints.push("UNIQUE".into()),
                        "ref" => {
                            if let Some(model) = string_of(entry, src) {
                                let target = mongoose_collection_name(&model);
                                field.constraints.push(format!("REFERENCES {target}"));
                                field.references = Some(target);
                            }
                        }
                        "enum" => {
                            let values = string_array(entry, src);
                            if !values.is_empty() {
                                field
                                    .constraints
                                    .push(format!("enum: {}", values.join("|")));
                            }
                        }
                        _ => {}
                    }
                }
                rel.fields.push(field);
            } else {
                decode_mongoose_object(value, src, path, rel);
            }
        }
        "array" => {
            let mut cursor = value.walk();
            match value.named_children(&mut cursor).next() {
                Some(element) if element.kind() == "object" => {
                    decode_mongoose_object(element, src, path, rel);
                }
                Some(element) => {
                    let text = element.utf8_text(src).unwrap_or("");
                    rel.fields.push(FieldDef {
                        name: path.to_string(),
                        data_type: Some(format!("{}[]", text.rsplit('.').next().unwrap_or(text))),
                        nullable: true,
                        line,
                        ..FieldDef::default()
                    });
                }
                None => {}
            }
        }
        _ => {}
    }
}

// ---- DynamoDB -------------------------------------------------------------

fn extract_dynamo(root: Node, src: &[u8], out: &mut StorageExtract) {
    // CDK constructs: `new dynamodb.Table(this, "Payments", { partitionKey:
    // {...}, tableName: "payments" })`, plus GSIs added on the variable.
    let mut table_vars: HashMap<String, usize> = HashMap::new();
    let mut news: Vec<Node> = Vec::new();
    collect_kind(root, "new_expression", &mut news);
    for node in &news {
        let Some(rel) = decode_cdk_table(*node, src) else {
            continue;
        };
        out.relations.push(rel);
        // Track the variable it was assigned to (for addGlobalSecondaryIndex).
        if let Some(declarator) = enclosing_declarator(*node)
            && let Some(name) = declarator
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
        {
            table_vars.insert(name.to_string(), out.relations.len() - 1);
        }
    }

    let mut calls: Vec<Node> = Vec::new();
    collect_kind(root, "call_expression", &mut calls);
    for call in &calls {
        let Some((callee, args)) = call_with_args(*call, src) else {
            continue;
        };
        // GSIs: `payments.addGlobalSecondaryIndex({ indexName, partitionKey })`.
        if let Some((object, method)) = callee.rsplit_once('.')
            && method == "addGlobalSecondaryIndex"
            && let Some(&index) = table_vars.get(object)
            && let Some(options) = positional(args).into_iter().find(|n| n.kind() == "object")
        {
            let gsi = object_string_entry(options, src, "indexName").unwrap_or_default();
            let rel = &mut out.relations[index];
            for (key_kind, label) in [("partitionKey", "PARTITION KEY"), ("sortKey", "SORT KEY")] {
                if let Some(key_object) = object_entry(options, src, key_kind) {
                    push_dynamo_key(rel, key_object, src, &format!("GSI {gsi} {label}"), true);
                }
            }
        }
    }

    // SDK parameter objects: `CreateTableCommand({ TableName, AttributeDefinitions,
    // KeySchema })` — the object shape is the signature, whatever wraps it.
    let mut objects: Vec<Node> = Vec::new();
    collect_kind(root, "object", &mut objects);
    for object in objects {
        if let Some(rel) = decode_sdk_create_table(object, src) {
            // The AttributeDefinitions shape can appear once per table; skip
            // an object that redescribes a CDK table already emitted.
            if !out.relations.iter().any(|r| {
                r.layer_hint.as_deref() == Some(DEFAULT_DYNAMO_LAYER) && r.name == rel.name
            }) {
                out.relations.push(rel);
            }
        }
    }
}

fn decode_cdk_table(node: Node, src: &[u8]) -> Option<RelationDef> {
    let constructor = node.child_by_field_name("constructor")?;
    let callee = constructor.utf8_text(src).ok()?;
    let bare = callee.rsplit('.').next().unwrap_or(callee);
    if bare != "Table" && bare != "TableV2" {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    let positionals = positional(args);
    let props = positionals.iter().find(|n| n.kind() == "object")?;
    object_entry(*props, src, "partitionKey")?; // the discriminator: a table needs a partition key

    let name = object_string_entry(*props, src, "tableName")
        .or_else(|| positionals.iter().find_map(|n| string_of(*n, src)))?;
    let mut rel = RelationDef {
        name,
        namespace: DEFAULT_NAMESPACE.to_string(),
        kind: RelationKind::Table,
        fields: Vec::new(),
        enum_values: Vec::new(),
        comment: None,
        layer_hint: Some(DEFAULT_DYNAMO_LAYER.to_string()),
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    };
    for (key_kind, label) in [("partitionKey", "PARTITION KEY"), ("sortKey", "SORT KEY")] {
        if let Some(key_object) = object_entry(*props, src, key_kind) {
            push_dynamo_key(&mut rel, key_object, src, label, false);
        }
    }
    (!rel.fields.is_empty()).then_some(rel)
}

/// `{ name: "pk", type: AttributeType.STRING }` → one key field.
fn push_dynamo_key(rel: &mut RelationDef, key_object: Node, src: &[u8], label: &str, gsi: bool) {
    let Some(name) = object_string_entry(key_object, src, "name") else {
        return;
    };
    let data_type = object_entry(key_object, src, "type")
        .and_then(|t| t.utf8_text(src).ok())
        .map(|t| t.rsplit('.').next().unwrap_or(t).to_string());
    if let Some(existing) = rel.fields.iter_mut().find(|f| f.name == name) {
        existing.constraints.push(label.to_string());
        return;
    }
    rel.fields.push(FieldDef {
        name,
        data_type,
        nullable: gsi, // base-table keys are required on every item
        constraints: vec![label.to_string()],
        line: key_object.start_position().row as u32 + 1,
        ..FieldDef::default()
    });
}

/// An SDK `CreateTable` parameter object: `TableName` plus
/// `AttributeDefinitions` and/or `KeySchema`.
fn decode_sdk_create_table(object: Node, src: &[u8]) -> Option<RelationDef> {
    let name = object_string_entry(object, src, "TableName")?;
    let attributes = object_entry(object, src, "AttributeDefinitions");
    let key_schema = object_entry(object, src, "KeySchema");
    if attributes.is_none() && key_schema.is_none() {
        return None;
    }
    let mut rel = RelationDef {
        name,
        namespace: DEFAULT_NAMESPACE.to_string(),
        kind: RelationKind::Table,
        fields: Vec::new(),
        enum_values: Vec::new(),
        comment: None,
        layer_hint: Some(DEFAULT_DYNAMO_LAYER.to_string()),
        start_line: object.start_position().row as u32 + 1,
        end_line: object.end_position().row as u32 + 1,
    };
    if let Some(attributes) = attributes {
        for element in array_objects(attributes) {
            if let Some(attr_name) = object_string_entry(element, src, "AttributeName") {
                let data_type = object_string_entry(element, src, "AttributeType");
                rel.fields.push(FieldDef {
                    name: attr_name,
                    data_type,
                    nullable: true,
                    line: element.start_position().row as u32 + 1,
                    ..FieldDef::default()
                });
            }
        }
    }
    if let Some(key_schema) = key_schema {
        for element in array_objects(key_schema) {
            let Some(attr_name) = object_string_entry(element, src, "AttributeName") else {
                continue;
            };
            let label = match object_string_entry(element, src, "KeyType").as_deref() {
                Some("HASH") => "PARTITION KEY",
                Some("RANGE") => "SORT KEY",
                _ => continue,
            };
            if let Some(field) = rel.fields.iter_mut().find(|f| f.name == attr_name) {
                field.constraints.push(label.to_string());
                field.nullable = false;
            } else {
                rel.fields.push(FieldDef {
                    name: attr_name,
                    nullable: false,
                    constraints: vec![label.to_string()],
                    line: element.start_position().row as u32 + 1,
                    ..FieldDef::default()
                });
            }
        }
    }
    (!rel.fields.is_empty()).then_some(rel)
}

// ---- Shared TS-tree helpers -----------------------------------------------

/// Every `variable_declarator` (name, value) in the tree, in source order.
fn variable_declarators<'a>(root: Node<'a>, src: &'a [u8]) -> Vec<(String, Node<'a>)> {
    let mut nodes = Vec::new();
    collect_kind(root, "variable_declarator", &mut nodes);
    nodes
        .into_iter()
        .filter_map(|n| {
            let name = n.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            Some((
                name.utf8_text(src).ok()?.to_string(),
                n.child_by_field_name("value")?,
            ))
        })
        .collect()
}

fn collect_kind<'a>(node: Node<'a>, kind: &str, out: &mut Vec<Node<'a>>) {
    if node.kind() == kind {
        out.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_kind(child, kind, out);
    }
}

fn find_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut found = Vec::new();
    collect_kind(node, kind, &mut found);
    found.into_iter().next()
}

/// `f(...)` → (dotted callee text, arguments node).
fn call_with_args<'a>(node: Node<'a>, src: &[u8]) -> Option<(String, Node<'a>)> {
    if node.kind() != "call_expression" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    if !matches!(function.kind(), "identifier" | "member_expression") {
        return None;
    }
    Some((
        function.utf8_text(src).ok()?.to_string(),
        node.child_by_field_name("arguments")?,
    ))
}

/// One call in a builder chain: the callee name and its arguments node.
type ChainCall<'a> = (String, Option<Node<'a>>);

/// Unwrap a builder chain `base("x").m1().m2(args)` into the base call and
/// its method list, outermost-last.
fn unwrap_call_chain<'a>(
    node: Node<'a>,
    src: &[u8],
) -> Option<(ChainCall<'a>, Vec<ChainCall<'a>>)> {
    if node.kind() != "call_expression" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    let args = node.child_by_field_name("arguments");
    match function.kind() {
        "identifier" => Some((
            (function.utf8_text(src).ok()?.to_string(), args),
            Vec::new(),
        )),
        "member_expression" => {
            let object = function.child_by_field_name("object")?;
            let property = function
                .child_by_field_name("property")?
                .utf8_text(src)
                .ok()?
                .to_string();
            if object.kind() == "call_expression" {
                let (base, mut methods) = unwrap_call_chain(object, src)?;
                methods.push((property, args));
                Some((base, methods))
            } else {
                // `t.text("x")` — a namespaced builder, no chain below it.
                let full = function.utf8_text(src).ok()?.to_string();
                Some(((full, args), Vec::new()))
            }
        }
        _ => None,
    }
}

fn positional<'a>(args: Node<'a>) -> Vec<Node<'a>> {
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

fn first_string_arg(args: Node, src: &[u8]) -> Option<String> {
    positional(args).into_iter().find_map(|n| string_of(n, src))
}

/// The literal text of a string node (or substitution-free template).
fn string_of(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "string" | "template_string" => {
            let mut cursor = node.walk();
            let mut out = String::new();
            let mut ok = true;
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "string_fragment" => out.push_str(child.utf8_text(src).ok()?),
                    "template_substitution" => ok = false,
                    _ => {}
                }
            }
            ok.then_some(out)
        }
        _ => None,
    }
}

/// The object elements of an array literal (`[{…}, {…}]`).
fn array_objects<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    if node.kind() != "array" {
        return Vec::new();
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| n.kind() == "object")
        .collect()
}

/// `["a", "b"]` → the string values.
fn string_array(node: Node, src: &[u8]) -> Vec<String> {
    if node.kind() != "array" {
        return Vec::new();
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter_map(|n| string_of(n, src))
        .collect()
}

/// Every `key: value` pair of an object literal, with the pair's line.
fn object_pairs<'a>(object: Node<'a>, src: &[u8]) -> Vec<(String, Node<'a>, u32)> {
    let mut out = Vec::new();
    let mut cursor = object.walk();
    for pair in object.named_children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        let Some(key) = pair.child_by_field_name("key") else {
            continue;
        };
        let key_text = match key.kind() {
            "property_identifier" => key.utf8_text(src).ok().map(|t| t.to_string()),
            "string" => string_of(key, src),
            _ => None,
        };
        if let (Some(key_text), Some(value)) = (key_text, pair.child_by_field_name("value")) {
            out.push((key_text, value, pair.start_position().row as u32 + 1));
        }
    }
    out
}

fn object_entry<'a>(object: Node<'a>, src: &[u8], key: &str) -> Option<Node<'a>> {
    object_pairs(object, src)
        .into_iter()
        .find(|(k, _, _)| k == key)
        .map(|(_, v, _)| v)
}

fn object_string_entry(object: Node, src: &[u8], key: &str) -> Option<String> {
    object_entry(object, src, key).and_then(|v| string_of(v, src))
}

/// Decorators attached to a class or class member (searched on the node and,
/// for exported classes, its wrapping export statement).
fn decorators(node: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            out.push(child);
        }
    }
    if out.is_empty()
        && let Some(parent) = node.parent()
        && parent.kind() == "export_statement"
    {
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            if child.kind() == "decorator" {
                out.push(child);
            }
        }
    }
    out
}

/// `@Entity` / `@Entity(...)` → `Entity`.
fn decorator_name(decorator: Node, src: &[u8]) -> Option<String> {
    let inner = decorator.named_child(0)?;
    let name = match inner.kind() {
        "identifier" => inner.utf8_text(src).ok()?.to_string(),
        "call_expression" => {
            let function = inner.child_by_field_name("function")?;
            let text = function.utf8_text(src).ok()?;
            text.rsplit('.').next().unwrap_or(text).to_string()
        }
        _ => return None,
    };
    Some(name)
}

fn decorator_args(decorator: Node) -> Option<Node> {
    let inner = decorator.named_child(0)?;
    (inner.kind() == "call_expression")
        .then(|| inner.child_by_field_name("arguments"))
        .flatten()
}

/// The `variable_declarator` an expression is (directly) assigned to.
fn enclosing_declarator(node: Node) -> Option<Node> {
    let parent = node.parent()?;
    (parent.kind() == "variable_declarator").then_some(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lang: Language, src: &str) -> StorageExtract {
        let grammars = Grammars::load().expect("grammars compile");
        extract(&grammars, lang, src)
    }

    fn relation<'a>(out: &'a StorageExtract, name: &str) -> &'a RelationDef {
        out.relations
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("relation {name} missing: {:?}", out.relations))
    }

    #[test]
    fn drizzle_pg_table_columns_and_chain_modifiers() {
        let out = run(
            Language::TypeScript,
            r#"
import { pgTable, serial, numeric, integer, text } from "drizzle-orm/pg-core";

export const users = pgTable("users", {
  id: serial("id").primaryKey(),
});

export const payments = pgTable("payments", {
  id: serial("id").primaryKey(),
  amount: numeric("amount", { precision: 10, scale: 2 }).notNull(),
  currency: text("currency").default("'USD'"),
  userId: integer("user_id").references(() => users.id),
});
"#,
        );
        let rel = relation(&out, "payments");
        assert_eq!(rel.kind, RelationKind::Table);
        assert!(
            rel.layer_hint.is_none(),
            "drizzle is relational → sql layer"
        );

        let id = rel.fields.iter().find(|f| f.name == "id").unwrap();
        assert!(!id.nullable);
        assert!(id.constraints.contains(&"PRIMARY KEY".to_string()));
        assert_eq!(id.data_type.as_deref(), Some("serial"));

        let amount = rel.fields.iter().find(|f| f.name == "amount").unwrap();
        assert!(!amount.nullable);

        let currency = rel.fields.iter().find(|f| f.name == "currency").unwrap();
        assert!(currency.nullable);
        assert_eq!(currency.default_value.as_deref(), Some("\"'USD'\""));

        let user_id = rel.fields.iter().find(|f| f.name == "user_id").unwrap();
        assert_eq!(user_id.references.as_deref(), Some("users"));
    }

    #[test]
    fn drizzle_pg_schema_and_enum() {
        let out = run(
            Language::TypeScript,
            r#"
const billing = pgSchema("billing");
export const statusEnum = pgEnum("payment_status", ["pending", "done"]);
export const invoices = billing.table("invoices", {
  id: serial("id").primaryKey(),
  status: statusEnum("status"),
});
"#,
        );
        let invoices = relation(&out, "invoices");
        assert_eq!(invoices.namespace, "billing");
        let status = invoices.fields.iter().find(|f| f.name == "status").unwrap();
        assert_eq!(status.data_type.as_deref(), Some("payment_status"));
        assert!(
            status
                .constraints
                .contains(&"enum: pending|done".to_string())
        );

        let e = relation(&out, "payment_status");
        assert_eq!(e.kind, RelationKind::EnumType);
        assert_eq!(e.enum_values, vec!["pending", "done"]);
    }

    #[test]
    fn typeorm_entity_columns_relations_and_defaults() {
        let out = run(
            Language::TypeScript,
            r#"
@Entity("payments", { schema: "billing" })
export class Payment {
  @PrimaryGeneratedColumn()
  id: number;

  @Column({ type: "numeric", nullable: false, default: 0 })
  amount: number;

  @Column("varchar", { name: "currency_code", nullable: true })
  currency: string;

  @ManyToOne(() => User)
  @JoinColumn({ name: "user_id" })
  user: User;

  helper: string;
}

@Entity()
export class User {
  @PrimaryColumn()
  id: number;
}
"#,
        );
        let rel = relation(&out, "payments");
        assert_eq!(rel.namespace, "billing");

        let id = rel.fields.iter().find(|f| f.name == "id").unwrap();
        assert!(id.constraints.contains(&"PRIMARY KEY".to_string()));

        let amount = rel.fields.iter().find(|f| f.name == "amount").unwrap();
        assert_eq!(amount.data_type.as_deref(), Some("numeric"));
        assert!(!amount.nullable, "TypeORM default is NOT NULL");
        assert_eq!(amount.default_value.as_deref(), Some("0"));

        let currency = rel
            .fields
            .iter()
            .find(|f| f.name == "currency_code")
            .expect("options.name renames the column");
        assert!(currency.nullable);
        assert_eq!(currency.data_type.as_deref(), Some("varchar"));

        let user = rel.fields.iter().find(|f| f.name == "user_id").unwrap();
        assert_eq!(
            user.references.as_deref(),
            Some("user"),
            "bare @Entity() falls back to the class name"
        );

        assert!(
            rel.fields.iter().all(|f| f.name != "helper"),
            "undecorated members are not columns"
        );
    }

    #[test]
    fn mongoose_schema_model_join_nested_paths_and_layer() {
        let out = run(
            Language::JavaScript,
            r#"
const mongoose = require("mongoose");
const { Schema, model } = mongoose;

const invoiceSchema = new Schema({
  number: { type: String, required: true, unique: true },
  customer: { type: Schema.Types.ObjectId, ref: "User" },
  status: { type: String, enum: ["draft", "sent", "paid"] },
  line_items: [{
    sku: String,
    qty: { type: Number, default: 1 },
  }],
  meta: {
    source: String,
  },
});

module.exports = model("Invoice", invoiceSchema);
"#,
        );
        let rel = relation(&out, "invoices");
        assert_eq!(rel.kind, RelationKind::Collection);
        assert_eq!(rel.layer_hint.as_deref(), Some(DEFAULT_MONGO_LAYER));

        let number = rel.fields.iter().find(|f| f.name == "number").unwrap();
        assert!(!number.nullable, "required: true");
        assert!(number.constraints.contains(&"UNIQUE".to_string()));

        let customer = rel.fields.iter().find(|f| f.name == "customer").unwrap();
        assert_eq!(customer.data_type.as_deref(), Some("ObjectId"));
        assert_eq!(customer.references.as_deref(), Some("users"));

        let status = rel.fields.iter().find(|f| f.name == "status").unwrap();
        assert!(
            status
                .constraints
                .iter()
                .any(|c| c == "enum: draft|sent|paid")
        );

        // Nested paths, dotted (spec §3: `invoices/line_items.sku`).
        assert!(rel.fields.iter().any(|f| f.name == "line_items.sku"));
        let qty = rel
            .fields
            .iter()
            .find(|f| f.name == "line_items.qty")
            .unwrap();
        assert_eq!(qty.default_value.as_deref(), Some("1"));
        assert!(rel.fields.iter().any(|f| f.name == "meta.source"));
    }

    #[test]
    fn mongoose_explicit_collection_name_wins() {
        let out = run(
            Language::JavaScript,
            "const mongoose = require('mongoose');\n\
             const s = new mongoose.Schema({ n: Number });\n\
             mongoose.model('Ledger', s, 'ledger_entries');\n",
        );
        assert!(out.relations.iter().any(|r| r.name == "ledger_entries"));
    }

    #[test]
    fn dynamo_cdk_table_with_gsi() {
        let out = run(
            Language::TypeScript,
            r#"
import * as dynamodb from "aws-cdk-lib/aws-dynamodb";

const payments = new dynamodb.Table(this, "PaymentsTable", {
  tableName: "payments",
  partitionKey: { name: "pk", type: dynamodb.AttributeType.STRING },
  sortKey: { name: "sk", type: dynamodb.AttributeType.STRING },
});

payments.addGlobalSecondaryIndex({
  indexName: "byUser",
  partitionKey: { name: "userId", type: dynamodb.AttributeType.STRING },
});
"#,
        );
        let rel = relation(&out, "payments");
        assert_eq!(rel.layer_hint.as_deref(), Some(DEFAULT_DYNAMO_LAYER));

        let pk = rel.fields.iter().find(|f| f.name == "pk").unwrap();
        assert_eq!(pk.data_type.as_deref(), Some("STRING"));
        assert!(pk.constraints.contains(&"PARTITION KEY".to_string()));
        assert!(!pk.nullable);

        let user_id = rel.fields.iter().find(|f| f.name == "userId").unwrap();
        assert!(
            user_id
                .constraints
                .iter()
                .any(|c| c == "GSI byUser PARTITION KEY"),
            "{:?}",
            user_id.constraints
        );
    }

    #[test]
    fn dynamo_sdk_create_table_params() {
        let out = run(
            Language::TypeScript,
            r#"
const command = new CreateTableCommand({
  TableName: "sessions",
  AttributeDefinitions: [
    { AttributeName: "sid", AttributeType: "S" },
    { AttributeName: "expiresAt", AttributeType: "N" },
  ],
  KeySchema: [{ AttributeName: "sid", KeyType: "HASH" }],
});
"#,
        );
        let rel = relation(&out, "sessions");
        let sid = rel.fields.iter().find(|f| f.name == "sid").unwrap();
        assert_eq!(sid.data_type.as_deref(), Some("S"));
        assert!(sid.constraints.contains(&"PARTITION KEY".to_string()));
        assert!(!sid.nullable);
        let expires = rel.fields.iter().find(|f| f.name == "expiresAt").unwrap();
        assert!(expires.nullable, "non-key attributes are optional");
    }

    #[test]
    fn plain_typescript_without_markers_is_never_parsed() {
        let out = run(
            Language::TypeScript,
            "export class PaymentService { charge() { return 1; } }\n",
        );
        assert!(out.is_empty());
    }
}
