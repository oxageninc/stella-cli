use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use stella_media::{
    CostDecision, ImageRequest, MediaArtifact, MediaCapabilities, MediaError, MediaJob,
    MediaJobStatus, MediaKind, MediaOperationClaim, MediaOperationJournal, MediaOperationRetention,
    MediaProvider, MediaSpendGate, MediaSpendRequest, SqliteMediaOperationJournal, VideoRequest,
};
use stella_tools::media::{
    HostDataIsolation, HostMediaOperation, MediaBackend, MediaOperationIdSource,
};
use stella_tools::{RegistryOptions, ToolRegistry};

struct SameHostOperation {
    expires_at: u64,
}

impl MediaOperationIdSource for SameHostOperation {
    fn operation_id(&self) -> HostMediaOperation {
        HostMediaOperation {
            opaque_id: "host-concurrent-retry".into(),
            expires_at: self.expires_at,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn journal(path: &std::path::Path) -> Arc<dyn MediaOperationJournal> {
    Arc::new(SqliteMediaOperationJournal::open(path, MediaOperationRetention::default()).unwrap())
}

struct CountingGate(AtomicUsize);

#[async_trait]
impl MediaSpendGate for CountingGate {
    async fn authorize(&self, _request: &MediaSpendRequest) -> CostDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        CostDecision::Approve
    }
}

struct SlowImageProvider(AtomicUsize);

#[async_trait]
impl MediaProvider for SlowImageProvider {
    fn id(&self) -> &str {
        "concurrent-test"
    }

    fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            provider_id: self.id().into(),
            image: true,
            image_usd_each: Some(0.01),
            ..Default::default()
        }
    }

    async fn generate_image(&self, request: ImageRequest) -> Result<MediaArtifact, MediaError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(MediaArtifact {
            kind: MediaKind::Image,
            bytes: b"image".to_vec(),
            extension: "png".into(),
            label: request.label,
            model: "concurrent-test".into(),
            cost_usd: 0.01,
        })
    }

    async fn generate_video(&self, _request: VideoRequest) -> Result<MediaJob, MediaError> {
        Err(MediaError::Transport("not under test".into()))
    }

    async fn poll_video(&self, _job: &MediaJob) -> Result<MediaJobStatus, MediaError> {
        Err(MediaError::Transport("not under test".into()))
    }
}

#[tokio::test]
async fn concurrent_same_id_claims_authorize_and_submit_once() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(CountingGate(AtomicUsize::new(0)));
    let provider = Arc::new(SlowImageProvider(AtomicUsize::new(0)));
    let operation_journal = journal(&dir.path().join("host-data/media-operations.db"));
    let registry = ToolRegistry::with_backends_and_options(
        dir.path().to_path_buf(),
        None,
        Some(MediaBackend {
            image: provider.clone(),
            video: None,
        }),
        RegistryOptions {
            media_requires_host_approval: true,
            media_spend_gate: Some(gate.clone()),
            media_operation_ids: Some(Arc::new(SameHostOperation {
                expires_at: unix_now() + 3600,
            })),
            media_operation_journal: Some(operation_journal),
            media_host_data_isolation: Some(HostDataIsolation::ProcessFree),
            ..Default::default()
        },
    );
    let input = serde_json::json!({"prompt": "same"});

    let (first, second) = tokio::join!(
        registry.execute("generate_image", &input),
        registry.execute("generate_image", &input)
    );

    assert_eq!(gate.0.load(Ordering::SeqCst), 1);
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        usize::from(first.is_error()) + usize::from(second.is_error()),
        1
    );
}

