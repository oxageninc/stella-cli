//! The Python adapters (spec §4a): Django `models.Model` classes and
//! SQLAlchemy definitions (declarative `__tablename__` classes and core
//! `Table("…", metadata, Column(…))` calls), walked over the Python tree
//! the graph already parses.
//!
//! Extraction is marker-gated: a file without `models.Model` /
//! `__tablename__` / `mapped_column` / `sqlalchemy` text costs one
//! substring scan and is never parsed. Both passes emit relations into the
//! implicit relational layer (`layer_hint: None`) — Django and SQLAlchemy
//! describe the same SQL database their migrations do.

use tree_sitter::Node;

use crate::lang::Language;
use crate::parse::Grammars;

use super::{DEFAULT_NAMESPACE, FieldDef, RelationDef, RelationKind, StorageExtract};

pub(super) fn extract(grammars: &Grammars, path: &str, source: &str) -> StorageExtract {
    let mut out = StorageExtract::default();
    let django = source.contains("models.Model");
    let sqlalchemy = source.contains("__tablename__")
        || source.contains("mapped_column")
        || (source.contains("sqlalchemy") && source.contains("Table("));
    if !django && !sqlalchemy {
        return out;
    }
    let Some(tree) = crate::parse::parse_tree(grammars, Language::Python, source) else {
        return out;
    };
    let src = source.as_bytes();
    let app = django.then(|| app_label(path)).flatten();
    walk(
        tree.root_node(),
        src,
        &Passes { django, sqlalchemy },
        app.as_deref(),
        &mut out,
    );
    out
}

struct Passes {
    django: bool,
    sqlalchemy: bool,
}

