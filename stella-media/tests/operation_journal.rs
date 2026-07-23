use std::sync::{Arc, Barrier};

use stella_media::{
    MediaOperationClaim, MediaOperationJournal, MediaOperationRetention, MediaOperationState,
    SqliteMediaOperationJournal,
};
use stella_protocol::MediaKind;

fn config(retention_secs: u64, max_rows: u64) -> MediaOperationRetention {
    MediaOperationRetention {
        retention_secs,
        max_rows,
    }
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn journal_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let parent = dir.path().join("host-data");
    std::fs::create_dir(&parent).unwrap();
    #[cfg(unix)]
    set_mode(&parent, 0o700);
    parent.join("operations.db")
}

#[cfg(unix)]
#[test]
fn first_open_creates_private_parent_database_and_live_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let host_root = dir.path().join("host").join("state");
    let path = host_root.join("operations.db");

    let _journal = SqliteMediaOperationJournal::open(&path, config(3600, 100)).unwrap();

    assert_eq!(mode(&dir.path().join("host")), 0o700);
    assert_eq!(mode(&host_root), 0o700);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&path.with_file_name("operations.db-wal")), 0o600);
    assert_eq!(mode(&path.with_file_name("operations.db-shm")), 0o600);
}

#[cfg(unix)]
#[test]
fn open_refuses_a_permissive_existing_parent_without_repairing_it() {
    let dir = tempfile::tempdir().unwrap();
    let host_root = dir.path().join("host");
    std::fs::create_dir(&host_root).unwrap();
    set_mode(&host_root, 0o755);
    let path = host_root.join("operations.db");

    let error = SqliteMediaOperationJournal::open(&path, config(3600, 100))
        .err()
        .expect("permissive parent must fail closed")
        .to_string();

    assert!(error.contains("owner-only"), "{error}");
    assert_eq!(
        mode(&host_root),
        0o755,
        "unsafe legacy state is not repaired"
    );
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn open_refuses_a_permissive_existing_database_without_repairing_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(&dir);
    std::fs::write(&path, b"legacy").unwrap();
    set_mode(&path, 0o644);

    let error = SqliteMediaOperationJournal::open(&path, config(3600, 100))
        .err()
        .expect("permissive database must fail closed")
        .to_string();

    assert!(error.contains("owner-only"), "{error}");
    assert_eq!(mode(&path), 0o644, "unsafe legacy state is not repaired");
    assert_eq!(std::fs::read(&path).unwrap(), b"legacy");
}

#[cfg(unix)]
#[test]
fn open_refuses_a_terminal_database_symlink() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.db");
    let path = journal_path(&dir);
    std::fs::write(&target, b"outside").unwrap();
    set_mode(&target, 0o600);
    symlink(&target, &path).unwrap();

    let error = SqliteMediaOperationJournal::open(&path, config(3600, 100))
        .err()
        .expect("terminal symlink must fail closed")
        .to_string();

    assert!(error.contains("regular file"), "{error}");
    assert_eq!(std::fs::read(target).unwrap(), b"outside");
}

#[cfg(unix)]
#[test]
fn open_refuses_a_hardlinked_database() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.db");
    let path = journal_path(&dir);
    std::fs::write(&target, b"outside").unwrap();
    set_mode(&target, 0o600);
    std::fs::hard_link(&target, &path).unwrap();

    let error = SqliteMediaOperationJournal::open(&path, config(3600, 100))
        .err()
        .expect("hardlinked database must fail closed")
        .to_string();

    assert!(error.contains("single-link"), "{error}");
    assert_eq!(std::fs::read(target).unwrap(), b"outside");
}

#[cfg(unix)]
#[test]
fn open_refuses_unsafe_wal_and_shm_sidecars_without_mutating_them() {
    for suffix in ["-wal", "-shm"] {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(&dir);
        let sidecar = path.with_file_name(format!("operations.db{suffix}"));
        std::fs::write(&sidecar, b"unsafe-sidecar").unwrap();
        set_mode(&sidecar, 0o644);

        let error = SqliteMediaOperationJournal::open(&path, config(3600, 100))
            .err()
            .expect("unsafe sidecar must fail closed")
            .to_string();

        assert!(error.contains("sidecar"), "{error}");
        assert_eq!(mode(&sidecar), 0o644);
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"unsafe-sidecar");
        assert!(!path.exists());
    }
}