#[tokio::test]
async fn different_roots_with_one_host_journal_submit_once() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let host_data = tempfile::tempdir().unwrap();
    let gate = Arc::new(CountingGate(AtomicUsize::new(0)));
    let provider = Arc::new(SlowImageProvider(AtomicUsize::new(0)));
    let operation_journal = journal(&host_data.path().join("media-operations.db"));
    let options = RegistryOptions {
        media_requires_host_approval: true,
        media_spend_gate: Some(gate.clone()),
        media_operation_ids: Some(Arc::new(SameHostOperation {
            expires_at: unix_now() + 3600,
        })),
        media_operation_journal: Some(operation_journal),
        media_host_data_isolation: Some(HostDataIsolation::ProcessFree),
        ..Default::default()
    };
    let backend = || MediaBackend {
        image: provider.clone(),
        video: None,
    };
    let first = ToolRegistry::with_backends_and_options(
        first_root.path().to_path_buf(),
        None,
        Some(backend()),
        options.clone(),
    );
    let second = ToolRegistry::with_backends_and_options(
        second_root.path().to_path_buf(),
        None,
        Some(backend()),
        options,
    );
    let input = serde_json::json!({"prompt": "same"});

    let (first_out, second_out) = tokio::join!(
        first.execute("generate_image", &input),
        second.execute("generate_image", &input)
    );

    assert_eq!(gate.0.load(Ordering::SeqCst), 1);
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        usize::from(first_out.is_error()) + usize::from(second_out.is_error()),
        1
    );
}

#[tokio::test]
async fn workspace_tools_cannot_modify_or_delete_host_journal() {
    let workspace = tempfile::tempdir().unwrap();
    let host_data = tempfile::tempdir().unwrap();
    let journal_path = host_data.path().join("media-operations.db");
    let operation_journal = journal(&journal_path);
    let expires_at = unix_now() + 3600;
    assert_eq!(
        operation_journal
            .claim(
                "host-owned-key",
                MediaKind::Image,
                "concurrent-test",
                expires_at,
            )
            .unwrap(),
        MediaOperationClaim::New
    );
    let registry = ToolRegistry::new(workspace.path().to_path_buf(), RegistryOptions::default());
    let path = journal_path.to_string_lossy();

    let write = registry
        .execute(
            "write_file",
            &serde_json::json!({"path": path, "content": "reset"}),
        )
        .await;
    let delete = registry
        .execute("delete_file", &serde_json::json!({"path": path}))
        .await;

    assert!(
        write.is_error(),
        "workspace write escaped its root: {write:?}"
    );
    assert!(
        delete.is_error(),
        "workspace delete escaped its root: {delete:?}"
    );
    assert!(journal_path.exists());
    assert_eq!(
        operation_journal
            .claim(
                "host-owned-key",
                MediaKind::Image,
                "concurrent-test",
                expires_at,
            )
            .unwrap(),
        MediaOperationClaim::Existing(stella_media::MediaOperationState::Pending)
    );
}

#[tokio::test]
async fn process_free_registry_cannot_spawn_a_journal_deletion() {
    let workspace = tempfile::tempdir().unwrap();
    let host_data = tempfile::tempdir().unwrap();
    let journal_path = host_data.path().join("media-operations.db");
    let operation_journal = journal(&journal_path);
    let expires_at = unix_now() + 3600;
    assert_eq!(
        operation_journal
            .claim(
                "host-process-adversary",
                MediaKind::Image,
                "concurrent-test",
                expires_at,
            )
            .unwrap(),
        MediaOperationClaim::New
    );
    let registry = ToolRegistry::with_backends_and_options(
        workspace.path().to_path_buf(),
        None,
        Some(MediaBackend {
            image: Arc::new(SlowImageProvider(AtomicUsize::new(0))),
            video: None,
        }),
        RegistryOptions {
            media_requires_host_approval: true,
            media_spend_gate: Some(Arc::new(CountingGate(AtomicUsize::new(0)))),
            media_operation_ids: Some(Arc::new(SameHostOperation { expires_at })),
            media_operation_journal: Some(operation_journal.clone()),
            media_host_data_isolation: Some(HostDataIsolation::ProcessFree),
            ..Default::default()
        },
    );
    assert!(
        registry
            .schemas()
            .iter()
            .any(|schema| schema.name == "generate_image"),
        "test requires an approving paid-media registry"
    );
    let attack = registry
        .execute(
            "start_process",
            &serde_json::json!({
                "argv": ["sh", "-c", format!("rm -f -- {}", journal_path.display())]
            }),
        )
        .await;

    assert!(
        attack.is_error(),
        "process tool unexpectedly ran: {attack:?}"
    );
    assert!(journal_path.exists(), "host journal was deleted");
    assert_eq!(
        operation_journal
            .claim(
                "host-process-adversary",
                MediaKind::Image,
                "concurrent-test",
                expires_at,
            )
            .unwrap(),
        MediaOperationClaim::Existing(stella_media::MediaOperationState::Pending)
    );
}
