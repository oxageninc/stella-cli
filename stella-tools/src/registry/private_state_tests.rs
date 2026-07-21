use super::*;
use std::os::unix::fs::PermissionsExt;

fn legacy_workspace(mode: u32) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let dot = dir.path().join(".stella");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(mode)).unwrap();
    (dir, dot)
}

#[tokio::test]
async fn schema_gate_rejects_write_when_legacy_codegraph_is_unsafe() {
    let (dir, dot) = legacy_workspace(0o777);
    std::fs::write(dot.join("codegraph.db"), b"unsafe legacy graph").unwrap();
    let reg = ToolRegistry::with_issue_backend(dir.path().to_path_buf(), None);
    let result = reg
        .execute(
            "write_file",
            &serde_json::json!({
                "path": "migrations/unsafe.sql",
                "content": "CREATE TABLE audit_records (id INT);\n"
            }),
        )
        .await;
    match result {
        ToolOutput::Error { message } => {
            assert!(
                message.contains("legacy") && message.contains("private"),
                "{message}"
            );
        }
        other => panic!("unsafe graph state must fail the write closed: {other:?}"),
    }
    assert!(!dir.path().join("migrations/unsafe.sql").exists());
    assert!(dot.join("codegraph.db").exists());
}

#[tokio::test]
async fn schema_gate_migrates_safe_legacy_codegraph_before_write() {
    let (dir, dot) = legacy_workspace(0o700);
    let legacy = dot.join("codegraph.db");
    stella_graph::CodeGraph::open(dir.path(), &legacy)
        .unwrap()
        .shutdown();
    let reg = ToolRegistry::with_issue_backend(dir.path().to_path_buf(), None);
    let result = reg
        .execute(
            "write_file",
            &serde_json::json!({
                "path": "migrations/safe.sql",
                "content": "CREATE TABLE audit_records (id INT);\n"
            }),
        )
        .await;
    assert!(
        !result.is_error(),
        "safe legacy graph should migrate: {result:?}"
    );
    assert!(!legacy.exists());
    assert!(dot.join("private/codegraph.db").exists());
    assert!(dir.path().join("migrations/safe.sql").exists());
}
