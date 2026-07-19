//! The SQL DDL adapter (spec §4a source 1): tree-sitter finds the statement
//! structure (proven node kinds: `create_table` / `create_view` /
//! `create_type` / `object_reference` / `column_definition`); deterministic
//! token scans decode each column's type/constraint tail, `ALTER TABLE …
//! ADD COLUMN` statements, and `COMMENT ON` harvesting — tolerant of
//! dialects the grammar does not fully parse.

use tree_sitter::Node;

use crate::parse::Grammars;

use super::{
    DEFAULT_NAMESPACE, FieldAddition, FieldDef, RelationDef, RelationKind, StorageExtract,
    normalize_name,
};

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
        layer_hint: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> StorageExtract {
        let grammars = Grammars::load().expect("grammars compile");
        extract_sql(&grammars, src)
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
}