fn walk(node: Node, src: &[u8], passes: &Passes, app: Option<&str>, out: &mut StorageExtract) {
    match node.kind() {
        "class_definition" => {
            if passes.sqlalchemy
                && let Some(rel) = decode_sqlalchemy_class(node, src)
            {
                out.relations.push(rel);
            } else if passes.django
                && let Some(rel) = decode_django_class(node, src, app)
            {
                out.relations.push(rel);
            }
        }
        "call" if passes.sqlalchemy => {
            if let Some(rel) = decode_core_table(node, src) {
                out.relations.push(rel);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, passes, app, out);
    }
}

/// The Django app label a models file belongs to, from its path:
/// `billing/models.py` → `billing`, `billing/models/payment.py` → `billing`.
/// Django's default table name is `<app>_<model>`, so without this the
/// derived name would collide with nothing real.
fn app_label(path: &str) -> Option<String> {
    let mut parts = std::path::Path::new(path)
        .components()
        .filter_map(|c| c.as_os_str().to_str());
    let mut previous: Option<&str> = None;
    let mut before_previous: Option<&str> = None;
    for part in parts.by_ref() {
        before_previous = previous;
        previous = Some(part);
    }
    match (before_previous, previous) {
        (Some(app), Some("models.py")) => Some(app.to_string()),
        (Some("models"), Some(_)) => {
            // `<app>/models/<file>.py` — the app is one level further up;
            // re-walk to fetch it (paths are short, this is cheap).
            let parts: Vec<&str> = std::path::Path::new(path)
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect();
            parts.len().checked_sub(3).map(|i| parts[i].to_string())
        }
        _ => None,
    }
}

// ---- Django ---------------------------------------------------------------

fn decode_django_class(node: Node, src: &[u8], app: Option<&str>) -> Option<RelationDef> {
    if !has_django_model_base(node, src) {
        return None;
    }
    let class_name = node
        .child_by_field_name("name")?
        .utf8_text(src)
        .ok()?
        .to_string();
    let meta = meta_class(node, src);
    if meta
        .as_ref()
        .is_some_and(|m| class_assignment_text(*m, src, "abstract") == Some("True".into()))
    {
        return None; // abstract base: contributes fields to subclasses, not a table
    }
    let name = meta
        .as_ref()
        .and_then(|m| class_assignment_string(*m, src, "db_table"))
        .unwrap_or_else(|| django_table_name(app, &class_name));

    let mut rel = RelationDef {
        name,
        namespace: DEFAULT_NAMESPACE.to_string(),
        kind: RelationKind::Table,
        fields: Vec::new(),
        enum_values: Vec::new(),
        comment: docstring(node, src),
        layer_hint: None,
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    };

    let body = node.child_by_field_name("body")?;
    for assignment in assignments(body) {
        let Some((left, right)) = assignment_parts(assignment, src) else {
            continue;
        };
        let Some((func, args)) = call_parts(right, src) else {
            continue;
        };
        let Some(field_kind) = django_field_kind(&func) else {
            continue;
        };
        if field_kind == "ManyToManyField" {
            continue; // materializes as a join table, not a column here
        }
        let mut field = FieldDef {
            name: left,
            data_type: Some(field_kind.to_string()),
            // Django's default is NOT NULL; `null=True` opts out.
            nullable: false,
            line: assignment.start_position().row as u32 + 1,
            ..FieldDef::default()
        };
        if matches!(field_kind, "ForeignKey" | "OneToOneField") {
            field.references = args
                .as_ref()
                .and_then(|a| first_positional(*a, src))
                .and_then(|target| django_fk_target(target, src, app, &rel.name));
            if let Some(target) = &field.references {
                field.constraints.push(format!("REFERENCES {target}"));
            }
        }
        if let Some(args) = args {
            apply_django_kwargs(args, src, &mut field);
        }
        rel.fields.push(field);
    }
    (!rel.fields.is_empty()).then_some(rel)
}

fn has_django_model_base(class_node: Node, src: &[u8]) -> bool {
    let Some(superclasses) = class_node.child_by_field_name("superclasses") else {
        return false;
    };
    let mut cursor = superclasses.walk();
    superclasses.children(&mut cursor).any(|arg| {
        arg.utf8_text(src).is_ok_and(|text| {
            let text = text.trim();
            text == "models.Model" || text == "Model"
        })
    })
}

/// Django's default table naming: `<app>_<model-lowercased>` (no plural).
fn django_table_name(app: Option<&str>, class_name: &str) -> String {
    let model = class_name.to_lowercase();
    match app {
        Some(app) => format!("{app}_{model}"),
        None => model,
    }
}

/// The table a `ForeignKey(target)` points at, applying the same naming
/// rule the target model itself would get: `User` → `<app>_user`,
/// `"billing.Invoice"` → `billing_invoice`, `"self"` → this table.
fn django_fk_target(
    target: Node,
    src: &[u8],
    app: Option<&str>,
    own_table: &str,
) -> Option<String> {
    match target.kind() {
        "string" => {
            let value = string_value(target, src)?;
            if value == "self" {
                return Some(own_table.to_string());
            }
            match value.split_once('.') {
                Some((target_app, model)) => Some(format!(
                    "{}_{}",
                    target_app.to_lowercase(),
                    model.to_lowercase()
                )),
                None => Some(django_table_name(app, &value)),
            }
        }
        "identifier" | "attribute" => {
            let text = target.utf8_text(src).ok()?;
            let bare = text.rsplit('.').next().unwrap_or(text);
            Some(django_table_name(app, bare))
        }
        _ => None,
    }
}

fn django_field_kind(func: &str) -> Option<&str> {
    let bare = func.rsplit('.').next().unwrap_or(func);
    (bare.ends_with("Field") || matches!(bare, "ForeignKey")).then_some(bare)
}

fn apply_django_kwargs(args: Node, src: &[u8], field: &mut FieldDef) {
    for (name, value) in keyword_arguments(args, src) {
        let text = value.utf8_text(src).unwrap_or("").to_string();
        match name.as_str() {
            "null" => field.nullable = text == "True",
            "primary_key" if text == "True" => {
                field.nullable = false;
                field.constraints.push("PRIMARY KEY".into());
            }
            "unique" if text == "True" => field.constraints.push("UNIQUE".into()),
            "default" => field.default_value = Some(text),
            "db_column" => {
                if let Some(column) = string_value(value, src) {
                    field.name = column;
                }
            }
            _ => {}
        }
    }
}

/// The nested `class Meta:` of a Django model, if present.
fn meta_class<'a>(class_node: Node<'a>, src: &[u8]) -> Option<Node<'a>> {
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    body.children(&mut cursor).find(|child| {
        child.kind() == "class_definition"
            && child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
                == Some("Meta")
    })
}