#[test]
fn concurrent_first_open_initializes_one_private_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("host").join("operations.db"));
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            SqliteMediaOperationJournal::open(path.as_path(), config(3600, 100))
        }));
    }
    barrier.wait();
    let journals: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap().unwrap())
        .collect();

    let expires_at = unix_now() + 3600;
    assert_eq!(
        journals[0]
            .claim("mop_concurrent_init", MediaKind::Image, "fake", expires_at)
            .unwrap(),
        MediaOperationClaim::New
    );
    assert_eq!(
        journals[1]
            .claim("mop_concurrent_init", MediaKind::Image, "fake", expires_at)
            .unwrap(),
        MediaOperationClaim::Existing(MediaOperationState::Pending)
    );
}

#[test]
fn concurrent_first_open_with_many_racers_all_succeed() {
    const RACERS: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("host").join("operations.db"));
    let barrier = Arc::new(Barrier::new(RACERS + 1));
    let mut workers = Vec::new();
    for _ in 0..RACERS {
        let path = path.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            SqliteMediaOperationJournal::open(path.as_path(), config(3600, 100))
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
}

#[test]
fn concurrent_claims_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(&dir);
    let journals = [
        Arc::new(SqliteMediaOperationJournal::open(&path, config(3600, 100)).unwrap()),
        Arc::new(SqliteMediaOperationJournal::open(&path, config(3600, 100)).unwrap()),
    ];
    let barrier = Arc::new(Barrier::new(3));
    let expires_at = unix_now() + 3600;
    let mut workers = Vec::new();
    for journal in journals {
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            journal.claim("mop_same", MediaKind::Image, "fake", expires_at)
        }));
    }
    barrier.wait();
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == MediaOperationClaim::New)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| {
                **outcome == MediaOperationClaim::Existing(MediaOperationState::Pending)
            })
            .count(),
        1
    );
}

#[test]
fn child_claim_helper() {
    let Some(path) = std::env::var_os("STELLA_TEST_MEDIA_JOURNAL") else {
        return;
    };
    let journal = SqliteMediaOperationJournal::open(path, config(3600, 100)).unwrap();
    assert_eq!(
        journal
            .claim("mop_child", MediaKind::Video, "fake", unix_now() + 3600,)
            .unwrap(),
        MediaOperationClaim::New
    );
}

#[test]
fn child_process_pending_claim_survives_reopen_without_a_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(&dir);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_claim_helper", "--nocapture"])
        .env("STELLA_TEST_MEDIA_JOURNAL", &path)
        .status()
        .unwrap();
    assert!(status.success());

    let journal = SqliteMediaOperationJournal::open(&path, config(3600, 100)).unwrap();
    assert_eq!(
        journal
            .claim("mop_child", MediaKind::Video, "fake", unix_now() + 3600,)
            .unwrap(),
        MediaOperationClaim::Existing(MediaOperationState::Pending)
    );
    assert!(!dir.path().join("operations.db.lock").exists());
}

#[test]
fn expiry_prunes_rows_rejects_old_tokens_and_recovers_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(&dir);
    let journal = SqliteMediaOperationJournal::open(&path, config(10, 2)).unwrap();
    assert_eq!(
        journal
            .claim_at("mop_old", MediaKind::Image, "fake", 110, 100)
            .unwrap(),
        MediaOperationClaim::New
    );
    assert_eq!(
        journal
            .claim_at("mop_old", MediaKind::Image, "fake", 110, 111)
            .unwrap(),
        MediaOperationClaim::Expired
    );
    let too_long = journal
        .claim_at("mop_too_long", MediaKind::Image, "fake", 122, 111)
        .unwrap_err()
        .to_string();
    assert!(too_long.contains("expiry exceeds"), "{too_long}");
    assert_eq!(
        journal
            .claim_at("mop_second", MediaKind::Image, "fake", 121, 111)
            .unwrap(),
        MediaOperationClaim::New
    );
    assert_eq!(
        journal
            .claim_at("mop_third", MediaKind::Image, "fake", 121, 111)
            .unwrap(),
        MediaOperationClaim::New
    );
    let full = journal
        .claim_at("mop_fourth", MediaKind::Image, "fake", 121, 111)
        .unwrap_err()
        .to_string();
    assert!(full.contains("capacity"), "{full}");

    assert_eq!(
        journal
            .claim_at("mop_fresh", MediaKind::Image, "fake", 132, 122)
            .unwrap(),
        MediaOperationClaim::New
    );
    drop(journal);
    let reopened = SqliteMediaOperationJournal::open(&path, config(10, 2)).unwrap();
    assert_eq!(
        reopened
            .claim_at("mop_old", MediaKind::Image, "fake", 110, 1_000)
            .unwrap(),
        MediaOperationClaim::Expired
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    let old_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM media_operations WHERE operation_key = 'mop_old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_rows, 0, "expired payload/state row must be pruned");
}

