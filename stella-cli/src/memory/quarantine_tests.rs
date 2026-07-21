use super::*;

async fn memory_with_lesson(workspace_root: &Path, lesson: &str) -> SessionMemory {
    let memory = SessionMemory::open(workspace_root, false).expect("session memory");
    memory
        .store
        .upsert(ContextDelta {
            memories: vec![MemoryInput::reflection(lesson, Vec::<String>::new())],
            ..ContextDelta::default()
        })
        .await
        .expect("store recallable lesson");
    memory
}

async fn assert_no_prompt_or_pipeline_frames(memory: &SessionMemory, lesson: &str) {
    let mut diagnostics = Vec::new();
    let reported_frames = memory
        .recalled_frames_reporting(lesson, |message| diagnostics.push(message))
        .await;
    assert!(reported_frames.is_empty());
    assert_eq!(diagnostics.len(), 1, "failure emits one diagnostic");
    assert!(
        diagnostics[0].contains("quarantine") && diagnostics[0].contains("disabled"),
        "diagnostic explains the fail-closed recall decision: {}",
        diagnostics[0]
    );

    let pipeline_frames = ContextRecallPort::recall(memory, lesson).await;
    assert!(
        pipeline_frames.is_empty(),
        "pipeline recall must fail closed when quarantine state is unknown: {pipeline_frames:?}"
    );
    assert!(
        memory.recall_block(lesson).await.is_none(),
        "prompt recall must fail closed when quarantine state is unknown"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_legacy_store_permissions_fail_recall_closed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let lesson = "never surface this memory when quarantine state cannot be verified";
    let memory = memory_with_lesson(dir.path(), lesson).await;
    let dot = dir.path().join(".stella");

    std::fs::write(dot.join("store.db"), b"unsafe legacy store").unwrap();
    std::fs::set_permissions(&dot, std::fs::Permissions::from_mode(0o777)).unwrap();

    assert_no_prompt_or_pipeline_frames(&memory, lesson).await;
}

#[tokio::test]
async fn corrupt_quarantine_schema_fails_recall_closed() {
    let dir = tempfile::tempdir().unwrap();
    let lesson = "never surface this memory after the quarantine query fails";
    let memory = memory_with_lesson(dir.path(), lesson).await;

    drop(stella_store::Store::open(dir.path()).expect("initialize private store"));
    let store_path = dir.path().join(".stella/private/store.db");
    let conn = rusqlite::Connection::open(store_path).unwrap();
    conn.execute("DROP TABLE memory_citations", []).unwrap();
    drop(conn);

    assert_no_prompt_or_pipeline_frames(&memory, lesson).await;
}