// ---- SQLAlchemy -----------------------------------------------------------

fn decode_sqlalchemy_class(node: Node, src: &[u8]) -> Option<RelationDef> {
    let body = node.child_by_field_name("body")?;
    let name = class_assignment_string(node, src, "__tablename__")?;
    let namespace = table_args_schema(node, src).unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());

    let mut rel = RelationDef {
        name,
        namespace,
        kind: RelationKind::Table,
        fields: Vec::new(),
        enum_values: Vec::new(),
        comment: docstring(node, src),
        layer_hint: None,
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    };

    for assignment in assignments(body) {
        let Some((left, right)) = assignment_parts(assignment, src) else {
            continue;
        };
        if left.starts_with("__") {
            continue;
        }
        let Some((func, args)) = call_parts(right, src) else {
            continue;
        };
        let bare = func.rsplit('.').next().unwrap_or(&func);
        if !matches!(bare, "Column" | "mapped_column") {
            continue;
        }
        let annotation = assignment
            .child_by_field_name("type")
            .and_then(|t| t.utf8_text(src).ok())
            .map(|t| t.to_string());
        let mut field = FieldDef {
            name: left,
            line: assignment.start_position().row as u32 + 1,
            ..FieldDef::default()
        };
        decode_sqlalchemy_column(args, src, annotation.as_deref(), &mut field);
        rel.fields.push(field);
    }
    Some(rel)
}

/// Core style: `Table("orders", metadata, Column("id", Integer), …)`.
fn decode_core_table(node: Node, src: &[u8]) -> Option<RelationDef> {
    let (func, args) = call_parts(node, src)?;
    if func.rsplit('.').next().unwrap_or(&func) != "Table" {
        return None;
    }
    let args = args?;
    let name = string_value(first_positional(args, src)?, src)?;

    let mut rel = RelationDef {
        name,
        namespace: keyword_arguments(args, src)
            .into_iter()
            .find(|(k, _)| k == "schema")
            .and_then(|(_, v)| string_value(v, src))
            .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
        kind: RelationKind::Table,
        fields: Vec::new(),
        enum_values: Vec::new(),
        comment: None,
        layer_hint: None,
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
    };

    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        let Some((func, column_args)) = call_parts(arg, src) else {
            continue;
        };
        if func.rsplit('.').next().unwrap_or(&func) != "Column" {
            continue;
        }
        let Some(column_args) = column_args else {
            continue;
        };
        let Some(name) = first_positional(column_args, src).and_then(|n| string_value(n, src))
        else {
            continue;
        };
        let mut field = FieldDef {
            name,
            line: arg.start_position().row as u32 + 1,
            ..FieldDef::default()
        };
        decode_sqlalchemy_column(Some(column_args), src, None, &mut field);
        rel.fields.push(field);
    }
    // A `Table(...)` call with no Column args is a reflection/lookup, not a
    // definition — emitting it would index every `Table("x", meta)` read.
    (!rel.fields.is_empty()).then_some(rel)
}