#[test]
fn retry_cannot_change_the_host_expiry_for_an_existing_identity() {
    let journal = SqliteMediaOperationJournal::open_in_memory(config(3600, 100)).unwrap();
    assert_eq!(
        journal
            .claim_at("mop_stable", MediaKind::Image, "fake", 200, 100)
            .unwrap(),
        MediaOperationClaim::New
    );

    let error = journal
        .claim_at("mop_stable", MediaKind::Image, "fake", 300, 100)
        .unwrap_err()
        .to_string();

    assert!(error.contains("conflicting"), "{error}");
}

#[test]
fn finalize_rejects_a_state_from_the_wrong_media_family() {
    let journal = SqliteMediaOperationJournal::open_in_memory(config(3600, 100)).unwrap();
    journal
        .claim_at("mop_kind", MediaKind::Image, "fake", 200, 100)
        .unwrap();

    let error = journal
        .finalize(
            "mop_kind",
            MediaOperationState::VideoSubmitted {
                provider_job_id: "job-wrong".into(),
            },
        )
        .unwrap_err()
        .to_string();

    assert!(error.contains("media kind"), "{error}");
}

#[test]
fn completed_video_is_replayable_after_reopen_by_provider_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(&dir);
    let journal = SqliteMediaOperationJournal::open(&path, config(3600, 100)).unwrap();
    journal
        .claim_at("mop_video", MediaKind::Video, "fake", 200, 100)
        .unwrap();
    journal
        .finalize(
            "mop_video",
            MediaOperationState::VideoSubmitted {
                provider_job_id: "job-1".into(),
            },
        )
        .unwrap();
    assert!(journal.complete_video("fake", "job-1").unwrap());
    drop(journal);

    let reopened = SqliteMediaOperationJournal::open(&path, config(3600, 100)).unwrap();
    assert_eq!(
        reopened
            .claim_at("mop_video", MediaKind::Video, "fake", 200, 100)
            .unwrap(),
        MediaOperationClaim::Existing(MediaOperationState::VideoCompleted {
            provider_job_id: "job-1".into()
        })
    );
    assert!(!reopened.complete_video("fake", "legacy-job").unwrap());
}

#[test]
fn journal_schema_and_rows_exclude_content_and_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(&dir);
    let journal = SqliteMediaOperationJournal::open(&path, config(3600, 100)).unwrap();
    journal
        .claim("mop_private", MediaKind::Image, "fake", unix_now() + 3600)
        .unwrap();
    journal
        .finalize(
            "mop_private",
            MediaOperationState::ImageCompleted {
                artifact_id: "med_opaque".into(),
            },
        )
        .unwrap();
    drop(journal);

    let connection = rusqlite::Connection::open(&path).unwrap();
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info('media_operations') ORDER BY cid")
        .unwrap();
    let columns: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "operation_key",
            "kind",
            "provider_id",
            "state",
            "handle",
            "created_at",
            "expires_at"
        ]
    );
    let serialized = std::fs::read(path).unwrap();
    let searchable = String::from_utf8_lossy(&serialized);
    for forbidden in ["prompt", "label", "args", "/workspace", ".stella"] {
        assert!(!searchable.contains(forbidden), "stored {forbidden}");
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
