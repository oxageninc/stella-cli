//! Persisted video-job state so a dollar-cost job is never orphaned by a
//! dropped terminal. Submitted job handles are written
//! to `jobs.json` inside the artifacts dir; after a Ctrl-C or a process
//! restart, `stella gen video --resume <id>` reattaches.
//!
//! The truthfulness contract (L-V3): a persisted handle is **never** reported
//! from cache. [`resume`] loads the handle and reconciles it *live* against
//! the provider — a job the provider says is gone comes back as
//! `MediaJobState::Failed`, not a stale "running". The adapters implement the
//! gone-detection (a 404 on the poll endpoint → `Failed`); this module owns
//! the persistence and the load-then-poll flow.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::MediaError;
use crate::provider::{MediaJob, MediaJobStatus, MediaProvider};

const JOBS_NAME: &str = "jobs.json";

/// A small JSON-backed store of in-flight video jobs, rooted at the artifacts
/// directory. Reads and writes the whole file (a handful of jobs, not a
/// database); writes are crash-atomic via temp-write-then-rename.
#[derive(Debug, Clone)]
pub struct JobStore {
    path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JobsFile {
    #[serde(default)]
    jobs: Vec<PersistedMediaJob>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedMediaJob {
    artifact_id: String,
    provider_id: String,
    provider_job_id: String,
    kind: stella_protocol::MediaKind,
    model: String,
    estimated_cost_usd: f64,
    submitted_at: u64,
}

impl From<&MediaJob> for PersistedMediaJob {
    fn from(job: &MediaJob) -> Self {
        Self {
            artifact_id: job.artifact_id.clone(),
            provider_id: job.provider_id.clone(),
            provider_job_id: job.provider_job_id.clone(),
            kind: job.kind,
            model: job.model.clone(),
            estimated_cost_usd: job.estimated_cost_usd,
            submitted_at: job.submitted_at,
        }
    }
}

impl From<PersistedMediaJob> for MediaJob {
    fn from(job: PersistedMediaJob) -> Self {
        Self {
            label: job.artifact_id.clone(),
            artifact_id: job.artifact_id,
            provider_id: job.provider_id,
            provider_job_id: job.provider_job_id,
            kind: job.kind,
            model: job.model,
            estimated_cost_usd: job.estimated_cost_usd,
            submitted_at: job.submitted_at,
        }
    }
}

impl JobStore {
    /// Open a job store whose `jobs.json` lives inside `artifacts_root`. The
    /// root is not created here — the [`crate::artifact::ArtifactStore`] owns
    /// that; a missing file simply reads as an empty list.
    pub fn open(artifacts_root: impl AsRef<Path>) -> Self {
        Self {
            path: artifacts_root.as_ref().join(JOBS_NAME),
        }
    }

    /// Record (or replace, keyed by `provider_job_id`) a submitted job.
    pub fn record(&self, job: &MediaJob) -> Result<(), MediaError> {
        let _lock = self.mutation_lock()?;
        let mut file = self.load()?;
        if let Some(existing) = file
            .jobs
            .iter_mut()
            .find(|j| j.provider_job_id == job.provider_job_id)
        {
            *existing = job.into();
        } else {
            file.jobs.push(job.into());
        }
        self.store(&file)
    }

    /// Look up a persisted job by the provider's job id.
    pub fn get(&self, provider_job_id: &str) -> Result<Option<MediaJob>, MediaError> {
        let file = self.load()?;
        Ok(file
            .jobs
            .into_iter()
            .find(|j| j.provider_job_id == provider_job_id)
            .map(Into::into))
    }

    /// All persisted in-flight jobs.
    pub fn list(&self) -> Result<Vec<MediaJob>, MediaError> {
        Ok(self.load()?.jobs.into_iter().map(Into::into).collect())
    }

    /// Forget a job (call after it reaches a terminal state and any artifact
    /// has been persisted).
    pub fn remove(&self, provider_job_id: &str) -> Result<(), MediaError> {
        let _lock = self.mutation_lock()?;
        let mut file = self.load()?;
        let before = file.jobs.len();
        file.jobs.retain(|j| j.provider_job_id != provider_job_id);
        if file.jobs.len() != before {
            self.store(&file)?;
        }
        Ok(())
    }

    fn load(&self) -> Result<JobsFile, MediaError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(JobsFile::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                MediaError::Artifact(format!("job store {} is corrupt: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(JobsFile::default()),
            Err(e) => Err(MediaError::Artifact(format!(
                "cannot read job store {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn store(&self, file: &JobsFile) -> Result<(), MediaError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MediaError::Artifact(format!("cannot create job store dir: {e}")))?;
        }
        let body = serde_json::to_string_pretty(file)
            .map_err(|e| MediaError::Artifact(format!("cannot serialize job store: {e}")))?;
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let tmp = self.path.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, body).map_err(|e| {
            MediaError::Artifact(format!(
                "cannot write temp job store {}: {e}",
                tmp.display()
            ))
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            MediaError::Artifact(format!(
                "cannot commit job store {}: {e}",
                self.path.display()
            ))
        })?;
        Ok(())
    }

    fn mutation_lock(&self) -> Result<std::fs::File, MediaError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                MediaError::Artifact(format!("cannot create job store dir: {error}"))
            })?;
        }
        let path = self.path.with_extension("json.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                MediaError::Artifact(format!(
                    "cannot open job store lock {}: {error}",
                    path.display()
                ))
            })?;
        file.lock().map_err(|error| {
            MediaError::Artifact(format!("cannot lock job store {}: {error}", path.display()))
        })?;
        Ok(file)
    }
}