/// Shared `Column(...)` / `mapped_column(...)` argument decode: positional
/// type + `ForeignKey("t.c")`, then kwargs, then annotation-derived
/// nullability (SQLAlchemy 2.0 `Mapped[Optional[str]]`).
fn decode_sqlalchemy_column(
    args: Option<Node>,
    src: &[u8],
    annotation: Option<&str>,
    field: &mut FieldDef,
) {
    let mut explicit_nullable: Option<bool> = None;
    let mut primary_key = false;

    if let Some(args) = args {
        let mut cursor = args.walk();
        for arg in args.named_children(&mut cursor) {
            match arg.kind() {
                "keyword_argument" => {}
                "call" => {
                    let Some((func, fk_args)) = call_parts(arg, src) else {
                        continue;
                    };
                    let bare = func.rsplit('.').next().unwrap_or(&func);
                    if bare == "ForeignKey" {
                        if let Some(target) = fk_args
                            .and_then(|a| first_positional(a, src))
                            .and_then(|n| string_value(n, src))
                        {
                            // "users.id" → users; "billing.users.id" → users.
                            let parts: Vec<&str> = target.split('.').collect();
                            if parts.len() >= 2 {
                                let table = parts[parts.len() - 2].to_lowercase();
                                field.constraints.push(format!("REFERENCES {table}"));
                                field.references = Some(table);
                            }
                        }
                    } else if field.data_type.is_none() {
                        field.data_type = arg.utf8_text(src).ok().map(|t| t.to_string());
                    }
                }
                "string" => {} // a positional name (core style), already taken
                _ => {
                    if field.data_type.is_none()
                        && let Ok(text) = arg.utf8_text(src)
                    {
                        field.data_type = Some(text.to_string());
                    }
                }
            }
        }
        for (name, value) in keyword_arguments(args, src) {
            let text = value.utf8_text(src).unwrap_or("").to_string();
            match name.as_str() {
                "primary_key" if text == "True" => {
                    primary_key = true;
                    field.constraints.push("PRIMARY KEY".into());
                }
                "nullable" => explicit_nullable = Some(text == "True"),
                "unique" if text == "True" => field.constraints.push("UNIQUE".into()),
                "default" | "server_default" => field.default_value = Some(text),
                _ => {}
            }
        }
    }

    // Type from the annotation when no positional type was given
    // (`Mapped[str] = mapped_column(primary_key=True)`).
    if field.data_type.is_none()
        && let Some(annotation) = annotation
    {
        let inner = annotation
            .strip_prefix("Mapped[")
            .and_then(|a| a.strip_suffix(']'))
            .unwrap_or(annotation);
        field.data_type = Some(inner.to_string());
    }

    field.nullable = explicit_nullable.unwrap_or_else(|| {
        if primary_key {
            false
        } else if let Some(annotation) = annotation {
            // SQLAlchemy 2.0 derives NOT NULL from a non-Optional Mapped[…].
            annotation.contains("Optional[") || annotation.contains("| None")
        } else {
            true // classic Column default
        }
    });
}

/// `__table_args__ = {"schema": "billing"}` (dict, or dict inside a tuple).
fn table_args_schema(class_node: Node, src: &[u8]) -> Option<String> {
    let body = class_node.child_by_field_name("body")?;
    for assignment in assignments(body) {
        let Some(left) = assignment.child_by_field_name("left") else {
            continue;
        };
        if left.utf8_text(src) != Ok("__table_args__") {
            continue;
        }
        let right = assignment.child_by_field_name("right")?;
        return dict_string_entry(right, src, "schema");
    }
    None
}

fn dict_string_entry(node: Node, src: &[u8], key: &str) -> Option<String> {
    match node.kind() {
        "dictionary" => {
            let mut cursor = node.walk();
            for pair in node.named_children(&mut cursor) {
                if pair.kind() == "pair"
                    && pair
                        .child_by_field_name("key")
                        .and_then(|k| string_value(k, src))
                        .as_deref()
                        == Some(key)
                {
                    return pair
                        .child_by_field_name("value")
                        .and_then(|v| string_value(v, src));
                }
            }
            None
        }
        "tuple" | "expression_list" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find_map(|child| dict_string_entry(child, src, key))
        }
        _ => None,
    }
}

// ---- Shared Python-tree helpers -------------------------------------------

/// Direct assignments of a class/block body (each wrapped in an
/// `expression_statement`).
fn assignments<'a>(body: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut cursor = body.walk();
    for stmt in body.children(&mut cursor) {
        if stmt.kind() == "assignment" {
            out.push(stmt);
        } else if stmt.kind() == "expression_statement" {
            let mut inner = stmt.walk();
            out.extend(
                stmt.children(&mut inner)
                    .filter(|c| c.kind() == "assignment"),
            );
        }
    }
    out
}

fn assignment_parts<'a>(assignment: Node<'a>, src: &[u8]) -> Option<(String, Node<'a>)> {
    let left = assignment.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    let right = assignment.child_by_field_name("right")?;
    Some((left.utf8_text(src).ok()?.to_string(), right))
}

/// A string assigned to `name` in a class body (`db_table = "payments"`).
fn class_assignment_string(class_node: Node, src: &[u8], name: &str) -> Option<String> {
    let body = class_node.child_by_field_name("body")?;
    assignments(body).into_iter().find_map(|assignment| {
        let left = assignment.child_by_field_name("left")?;
        (left.utf8_text(src) == Ok(name))
            .then(|| assignment.child_by_field_name("right"))
            .flatten()
            .and_then(|right| string_value(right, src))
    })
}

