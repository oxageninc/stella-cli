//! Schema conflict gate — prevents the agent from creating duplicate tables,
//! columns, or types during long-horizon tasks.
//!
//! This is the "zero schema drift" mechanism: when `write_file` or `edit_file`
//! targets a `.sql` file, the proposed content is parsed for DDL objects
//! (CREATE TABLE / TYPE / VIEW), and each object name is checked against the
//! known schema index. If a conflict is found, the tool returns a structured
//! error explaining what already exists and where.
//!
//! The gate is deterministic (tree-sitter parse + name lookup), not
//! LLM-dependent. It runs *before* the write lands — the same pattern as
//! `verify_done`, applied to schema.

use std::collections::HashSet;

/// One DDL object extracted from proposed SQL content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlObject {
    pub name: String,
    pub kind: DdlKind,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlKind {
    Table,
    Type,
    View,
}

impl DdlKind {
    pub fn label(self) -> &'static str {
        match self {
            DdlKind::Table => "Table",
            DdlKind::Type => "Type",
            DdlKind::View => "View",
        }
    }
}

/// A conflict found by the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaConflict {
    pub kind: DdlKind,
    pub name: String,
    pub proposed_line: u32,
}

/// Parse SQL content for DDL object names (CREATE TABLE / TYPE / VIEW).
/// Returns every object the content would create.
///
/// This uses a lightweight regex scan rather than tree-sitter to avoid pulling
/// the grammar into stella-tools (which has no tree-sitter dependency today).
/// The parse is intentionally conservative: it matches `CREATE [TABLE|TYPE|VIEW]`
/// case-insensitively, followed by `IF NOT EXISTS` (skipped) and the object name.
pub fn extract_ddl_objects(sql: &str) -> Vec<DdlObject> {
    let mut out = Vec::new();
    for (i, line) in sql.lines().enumerate() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if !lower.starts_with("create") {
            continue;
        }
        let (kind, rest) = if lower.strip_prefix("create table ").is_some() {
            (DdlKind::Table, &trimmed[13..])
        } else if lower.strip_prefix("create type ").is_some() {
            (DdlKind::Type, &trimmed[12..])
        } else if lower.strip_prefix("create view ").is_some() {
            (DdlKind::View, &trimmed[12..])
        } else if lower.strip_prefix("create or replace view ").is_some() {
            (DdlKind::View, &trimmed[23..])
        } else {
            continue;
        };

        let name = parse_identifier(rest);
        if name.is_empty() {
            continue;
        }
        out.push(DdlObject {
            name,
            kind,
            line: i as u32 + 1,
        });
    }
    out
}

/// Extract the first identifier from the remainder of a CREATE statement,
/// skipping optional `IF NOT EXISTS` and schema qualifiers.
fn parse_identifier(rest: &str) -> String {
    let mut tokens = rest.split_whitespace();
    let first = match tokens.next() {
        Some(t) => t,
        None => return String::new(),
    };
    let name = if first.eq_ignore_ascii_case("if") {
        match tokens.next() {
            Some(t) if t.eq_ignore_ascii_case("not") => match tokens.next() {
                Some(t) if t.eq_ignore_ascii_case("exists") => tokens.next().unwrap_or(""),
                _ => "",
            },
            _ => "",
        }
    } else {
        first
    };
    name.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Check proposed DDL objects against a set of known names. Returns any
/// conflicts (objects that already exist by name + kind).
pub fn find_conflicts(
    proposed: &[DdlObject],
    known_tables: &HashSet<String>,
    known_types: &HashSet<String>,
    known_views: &HashSet<String>,
) -> Vec<SchemaConflict> {
    proposed
        .iter()
        .filter(|obj| {
            let known = match obj.kind {
                DdlKind::Table => known_tables,
                DdlKind::Type => known_types,
                DdlKind::View => known_views,
            };
            known.contains(&obj.name.to_lowercase())
        })
        .map(|obj| SchemaConflict {
            kind: obj.kind,
            name: obj.name.clone(),
            proposed_line: obj.line,
        })
        .collect()
}

/// Format conflicts into a human-readable error message for the tool output.
pub fn format_conflicts(conflicts: &[SchemaConflict]) -> String {
    let mut out = String::from("Schema conflict detected before write:\n\n");
    for c in conflicts {
        out.push_str(&format!(
            "  CONFLICT: {} `{}` already exists (proposed at line {})\n",
            c.kind.label(),
            c.name,
            c.proposed_line
        ));
    }
    out.push_str("\nDid you mean to ALTER the existing object instead? Or use a different name?");
    out
}

/// Whether a file path looks like a SQL schema file worth gating.
pub fn is_schema_file(path: &str) -> bool {
    path.ends_with(".sql")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_create_table_and_type() {
        let sql = "\
CREATE TABLE users (id SERIAL PRIMARY KEY);
CREATE TABLE payments (id SERIAL PRIMARY KEY);
CREATE TYPE status AS ENUM ('ok', 'fail');
CREATE VIEW active AS SELECT * FROM users;
";
        let objects = extract_ddl_objects(sql);
        assert_eq!(objects.len(), 4);
        assert_eq!(objects[0].name, "users");
        assert_eq!(objects[0].kind, DdlKind::Table);
        assert_eq!(objects[1].name, "payments");
        assert_eq!(objects[2].name, "status");
        assert_eq!(objects[2].kind, DdlKind::Type);
        assert_eq!(objects[3].name, "active");
        assert_eq!(objects[3].kind, DdlKind::View);
    }

    #[test]
    fn extract_with_if_not_exists() {
        let sql = "CREATE TABLE IF NOT EXISTS orders (id INT)";
        let objects = extract_ddl_objects(sql);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].name, "orders");
    }

    #[test]
    fn extract_with_schema_prefix() {
        let sql = "CREATE TABLE public.users (id INT)";
        let objects = extract_ddl_objects(sql);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].name, "users");
    }

    #[test]
    fn find_conflict_against_known_names() {
        let sql = "CREATE TABLE users (id INT)";
        let proposed = extract_ddl_objects(sql);
        let mut known = HashSet::new();
        known.insert("users".to_string());
        let conflicts = find_conflicts(&proposed, &known, &HashSet::new(), &HashSet::new());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].name, "users");
    }

    #[test]
    fn no_conflict_when_table_is_new() {
        let sql = "CREATE TABLE orders (id INT)";
        let proposed = extract_ddl_objects(sql);
        let mut known = HashSet::new();
        known.insert("users".to_string());
        let conflicts = find_conflicts(&proposed, &known, &HashSet::new(), &HashSet::new());
        assert!(conflicts.is_empty());
    }

    #[test]
    fn format_produces_readable_error() {
        let conflicts = vec![SchemaConflict {
            kind: DdlKind::Table,
            name: "users".into(),
            proposed_line: 3,
        }];
        let msg = format_conflicts(&conflicts);
        assert!(msg.contains("Table `users` already exists"));
        assert!(msg.contains("ALTER"));
    }

    #[test]
    fn case_insensitive_conflict_match() {
        let sql = "CREATE TABLE USERS (id INT)";
        let proposed = extract_ddl_objects(sql);
        let mut known = HashSet::new();
        known.insert("users".to_string());
        let conflicts = find_conflicts(&proposed, &known, &HashSet::new(), &HashSet::new());
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn is_schema_file_detects_sql() {
        assert!(is_schema_file("migrations/001.sql"));
        assert!(is_schema_file("schema.sql"));
        assert!(!is_schema_file("main.rs"));
        assert!(!is_schema_file("README.md"));
    }
}