/// Reconcile a persisted job **live** against its provider (L-V3). This is
/// the only sanctioned way to report a resumed job's state: it polls the
/// provider rather than trusting the persisted handle, so a job the provider
/// has dropped is reported gone (`MediaJobState::Failed`), never as a cached
/// "running". Returns [`MediaError::Terminal`] if no persisted job matches
/// `provider_job_id`.
pub async fn resume(
    store: &JobStore,
    provider: &dyn MediaProvider,
    provider_job_id: &str,
) -> Result<MediaJobStatus, MediaError> {
    let job = store.get(provider_job_id)?.ok_or_else(|| {
        MediaError::Terminal(format!("no persisted job with id `{provider_job_id}`"))
    })?;
    provider.poll_video(&job).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ImageRequest, MediaArtifact, MediaCapabilities, VideoRequest};
    use async_trait::async_trait;
    use stella_protocol::{MediaJobState, MediaKind};
    use tempfile::TempDir;

    fn sample_job(provider_job_id: &str) -> MediaJob {
        MediaJob {
            artifact_id: "med_abc".into(),
            provider_id: "zai".into(),
            provider_job_id: provider_job_id.into(),
            kind: MediaKind::Video,
            model: "cogvideox".into(),
            estimated_cost_usd: 2.0,
            submitted_at: 1_700_000_000,
            label: "teaser".into(),
        }
    }

    #[test]
    fn record_get_list_remove_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = JobStore::open(dir.path());
        assert!(store.list().unwrap().is_empty());

        store.record(&sample_job("job-1")).unwrap();
        store.record(&sample_job("job-2")).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(
            store.get("job-1").unwrap().unwrap().provider_job_id,
            "job-1"
        );
        assert!(store.get("missing").unwrap().is_none());

        store.remove("job-1").unwrap();
        assert!(store.get("job-1").unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn record_replaces_by_provider_job_id() {
        let dir = TempDir::new().unwrap();
        let store = JobStore::open(dir.path());
        store.record(&sample_job("job-1")).unwrap();
        let mut updated = sample_job("job-1");
        updated.estimated_cost_usd = 9.99;
        store.record(&updated).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store.get("job-1").unwrap().unwrap().estimated_cost_usd,
            9.99
        );
    }

    #[test]
    fn concurrent_distinct_records_and_removes_preserve_every_update() {
        let dir = TempDir::new().unwrap();
        let store = JobStore::open(dir.path());
        for id in ["remove-a", "remove-b"] {
            store.record(&sample_job(id)).unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(5));
        let mut workers = Vec::new();
        for (record, remove) in [
            ("record-a", "remove-a"),
            ("record-b", "remove-b"),
            ("record-c", "missing-a"),
            ("record-d", "missing-b"),
        ] {
            let store = store.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store.record(&sample_job(record)).unwrap();
                store.remove(remove).unwrap();
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        let mut ids: Vec<_> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|job| job.provider_job_id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["record-a", "record-b", "record-c", "record-d"]);
    }

    #[test]
    fn survives_a_reopen_persistence_across_process_restart() {
        let dir = TempDir::new().unwrap();
        {
            let store = JobStore::open(dir.path());
            store.record(&sample_job("job-1")).unwrap();
        }
        // A fresh handle (simulating a new process) sees the persisted job.
        let reopened = JobStore::open(dir.path());
        assert_eq!(
            reopened.get("job-1").unwrap().unwrap().provider_job_id,
            "job-1"
        );
        let journal = std::fs::read_to_string(dir.path().join(JOBS_NAME)).unwrap();
        assert!(!journal.contains("\"label\""));
        assert!(!journal.contains("teaser"));
    }

    #[test]
    fn corrupt_job_file_is_a_named_error() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(JOBS_NAME), "{ not valid").unwrap();
        let store = JobStore::open(dir.path());
        assert!(matches!(store.list(), Err(MediaError::Artifact(_))));
    }

    // A fake provider that reports whatever state it's told — proves `resume`
    // reconciles against the provider rather than the persisted handle.
    struct FakeProvider {
        state: MediaJobState,
    }

    #[async_trait]
    impl MediaProvider for FakeProvider {
        fn id(&self) -> &str {
            "zai"
        }
        fn capabilities(&self) -> MediaCapabilities {
            MediaCapabilities::default()
        }
        async fn generate_image(&self, _req: ImageRequest) -> Result<MediaArtifact, MediaError> {
            Err(MediaError::Terminal("not used".into()))
        }
        async fn generate_video(&self, _req: VideoRequest) -> Result<MediaJob, MediaError> {
            Err(MediaError::Terminal("not used".into()))
        }
        async fn poll_video(&self, _job: &MediaJob) -> Result<MediaJobStatus, MediaError> {
            Ok(MediaJobStatus {
                state: self.state.clone(),
                progress: None,
                artifact: None,
            })
        }
    }

    #[tokio::test]
    async fn resume_reports_the_live_provider_state_not_the_cache() {
        let dir = TempDir::new().unwrap();
        let store = JobStore::open(dir.path());
        store.record(&sample_job("job-1")).unwrap();

        // The provider says the job is gone; resume must surface that,
        // not the persisted "in-flight" handle (L-V3).
        let provider = FakeProvider {
            state: MediaJobState::Failed {
                reason: "job not found".into(),
            },
        };
        let status = resume(&store, &provider, "job-1").await.unwrap();
        assert!(matches!(status.state, MediaJobState::Failed { .. }));
    }

    #[tokio::test]
    async fn resume_of_unknown_job_is_a_named_error() {
        let dir = TempDir::new().unwrap();
        let store = JobStore::open(dir.path());
        let provider = FakeProvider {
            state: MediaJobState::Running,
        };
        let err = resume(&store, &provider, "nope").await.unwrap_err();
        assert!(matches!(err, MediaError::Terminal(_)));
    }
}
