use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use stella_media::{MediaJobState, MediaOperationJournal, MediaProvider};
use stella_protocol::tool::{ToolOutput, ToolSchema};

use super::authority::{open_jobs, open_store, reconciliation_required};
use crate::registry::Tool;

pub struct PollVideo {
    provider: Arc<dyn MediaProvider>,
    operation_journal: Option<Arc<dyn MediaOperationJournal>>,
}

impl PollVideo {
    pub fn new(provider: Arc<dyn MediaProvider>) -> Self {
        Self {
            provider,
            operation_journal: None,
        }
    }

    pub(crate) fn with_operation_journal(
        provider: Arc<dyn MediaProvider>,
        operation_journal: Arc<dyn MediaOperationJournal>,
    ) -> Self {
        Self {
            provider,
            operation_journal: Some(operation_journal),
        }
    }
}

#[async_trait]
impl Tool for PollVideo {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "poll_video".into(),
            description: "Check a submitted video job live. On success, save it under \
                          .stella/artifacts/; while queued or running, poll again later."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"job_id": {"type": "string"}},
                "required": ["job_id"]
            }),
            read_only: false,
        }
    }

    async fn execute(&self, input: &Value, root: &std::path::Path) -> ToolOutput {
        let Some(job_id) = input.get("job_id").and_then(|value| value.as_str()) else {
            return ToolOutput::Error {
                message: "`job_id` is required".into(),
            };
        };
        let jobs = open_jobs(root);
        let job = match jobs.get(job_id) {
            Ok(Some(job)) => job,
            Ok(None) => {
                return ToolOutput::Error {
                    message: format!("no persisted video job `{job_id}`"),
                };
            }
            Err(error) => {
                return ToolOutput::Error {
                    message: format!("video job store unavailable: {error}"),
                };
            }
        };
        let status = match stella_media::resume(&jobs, self.provider.as_ref(), job_id).await {
            Ok(status) => status,
            Err(error) => {
                return ToolOutput::Error {
                    message: format!("video poll failed: {error}"),
                };
            }
        };
        match status.state {
            MediaJobState::Succeeded => {
                let Some(artifact) = status.artifact else {
                    return ToolOutput::Error {
                        message: format!("video job `{job_id}` succeeded without an artifact"),
                    };
                };
                let store = match open_store(root) {
                    Ok(store) => store,
                    Err(error) => return error,
                };
                match store.save_with_id(
                    &job.artifact_id,
                    &artifact.bytes,
                    artifact.kind,
                    &artifact.extension,
                    &artifact.label,
                ) {
                    Ok(saved) => {
                        if let Some(journal) = &self.operation_journal
                            && let Err(error) = journal.complete_video(self.provider.id(), job_id)
                        {
                            return reconciliation_required(job_id, error);
                        }
                        if let Err(error) = jobs.remove(job_id) {
                            return reconciliation_required(job_id, error);
                        }
                        ToolOutput::Ok {
                            content: format!(
                                "video \"{}\" → {} (model {}, ${:.4})",
                                saved.label, saved.path, artifact.model, artifact.cost_usd
                            ),
                        }
                    }
                    Err(error) => ToolOutput::Error {
                        message: format!("could not persist the video: {error}"),
                    },
                }
            }
            MediaJobState::Failed { reason } => {
                if let Err(error) = jobs.remove(job_id) {
                    return reconciliation_required(job_id, error);
                }
                ToolOutput::Error {
                    message: format!("video job `{job_id}` failed: {reason}"),
                }
            }
            MediaJobState::Queued | MediaJobState::Running => {
                let state = match status.state {
                    MediaJobState::Queued => "queued",
                    _ => "running",
                };
                let progress = status
                    .progress
                    .map(|value| format!(" ({:.0}% reported)", value * 100.0))
                    .unwrap_or_default();
                ToolOutput::Ok {
                    content: format!(
                        "video job `{job_id}` is {state}{progress} — poll again later"
                    ),
                }
            }
        }
    }
}
