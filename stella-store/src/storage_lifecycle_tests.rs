use super::*;

#[test]
fn on_disk_store_persists_across_reopen() {
    let root = std::env::temp_dir().join(format!("stella_store_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    {
        let store = Store::open(&root).unwrap();
        store
            .begin_execution("run", "hello", "anthropic", "claude-fable-5")
            .unwrap();
    }
    {
        let store = Store::open(&root).unwrap();
        assert_eq!(store.count("executions").unwrap(), 1);
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn open_hardens_a_fresh_dot_stella_dir() {
    let root = std::env::temp_dir().join(format!("stella_harden_{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let _store = Store::open(&root).unwrap();
    let dir = root.join(".stella");

    // Generated artifacts (transcript DBs, WAL siblings, reflections log)
    // must be gitignored so session transcripts can't be committed by
    // accident; user-authored files must NOT be (no bare `*`).
    let gitignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert!(gitignore.contains("*.db"));
    assert!(gitignore.contains("reflections.jsonl"));
    assert!(gitignore.lines().any(|line| line.trim() == "private/"));
    assert!(!gitignore.lines().any(|line| line.trim() == "*"));

    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .unwrap();
    let ignored = std::process::Command::new("git")
        .args(["check-ignore", ".stella/private/mcp_oauth.json"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        ignored.status.success(),
        "private OAuth tokens must never stage"
    );

    // A pre-existing customized ignore keeps its contents and gains the
    // private-state rule exactly once.
    std::fs::write(dir.join(".gitignore"), "custom\n").unwrap();
    drop(Store::open(&root).unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "custom\nprivate/\n"
    );
    drop(Store::open(&root).unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "custom\nprivate/\n"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "fresh .stella must be owner-only");
    }

    std::fs::remove_dir_all(&root).ok();
}