/// The raw text assigned to `name` in a class body (`abstract = True`).
fn class_assignment_text(class_node: Node, src: &[u8], name: &str) -> Option<String> {
    let body = class_node.child_by_field_name("body")?;
    assignments(body).into_iter().find_map(|assignment| {
        let left = assignment.child_by_field_name("left")?;
        (left.utf8_text(src) == Ok(name))
            .then(|| assignment.child_by_field_name("right"))
            .flatten()
            .and_then(|right| right.utf8_text(src).ok())
            .map(|t| t.to_string())
    })
}

/// A class's leading docstring, harvested as meaning (spec §4b).
fn docstring(class_node: Node, src: &[u8]) -> Option<String> {
    let body = class_node.child_by_field_name("body")?;
    let first = body.named_child(0)?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let text = string_value(first.named_child(0)?, src)?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn call_parts<'a>(node: Node<'a>, src: &[u8]) -> Option<(String, Option<Node<'a>>)> {
    if node.kind() != "call" {
        return None;
    }
    let func = node
        .child_by_field_name("function")?
        .utf8_text(src)
        .ok()?
        .to_string();
    Some((func, node.child_by_field_name("arguments")))
}

fn first_positional<'a>(args: Node<'a>, _src: &[u8]) -> Option<Node<'a>> {
    let mut cursor = args.walk();
    args.named_children(&mut cursor)
        .find(|a| a.kind() != "keyword_argument" && a.kind() != "comment")
}

fn keyword_arguments<'a>(args: Node<'a>, src: &[u8]) -> Vec<(String, Node<'a>)> {
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for arg in args.named_children(&mut cursor) {
        if arg.kind() == "keyword_argument"
            && let (Some(name), Some(value)) = (
                arg.child_by_field_name("name")
                    .and_then(|n| n.utf8_text(src).ok()),
                arg.child_by_field_name("value"),
            )
        {
            out.push((name.to_string(), value));
        }
    }
    out
}

