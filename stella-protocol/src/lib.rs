//! `stella-protocol` — serde types shared by every crate in the `stella-cli`
//! workspace: agent events, tool schemas, trace records, and provider
//! request/response envelopes.
//!
//! Zero logic, zero I/O. This is the stability contract of the whole
//! workspace — any
//! type here that crosses a process/protocol boundary must round-trip through
//! `serde_json` byte-for-byte (see the `roundtrip` tests in each module).

pub mod attachment;
pub mod completion;
pub mod error;
pub mod event;
pub mod provider;
pub mod role;
pub mod tool;

pub use attachment::{
    Attachment, AttachmentKind, AttachmentSource, classify_media_type, human_bytes,
    media_type_for_path,
};
pub use completion::{
    CompletionMessage, CompletionRequest, CompletionResult, CompletionUsage, FinishReason,
    MessageRole, ReasoningEffort,
};
pub use error::ProviderError;
pub use event::{
    AgentEvent, BudgetMode, ContextFrameRef, FileChangeKind, JudgeEvidence, MediaArtifactRef,
    MediaJobState, MediaKind, PrStatus, ProviderShare, ScopeProposal, StageKind,
};
pub use provider::{Provider, ToolCallObserver};
pub use role::{ModelRef, Role};
pub use tool::{ToolCall, ToolOutput, ToolResult, ToolSchema};