/// The inner text of a Python string literal node.
fn string_value(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    let mut out = String::new();
    let mut saw_content = false;
    for child in node.children(&mut cursor) {
        if child.kind() == "string_content" {
            saw_content = true;
            out.push_str(child.utf8_text(src).ok()?);
        }
    }
    saw_content.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str, src: &str) -> StorageExtract {
        let grammars = Grammars::load().expect("grammars compile");
        extract(&grammars, path, src)
    }

    fn relation<'a>(out: &'a StorageExtract, name: &str) -> &'a RelationDef {
        out.relations
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("relation {name} missing: {:?}", out.relations))
    }

    #[test]
    fn django_model_emits_fields_with_django_naming() {
        let out = run(
            "billing/models.py",
            r#"
from django.db import models

class Payment(models.Model):
    """One row per charge attempt."""
    amount = models.DecimalField(max_digits=10, decimal_places=2)
    currency = models.CharField(max_length=3, default="USD")
    note = models.TextField(null=True)
    user = models.ForeignKey(User, on_delete=models.CASCADE, db_column="user_id")
    tags = models.ManyToManyField(Tag)
"#,
        );
        let rel = relation(&out, "billing_payment");
        assert_eq!(rel.comment.as_deref(), Some("One row per charge attempt."));

        let amount = rel.fields.iter().find(|f| f.name == "amount").unwrap();
        assert_eq!(amount.data_type.as_deref(), Some("DecimalField"));
        assert!(!amount.nullable, "Django default is NOT NULL");

        let currency = rel.fields.iter().find(|f| f.name == "currency").unwrap();
        assert_eq!(currency.default_value.as_deref(), Some("\"USD\""));

        let note = rel.fields.iter().find(|f| f.name == "note").unwrap();
        assert!(note.nullable, "null=True opts into NULL");

        let user = rel.fields.iter().find(|f| f.name == "user_id").unwrap();
        assert_eq!(
            user.references.as_deref(),
            Some("billing_user"),
            "FK target gets the same app_model naming"
        );

        assert!(
            rel.fields.iter().all(|f| f.name != "tags"),
            "M2M is a join table, not a column"
        );
    }

    #[test]
    fn django_db_table_and_abstract_meta_are_honored() {
        let out = run(
            "billing/models.py",
            r#"
from django.db import models

class Payment(models.Model):
    amount = models.DecimalField()
    class Meta:
        db_table = "payments"

class BaseAudit(models.Model):
    created = models.DateTimeField()
    class Meta:
        abstract = True
"#,
        );
        assert!(out.relations.iter().any(|r| r.name == "payments"));
        assert!(
            out.relations.iter().all(|r| !r.name.contains("audit")),
            "abstract models are not tables: {:?}",
            out.relations
        );
    }

    #[test]
    fn sqlalchemy_declarative_class_with_tablename() {
        let out = run(
            "app/models.py",
            r#"
from sqlalchemy.orm import declarative_base
Base = declarative_base()

class Order(Base):
    __tablename__ = "orders"
    __table_args__ = {"schema": "billing"}
    id = Column(Integer, primary_key=True)
    amount = Column(Numeric(10, 2), nullable=False)
    user_id = Column(Integer, ForeignKey("users.id"))
    status = Column(String(20), default="new")
"#,
        );
        let rel = relation(&out, "orders");
        assert_eq!(rel.namespace, "billing");

        let id = rel.fields.iter().find(|f| f.name == "id").unwrap();
        assert!(!id.nullable);
        assert!(id.constraints.contains(&"PRIMARY KEY".to_string()));
        assert_eq!(id.data_type.as_deref(), Some("Integer"));

        let amount = rel.fields.iter().find(|f| f.name == "amount").unwrap();
        assert_eq!(amount.data_type.as_deref(), Some("Numeric(10, 2)"));
        assert!(!amount.nullable);

        let user_id = rel.fields.iter().find(|f| f.name == "user_id").unwrap();
        assert_eq!(user_id.references.as_deref(), Some("users"));

        let status = rel.fields.iter().find(|f| f.name == "status").unwrap();
        assert!(status.nullable, "classic Column defaults to nullable");
        assert_eq!(status.default_value.as_deref(), Some("\"new\""));
    }

    #[test]
    fn sqlalchemy_two_point_oh_mapped_columns_derive_nullability() {
        let out = run(
            "app/models.py",
            r#"
class User(Base):
    __tablename__ = "users"
    id: Mapped[int] = mapped_column(primary_key=True)
    email: Mapped[str] = mapped_column(String(120), unique=True)
    nickname: Mapped[Optional[str]] = mapped_column(String(40))
"#,
        );
        let rel = relation(&out, "users");
        let email = rel.fields.iter().find(|f| f.name == "email").unwrap();
        assert!(!email.nullable, "non-Optional Mapped[…] is NOT NULL");
        assert!(email.constraints.contains(&"UNIQUE".to_string()));
        let nickname = rel.fields.iter().find(|f| f.name == "nickname").unwrap();
        assert!(nickname.nullable, "Optional[…] is nullable");
        let id = rel.fields.iter().find(|f| f.name == "id").unwrap();
        assert_eq!(
            id.data_type.as_deref(),
            Some("int"),
            "type falls back to the Mapped[…] annotation"
        );
    }

    #[test]
    fn sqlalchemy_core_table_call() {
        let out = run(
            "app/tables.py",
            r#"
from sqlalchemy import Table, Column, Integer, String, MetaData

metadata = MetaData()
orders = Table(
    "orders", metadata,
    Column("id", Integer, primary_key=True),
    Column("sku", String(64), nullable=False),
    schema="billing",
)
lookup = Table("existing", metadata)
"#,
        );
        let rel = relation(&out, "orders");
        assert_eq!(rel.namespace, "billing");
        assert_eq!(rel.fields.len(), 2);
        assert!(
            out.relations.iter().all(|r| r.name != "existing"),
            "column-less Table() calls are lookups, not definitions"
        );
    }

    #[test]
    fn plain_python_without_markers_is_never_parsed() {
        let out = run(
            "app/service.py",
            "class PaymentService:\n    def charge(self):\n        pass\n",
        );
        assert!(out.is_empty());
    }
}
