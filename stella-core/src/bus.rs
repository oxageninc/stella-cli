//! The extension hook bus — a typed, in-process event system that lets
//! extensions observe agent activity and, on an explicit allowlist of
//! policy-sensitive actions, intercept it.
//!
//! Not to be confused with [`crate::hooks`], the *settings-declared shell
//! command* engine (Claude Code parity). This module is the programmatic
//! seam: in-process handlers registered against a catalog of dotted event
//! names ([`names`]), receiving a uniform [`HookEvent`] envelope.
//!
//! # Two hook kinds
//!
//! - **Observers** ([`HookBus::on`], [`HookBus::emit`]) watch events. They
//!   can never veto, delay, or corrupt the primary operation: a handler
//!   that returns `Err` (or panics) is logged, surfaced as an
//!   `extension.error` event, and skipped — the emitting operation
//!   continues unconditionally. The **supported** failure signal is
//!   `Err(msg)`; a panic is *also* contained via [`catch_unwind`], but only
//!   under an unwinding profile — the workspace's `release` profile sets
//!   `panic = "abort"`, where nothing can catch a panic, so extensions must
//!   report failure by returning `Err`, not by panicking.
//! - **Policy hooks** ([`HookBus::on_blocking`], [`HookBus::emit_blocking`])
//!   run *sequentially in registration order* before a policy-sensitive
//!   operation (the [`names::BLOCKING`] set: tool execution, filesystem
//!   writes, shell commands, git/PR/deployment actions). Each returns a
//!   [`HookDecision`]; the chain folds `Modify` into the payload and stops
//!   at the first `Deny`/`RequireApproval`. Every `emit_blocking` records
//!   its final decision in a `policy.evaluated` event, plus one of
//!   `policy.allowed` / `policy.blocked` / `approval.requested`.
//!
//! # Delivery model
//!
//! `emit` runs handlers inline on the emitting thread — there is no queue
//! to overflow and no executor coupling, and the "non-blocking" contract is
//! behavioral: observers cannot block or fail the primary operation.
//! Extensions that want genuinely asynchronous processing subscribe with
//! [`forward_to`], which pushes envelopes into a `tokio` channel consumed
//! on the extension's own task. No lock is held while a handler runs, so
//! handlers may safely re-enter the bus (`emit`, `on`, `off`).
//!
//! # Ordering
//!
//! Every event gets a `sequence` number, monotonic within the session
//! (assignment order). Events emitted from one thread are delivered in
//! emission order; concurrent emitters are delivered as they interleave —
//! consumers needing a total order sort by `sequence`.
//!
//! # Payload hygiene
//!
//! Payloads must not carry secrets, credentials, or full file contents by
//! default. Emitters use [`sanitize_tool_input`] to strip content-bearing
//! fields from *observable* events; the raw input is exposed only to
//! blocking policy handlers (the explicitly-privileged interception point)
//! and to the `emit_blocking` caller. [`scan_for_secrets`] and
//! [`is_sensitive_path`] back the `secret.detected` /
//! `sensitive_operation.detected` emissions.
//!
//! # No I/O in this module
//!
//! The bus is plain synchronous logic over owned data: registration lists,
//! an atomic sequence counter, and inline dispatch. Timestamping reads the
//! system clock (a pure computation over `SystemTime`, not I/O in the
//! architectural sense — no filesystem, no network, no processes).

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use stella_protocol::AgentEvent;

// Event names — the catalog

/// The event-name catalog. Names are dotted, lowercase, and namespaced;
/// subscriptions match exactly, by namespace wildcard (`file.*`, `tool.*`,
/// `tool.call.*`), or globally (`*`). Extensions may emit custom names —
/// the catalog is the contract for what the *host* emits, not a closed set.
pub mod names {
    // ---- session ----
    pub const SESSION_CREATED: &str = "session.created";
    pub const SESSION_STARTED: &str = "session.started";
    pub const SESSION_PAUSED: &str = "session.paused";
    pub const SESSION_RESUMED: &str = "session.resumed";
    pub const SESSION_CANCEL_REQUESTED: &str = "session.cancel_requested";
    pub const SESSION_CANCELLED: &str = "session.cancelled";
    pub const SESSION_COMPLETED: &str = "session.completed";
    pub const SESSION_FAILED: &str = "session.failed";
    // ---- agent + transcript ----
    pub const AGENT_TURN_STARTED: &str = "agent.turn.started";
    pub const AGENT_THINKING_STARTED: &str = "agent.thinking.started";
    pub const AGENT_THINKING_DELTA: &str = "agent.thinking.delta";
    pub const AGENT_THINKING_COMPLETED: &str = "agent.thinking.completed";
    pub const AGENT_MESSAGE_CREATED: &str = "agent.message.created";
    pub const AGENT_MESSAGE_DELTA: &str = "agent.message.delta";
    pub const AGENT_MESSAGE_COMPLETED: &str = "agent.message.completed";
    pub const AGENT_TURN_COMPLETED: &str = "agent.turn.completed";
    pub const AGENT_ERROR: &str = "agent.error";
    pub const TRANSCRIPT_ENTRY_CREATED: &str = "transcript.entry.created";
    pub const TRANSCRIPT_ENTRY_UPDATED: &str = "transcript.entry.updated";
    // ---- model ----
    pub const MODEL_REQUEST_STARTED: &str = "model.request.started";
    pub const MODEL_REQUEST_COMPLETED: &str = "model.request.completed";
    pub const MODEL_REQUEST_FAILED: &str = "model.request.failed";
    pub const MODEL_RESPONSE_STARTED: &str = "model.response.started";
    pub const MODEL_RESPONSE_DELTA: &str = "model.response.delta";
    pub const MODEL_RESPONSE_COMPLETED: &str = "model.response.completed";
    pub const MODEL_RATE_LIMITED: &str = "model.rate_limited";
    pub const MODEL_CONTEXT_COMPACTED: &str = "model.context.compacted";
    // ---- tools ----
    pub const TOOL_REGISTERED: &str = "tool.registered";
    pub const TOOL_CALL_REQUESTED: &str = "tool.call.requested";
    pub const TOOL_CALL_VALIDATED: &str = "tool.call.validated";
    pub const TOOL_CALL_STARTED: &str = "tool.call.started";
    pub const TOOL_CALL_PROGRESS: &str = "tool.call.progress";
    pub const TOOL_CALL_COMPLETED: &str = "tool.call.completed";
    pub const TOOL_CALL_FAILED: &str = "tool.call.failed";
    pub const TOOL_CALL_CANCELLED: &str = "tool.call.cancelled";
    // ---- policy + approval ----
    pub const POLICY_EVALUATED: &str = "policy.evaluated";
    pub const POLICY_ALLOWED: &str = "policy.allowed";
    pub const POLICY_BLOCKED: &str = "policy.blocked";
    pub const APPROVAL_REQUESTED: &str = "approval.requested";
    pub const APPROVAL_GRANTED: &str = "approval.granted";
    pub const APPROVAL_DENIED: &str = "approval.denied";
    pub const APPROVAL_EXPIRED: &str = "approval.expired";
    pub const SECRET_DETECTED: &str = "secret.detected";
    pub const SENSITIVE_OPERATION_DETECTED: &str = "sensitive_operation.detected";
    // ---- files + workspace ----
    pub const FILE_READ: &str = "file.read";
    pub const FILE_CREATED: &str = "file.created";
    pub const FILE_UPDATED: &str = "file.updated";
    pub const FILE_DELETED: &str = "file.deleted";
    pub const FILE_RENAMED: &str = "file.renamed";
    pub const FILE_DIFF_COMPUTED: &str = "file.diff.computed";
    pub const FILES_TOUCHED_UPDATED: &str = "files_touched.updated";
    pub const WORKSPACE_OPENED: &str = "workspace.opened";
    pub const WORKSPACE_INDEX_STARTED: &str = "workspace.index.started";
    pub const WORKSPACE_INDEX_COMPLETED: &str = "workspace.index.completed";
    pub const SEARCH_STARTED: &str = "search.started";
    pub const SEARCH_COMPLETED: &str = "search.completed";
    pub const SEARCH_FAILED: &str = "search.failed";
    // ---- commands + validation ----
    pub const COMMAND_STARTED: &str = "command.started";
    pub const COMMAND_STDOUT: &str = "command.stdout";
    pub const COMMAND_STDERR: &str = "command.stderr";
    pub const COMMAND_COMPLETED: &str = "command.completed";
    pub const COMMAND_FAILED: &str = "command.failed";
    pub const BUILD_STARTED: &str = "build.started";
    pub const BUILD_COMPLETED: &str = "build.completed";
    pub const BUILD_FAILED: &str = "build.failed";
    pub const TEST_STARTED: &str = "test.started";
    pub const TEST_COMPLETED: &str = "test.completed";
    pub const TEST_FAILED: &str = "test.failed";
    pub const DIAGNOSTIC_DETECTED: &str = "diagnostic.detected";
    pub const DIAGNOSTIC_RESOLVED: &str = "diagnostic.resolved";
    // ---- git + delivery ----
    pub const GIT_STATUS_CHANGED: &str = "git.status.changed";
    pub const GIT_DIFF_CREATED: &str = "git.diff.created";
    pub const GIT_COMMIT_REQUESTED: &str = "git.commit.requested";
    pub const GIT_COMMIT_CREATED: &str = "git.commit.created";
    pub const GIT_PUSH_REQUESTED: &str = "git.push.requested";
    pub const GIT_PUSH_COMPLETED: &str = "git.push.completed";
    pub const PULL_REQUEST_REQUESTED: &str = "pull_request.requested";
    pub const PULL_REQUEST_CREATED: &str = "pull_request.created";
    pub const DEPLOYMENT_REQUESTED: &str = "deployment.requested";
    pub const DEPLOYMENT_COMPLETED: &str = "deployment.completed";
    pub const DEPLOYMENT_FAILED: &str = "deployment.failed";
    // ---- extensions + telemetry ----
    pub const EXTENSION_LOADED: &str = "extension.loaded";
    pub const EXTENSION_UNLOADED: &str = "extension.unloaded";
    pub const EXTENSION_ERROR: &str = "extension.error";
    pub const TELEMETRY_EVENT_QUEUED: &str = "telemetry.event.queued";
    pub const TELEMETRY_EVENT_FLUSHED: &str = "telemetry.event.flushed";
    pub const TELEMETRY_EVENT_FAILED: &str = "telemetry.event.failed";

    /// Every catalog name, grouped as above. Extensions may emit names not
    /// in this list; hosts should not.
    pub const ALL: &[&str] = &[
        SESSION_CREATED,
        SESSION_STARTED,
        SESSION_PAUSED,
        SESSION_RESUMED,
        SESSION_CANCEL_REQUESTED,
        SESSION_CANCELLED,
        SESSION_COMPLETED,
        SESSION_FAILED,
        AGENT_TURN_STARTED,
        AGENT_THINKING_STARTED,
        AGENT_THINKING_DELTA,
        AGENT_THINKING_COMPLETED,
        AGENT_MESSAGE_CREATED,
        AGENT_MESSAGE_DELTA,
        AGENT_MESSAGE_COMPLETED,
        AGENT_TURN_COMPLETED,
        AGENT_ERROR,
        TRANSCRIPT_ENTRY_CREATED,
        TRANSCRIPT_ENTRY_UPDATED,
        MODEL_REQUEST_STARTED,
        MODEL_REQUEST_COMPLETED,
        MODEL_REQUEST_FAILED,
        MODEL_RESPONSE_STARTED,
        MODEL_RESPONSE_DELTA,
        MODEL_RESPONSE_COMPLETED,
        MODEL_RATE_LIMITED,
        MODEL_CONTEXT_COMPACTED,
        TOOL_REGISTERED,
        TOOL_CALL_REQUESTED,
        TOOL_CALL_VALIDATED,
        TOOL_CALL_STARTED,
        TOOL_CALL_PROGRESS,
        TOOL_CALL_COMPLETED,
        TOOL_CALL_FAILED,
        TOOL_CALL_CANCELLED,
        POLICY_EVALUATED,
        POLICY_ALLOWED,
        POLICY_BLOCKED,
        APPROVAL_REQUESTED,
        APPROVAL_GRANTED,
        APPROVAL_DENIED,
        APPROVAL_EXPIRED,
        SECRET_DETECTED,
        SENSITIVE_OPERATION_DETECTED,
        FILE_READ,
        FILE_CREATED,
        FILE_UPDATED,
        FILE_DELETED,
        FILE_RENAMED,
        FILE_DIFF_COMPUTED,
        FILES_TOUCHED_UPDATED,
        WORKSPACE_OPENED,
        WORKSPACE_INDEX_STARTED,
        WORKSPACE_INDEX_COMPLETED,
        SEARCH_STARTED,
        SEARCH_COMPLETED,
        SEARCH_FAILED,
        COMMAND_STARTED,
        COMMAND_STDOUT,
        COMMAND_STDERR,
        COMMAND_COMPLETED,
        COMMAND_FAILED,
        BUILD_STARTED,
        BUILD_COMPLETED,
        BUILD_FAILED,
        TEST_STARTED,
        TEST_COMPLETED,
        TEST_FAILED,
        DIAGNOSTIC_DETECTED,
        DIAGNOSTIC_RESOLVED,
        GIT_STATUS_CHANGED,
        GIT_DIFF_CREATED,
        GIT_COMMIT_REQUESTED,
        GIT_COMMIT_CREATED,
        GIT_PUSH_REQUESTED,
        GIT_PUSH_COMPLETED,
        PULL_REQUEST_REQUESTED,
        PULL_REQUEST_CREATED,
        DEPLOYMENT_REQUESTED,
        DEPLOYMENT_COMPLETED,
        DEPLOYMENT_FAILED,
        EXTENSION_LOADED,
        EXTENSION_UNLOADED,
        EXTENSION_ERROR,
        TELEMETRY_EVENT_QUEUED,
        TELEMETRY_EVENT_FLUSHED,
        TELEMETRY_EVENT_FAILED,
    ];

    /// The events hosts route through [`super::HookBus::emit_blocking`] —
    /// the explicit allowlist of interceptable, policy-sensitive actions.
    pub const BLOCKING: &[&str] = &[
        TOOL_CALL_REQUESTED,
        FILE_CREATED,
        FILE_UPDATED,
        FILE_DELETED,
        COMMAND_STARTED,
        GIT_COMMIT_REQUESTED,
        GIT_PUSH_REQUESTED,
        PULL_REQUEST_REQUESTED,
        DEPLOYMENT_REQUESTED,
    ];

    /// Whether `name` is in the host catalog.
    pub fn is_known(name: &str) -> bool {
        ALL.contains(&name)
    }

    /// Whether `name` is a blocking (interceptable) event.
    pub fn is_blocking(name: &str) -> bool {
        BLOCKING.contains(&name)
    }
}

// Envelope + decisions

/// The uniform envelope every hook event is delivered in. Serializes to the
/// documented JSON shape (`id`, `name`, `timestamp`, `session_id`,
/// `turn_id?`, `agent_id?`, `sequence`, `payload`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    /// Unique per event: `evt_<session_id>_<sequence>`.
    pub id: String,
    /// Dotted event name (see [`names`]).
    pub name: String,
    /// ISO 8601 UTC with millisecond precision (`2026-07-16T12:34:56.789Z`).
    pub timestamp: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Monotonic within the session, in assignment order, starting at 1.
    pub sequence: u64,
    pub payload: Value,
}

/// What a caller hands to [`HookBus::emit`]/[`HookBus::emit_blocking`]: the
/// name and payload, plus optional per-event turn/agent overrides. The bus
/// seals the rest of the envelope (id, timestamp, session, sequence) and
/// falls back to the ambient context ([`HookBus::set_turn`]) for the ids.
#[derive(Debug, Clone, PartialEq)]
pub struct HookEventDraft {
    pub name: String,
    pub payload: Value,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
}

impl HookEventDraft {
    pub fn new(name: impl Into<String>, payload: Value) -> Self {
        Self {
            name: name.into(),
            payload,
            turn_id: None,
            agent_id: None,
        }
    }

    pub fn turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    pub fn agent(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }
}

/// What one blocking handler decides. Serializes to the documented shape:
/// `{"action":"allow"}`, `{"action":"deny","reason":...}`,
/// `{"action":"require_approval","reason":...}`,
/// `{"action":"modify","payload":...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HookDecision {
    Allow,
    Deny { reason: String },
    RequireApproval { reason: String },
    Modify { payload: Value },
}

/// The folded result of one [`HookBus::emit_blocking`] chain.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyOutcome {
    /// The sealed event, its payload reflecting any `Modify` decisions.
    pub event: HookEvent,
    /// The final decision: `Allow`, `Deny`, or `RequireApproval` — never
    /// `Modify` (modifications fold into `event.payload` and the chain
    /// continues; a chain that only modified ends `Allow`).
    pub decision: HookDecision,
    /// Whether any handler modified the payload.
    pub modified: bool,
}

impl PolicyOutcome {
    /// True when the operation may proceed.
    pub fn allowed(&self) -> bool {
        matches!(self.decision, HookDecision::Allow)
    }
}

/// One isolated observer/policy handler failure, kept in a bounded
/// in-memory log ([`HookBus::recent_failures`]) alongside the
/// `extension.error` emission — hosts render or persist it; the bus never
/// writes to stderr (it would corrupt a TUI).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionFailure {
    /// The subscription pattern whose handler failed.
    pub pattern: String,
    /// The event being delivered when the handler failed.
    pub event_name: String,
    pub message: String,
    pub timestamp: String,
}

// Subscriptions

/// Result type observer handlers return: `Err` reports a handled failure
/// (isolated exactly like a panic — logged + `extension.error`).
pub type ObserverResult = Result<(), String>;

type ObserverFn = dyn Fn(&HookEvent) -> ObserverResult + Send + Sync;
type BlockingFn = dyn Fn(&HookEvent) -> HookDecision + Send + Sync;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandlerKind {
    Observer,
    Blocking,
}

struct Registration<H: ?Sized> {
    id: u64,
    pattern: String,
    handler: Arc<H>,
}

/// A live hook registration. Dropping it unsubscribes (cleanup-safe by
/// construction); [`HookSubscription::detach`] opts out, keeping the
/// handler for the bus's lifetime. Removal is idempotent — unsubscribing
/// twice (or after the bus is gone) is a no-op.
#[must_use = "dropping a HookSubscription unsubscribes its handler; call detach() to keep it registered"]
pub struct HookSubscription {
    bus: Weak<BusInner>,
    id: u64,
    kind: HandlerKind,
    detached: bool,
}

impl HookSubscription {
    /// Remove the handler now. Equivalent to dropping the subscription.
    pub fn unsubscribe(mut self) {
        self.remove();
        self.detached = true; // Drop becomes a no-op
    }

    /// Keep the handler registered for the bus's lifetime and discard the
    /// handle.
    pub fn detach(mut self) {
        self.detached = true;
    }

    fn remove(&self) {
        let Some(inner) = self.bus.upgrade() else {
            return;
        };
        match self.kind {
            HandlerKind::Observer => {
                let mut observers = inner.observers.lock().unwrap_or_else(|p| p.into_inner());
                observers.retain(|r| r.id != self.id);
            }
            HandlerKind::Blocking => {
                let mut blockers = inner.blockers.lock().unwrap_or_else(|p| p.into_inner());
                blockers.retain(|r| r.id != self.id);
            }
        }
    }
}

impl Drop for HookSubscription {
    fn drop(&mut self) {
        if !self.detached {
            self.remove();
        }
    }
}

// The bus

#[derive(Default)]
struct AmbientContext {
    turn_id: Option<String>,
    agent_id: Option<String>,
}

/// Cap on the in-memory failure log — enough for a session postmortem,
/// bounded so a hot broken handler cannot grow memory without limit.
const MAX_RECENT_FAILURES: usize = 64;

struct BusInner {
    session_id: String,
    sequence: AtomicU64,
    next_registration: AtomicU64,
    observers: Mutex<Vec<Registration<ObserverFn>>>,
    blockers: Mutex<Vec<Registration<BlockingFn>>>,
    context: Mutex<AmbientContext>,
    recent_failures: Mutex<VecDeque<ExtensionFailure>>,
}

/// The session-scoped hook bus. Cheap to clone (shared inner); every clone
/// emits into and subscribes to the same session stream.
#[derive(Clone)]
pub struct HookBus {
    inner: Arc<BusInner>,
}

impl HookBus {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(BusInner {
                session_id: session_id.into(),
                sequence: AtomicU64::new(0),
                next_registration: AtomicU64::new(0),
                observers: Mutex::new(Vec::new()),
                blockers: Mutex::new(Vec::new()),
                context: Mutex::new(AmbientContext::default()),
                recent_failures: Mutex::new(VecDeque::new()),
            }),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    /// Set (or clear) the ambient turn id stamped onto events whose draft
    /// carries none. Hosts call this at turn boundaries.
    pub fn set_turn(&self, turn_id: Option<String>) {
        self.inner
            .context
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .turn_id = turn_id;
    }

    /// Set (or clear) the ambient agent id (fleet/subagent attribution).
    pub fn set_agent(&self, agent_id: Option<String>) {
        self.inner
            .context
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_id = agent_id;
    }

    // ---- registration ------------------------------------------------

    /// Subscribe an observer to `pattern`: an exact name (`"file.read"`), a
    /// namespace wildcard (`"file.*"`, `"tool.call.*"`), or `"*"` for every
    /// event. Handlers run inline in registration order; failures are
    /// isolated (see the module docs).
    pub fn on<F>(&self, pattern: &str, handler: F) -> HookSubscription
    where
        F: Fn(&HookEvent) -> ObserverResult + Send + Sync + 'static,
    {
        let id = self.inner.next_registration.fetch_add(1, Ordering::Relaxed);
        self.inner
            .observers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Registration {
                id,
                pattern: pattern.to_string(),
                handler: Arc::new(handler),
            });
        HookSubscription {
            bus: Arc::downgrade(&self.inner),
            id,
            kind: HandlerKind::Observer,
            detached: false,
        }
    }

    /// Register a blocking policy handler for `pattern` (same matching
    /// rules as [`HookBus::on`]). Blocking handlers run sequentially in
    /// registration order under [`HookBus::emit_blocking`]; a handler that
    /// panics fails **closed** — the chain stops with `Deny` (a broken
    /// policy extension must never silently wave operations through). The
    /// fail-closed-on-panic net requires an unwinding profile; under
    /// `release`'s `panic = "abort"` a panic aborts, so a policy handler
    /// should signal refusal by returning `Deny`, not by panicking.
    pub fn on_blocking<F>(&self, pattern: &str, handler: F) -> HookSubscription
    where
        F: Fn(&HookEvent) -> HookDecision + Send + Sync + 'static,
    {
        let id = self.inner.next_registration.fetch_add(1, Ordering::Relaxed);
        self.inner
            .blockers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Registration {
                id,
                pattern: pattern.to_string(),
                handler: Arc::new(handler),
            });
        HookSubscription {
            bus: Arc::downgrade(&self.inner),
            id,
            kind: HandlerKind::Blocking,
            detached: false,
        }
    }

    /// Unsubscribe. Sugar for [`HookSubscription::unsubscribe`], provided
    /// for API parity (`on`/`off`/`emit`/`emit_blocking`).
    pub fn off(&self, subscription: HookSubscription) {
        subscription.unsubscribe();
    }

    // ---- emission ------------------------------------------------------

    /// Seal and deliver an observer event. Never fails, never blocks the
    /// caller on handler outcomes; returns the sealed envelope.
    pub fn emit(&self, draft: HookEventDraft) -> HookEvent {
        let event = self.seal(draft);
        self.dispatch(&event);
        event
    }

    /// [`HookBus::emit`] sugar without draft-building noise.
    pub fn emit_named(&self, name: &str, payload: Value) -> HookEvent {
        self.emit(HookEventDraft::new(name, payload))
    }

    /// Seal the event and run its blocking policy chain: matching handlers
    /// in registration order, folding `Modify` into the payload, stopping
    /// at the first `Deny`/`RequireApproval`. Records the final decision in
    /// a `policy.evaluated` event plus one of `policy.allowed` /
    /// `policy.blocked` / `approval.requested`.
    ///
    /// The blocking event itself is delivered **only to blocking handlers**
    /// ([`HookBus::on_blocking`]) — never to observers ([`HookBus::on`]).
    /// That is the payload-hygiene boundary: the raw payload (which may carry
    /// full file contents or a shell command) reaches only the explicitly
    /// privileged interception point. Observers see the *outcome* — the
    /// `policy.*` records (decision only, no payload) — plus whatever
    /// sanitized observable events the emitter chooses to also send (e.g. the
    /// registry's `tool.call.started`).
    pub fn emit_blocking(&self, draft: HookEventDraft) -> PolicyOutcome {
        let mut event = self.seal(draft);
        let chain: Vec<(String, Arc<BlockingFn>)> = {
            let blockers = self
                .inner
                .blockers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            blockers
                .iter()
                .filter(|r| pattern_matches(&r.pattern, &event.name))
                .map(|r| (r.pattern.clone(), r.handler.clone()))
                .collect()
        };

        let mut decision = HookDecision::Allow;
        let mut modified = false;
        let mut consulted = 0usize;
        for (pattern, handler) in chain {
            consulted += 1;
            match catch_unwind(AssertUnwindSafe(|| handler(&event))) {
                Ok(HookDecision::Allow) => {}
                Ok(HookDecision::Modify { payload }) => {
                    event.payload = payload;
                    modified = true;
                }
                Ok(final_decision) => {
                    decision = final_decision;
                    break;
                }
                Err(panic) => {
                    let message = panic_message(panic);
                    self.report_failure(
                        &pattern,
                        &event,
                        format!("policy handler panicked: {message}"),
                    );
                    decision = HookDecision::Deny {
                        reason: format!("policy handler for `{pattern}` failed: {message}"),
                    };
                    break;
                }
            }
        }

        let record = serde_json::json!({
            "event_name": event.name,
            "event_id": event.id,
            "decision": serde_json::to_value(&decision).unwrap_or(Value::Null),
            "modified": modified,
            "handlers_consulted": consulted,
        });
        let stamp = |name: &str| HookEventDraft {
            name: name.to_string(),
            payload: record.clone(),
            turn_id: event.turn_id.clone(),
            agent_id: event.agent_id.clone(),
        };
        self.emit(stamp(names::POLICY_EVALUATED));
        match &decision {
            HookDecision::Allow | HookDecision::Modify { .. } => {
                self.emit(stamp(names::POLICY_ALLOWED));
            }
            HookDecision::Deny { .. } => {
                self.emit(stamp(names::POLICY_BLOCKED));
            }
            HookDecision::RequireApproval { .. } => {
                self.emit(stamp(names::APPROVAL_REQUESTED));
            }
        }

        PolicyOutcome {
            event,
            decision,
            modified,
        }
    }

    /// The bounded log of isolated handler failures, oldest first.
    pub fn recent_failures(&self) -> Vec<ExtensionFailure> {
        self.inner
            .recent_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    // ---- internals -----------------------------------------------------

    fn seal(&self, draft: HookEventDraft) -> HookEvent {
        let sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let (ambient_turn, ambient_agent) = {
            let ctx = self.inner.context.lock().unwrap_or_else(|p| p.into_inner());
            (ctx.turn_id.clone(), ctx.agent_id.clone())
        };
        HookEvent {
            id: format!("evt_{}_{sequence}", self.inner.session_id),
            name: draft.name,
            timestamp: now_iso8601_utc(),
            session_id: self.inner.session_id.clone(),
            turn_id: draft.turn_id.or(ambient_turn),
            agent_id: draft.agent_id.or(ambient_agent),
            sequence,
            payload: draft.payload,
        }
    }

    /// Deliver to every matching observer, isolating each failure. The
    /// handler list is snapshotted first, so no lock is held while a
    /// handler runs (handlers may re-enter the bus) — the flip side is that
    /// a handler unsubscribed *during* a dispatch may still receive the
    /// in-flight event.
    fn dispatch(&self, event: &HookEvent) {
        let matching: Vec<(String, Arc<ObserverFn>)> = {
            let observers = self
                .inner
                .observers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            observers
                .iter()
                .filter(|r| pattern_matches(&r.pattern, &event.name))
                .map(|r| (r.pattern.clone(), r.handler.clone()))
                .collect()
        };
        for (pattern, handler) in matching {
            let outcome = catch_unwind(AssertUnwindSafe(|| handler(event)));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(message)) => self.report_failure(&pattern, event, message),
                Err(panic) => {
                    self.report_failure(&pattern, event, panic_message(panic));
                }
            }
        }
    }

    /// Log one isolated handler failure and surface it as an
    /// `extension.error` event. Failures raised while delivering
    /// `extension.error` itself are logged but not re-emitted — the
    /// recursion guard that keeps one broken global handler from looping.
    fn report_failure(&self, pattern: &str, event: &HookEvent, message: String) {
        {
            let mut log = self
                .inner
                .recent_failures
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if log.len() == MAX_RECENT_FAILURES {
                log.pop_front();
            }
            log.push_back(ExtensionFailure {
                pattern: pattern.to_string(),
                event_name: event.name.clone(),
                message: message.clone(),
                timestamp: now_iso8601_utc(),
            });
        }
        if event.name != names::EXTENSION_ERROR {
            self.emit(HookEventDraft {
                name: names::EXTENSION_ERROR.to_string(),
                payload: serde_json::json!({
                    "pattern": pattern,
                    "failed_event": event.name,
                    "failed_event_id": event.id,
                    "error": message,
                }),
                turn_id: event.turn_id.clone(),
                agent_id: event.agent_id.clone(),
            });
        }
    }
}

/// Bridge the policy/extension audit plane into an `AgentEvent` stream
/// (receipts spec §6.4, #364 gap 6): subscribes an observer that maps the
/// four audit event names — `policy.evaluated`, `policy.blocked`,
/// `approval.requested`, `secret.detected` — onto
/// [`AgentEvent::PolicyDecision`], content-free. Whatever journal the host
/// hangs off `events` is what makes the plane durable; the bus itself still
/// never writes (its module contract), and every other event name passes
/// through untouched.
///
/// `subject` is the gated chain's event name (`tool.call.requested`,
/// `file.updated`, …) — or the workspace-relative path for a secret
/// detection. `outcome` is the decision record's compact JSON (`decision` +
/// `handlers_consulted`) or the detector's kind list; neither ever carries
/// file contents or a secret value.
pub fn bridge_policy_plane(
    bus: &HookBus,
    events: crate::event_sender::EventSender,
) -> HookSubscription {
    use stella_protocol::PolicyKind;
    bus.on("*", move |event| {
        let kind = match event.name.as_str() {
            names::POLICY_EVALUATED => PolicyKind::Evaluated,
            names::POLICY_BLOCKED => PolicyKind::Blocked,
            names::APPROVAL_REQUESTED => PolicyKind::ApprovalRequested,
            names::SECRET_DETECTED => PolicyKind::SecretDetected,
            _ => return Ok(()),
        };
        let (subject, outcome) = if kind == PolicyKind::SecretDetected {
            (
                event.payload["path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                event.payload["kinds"].to_string(),
            )
        } else {
            (
                event.payload["event_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                serde_json::json!({
                    "decision": event.payload["decision"],
                    "handlers_consulted": event.payload["handlers_consulted"],
                })
                .to_string(),
            )
        };
        events
            .send(AgentEvent::PolicyDecision {
                kind,
                subject,
                outcome,
            })
            .map_err(|e| e.to_string())
    })
}

/// An observer that forwards every envelope into a `tokio` channel — the
/// bridge from the bus's inline delivery to an extension's own async task:
/// `bus.on("*", forward_to(tx)).detach()`.
pub fn forward_to(
    sender: tokio::sync::mpsc::UnboundedSender<HookEvent>,
) -> impl Fn(&HookEvent) -> ObserverResult + Send + Sync + 'static {
    move |event| {
        sender
            .send(event.clone())
            .map_err(|_| "forward_to receiver dropped".to_string())
    }
}

/// Whether `pattern` matches `name`: `*` matches everything; `ns.*`
/// matches any name under the `ns.` prefix (`file.*` → `file.read`,
/// `tool.*` → `tool.call.requested`, but never `filesystem.read` or bare
/// `file`); anything else matches exactly.
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" || pattern == name {
        return true;
    }
    match pattern.strip_suffix(".*") {
        Some(namespace) => name
            .strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|rest| !rest.is_empty()),
        None => false,
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "handler panicked".to_string()
    }
}

// Payload hygiene — redaction + sensitivity helpers

/// High-precision secret shapes the scanner recognizes. Deliberately a
/// short list of near-zero-false-positive patterns rather than an entropy
/// heuristic — a `secret.detected` event must be actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    /// `-----BEGIN … PRIVATE KEY-----` PEM block.
    PrivateKeyPem,
    /// AWS access key id (`AKIA` + 16 uppercase alphanumerics).
    AwsAccessKeyId,
    /// GitHub token (`ghp_`/`gho_`/`ghs_`/`ghr_`/`github_pat_` prefixes).
    GitHubToken,
    /// Slack token (`xoxb-`/`xoxp-`/`xoxa-`/`xoxs-` prefixes).
    SlackToken,
    /// Google API key (`AIza` + 35 key characters).
    GoogleApiKey,
    /// Generic `sk-…` secret key (OpenAI/Anthropic/Stripe style).
    SkPrefixedKey,
}

impl SecretKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::PrivateKeyPem => "private_key_pem",
            SecretKind::AwsAccessKeyId => "aws_access_key_id",
            SecretKind::GitHubToken => "github_token",
            SecretKind::SlackToken => "slack_token",
            SecretKind::GoogleApiKey => "google_api_key",
            SecretKind::SkPrefixedKey => "sk_prefixed_key",
        }
    }
}

/// Scan text for the [`SecretKind`] shapes, deduplicated, detection order.
/// Emitters use this to fire `secret.detected` — the event names the
/// *kind*, never the matched text.
pub fn scan_for_secrets(text: &str) -> Vec<SecretKind> {
    let mut kinds = Vec::new();
    let mut push = |kind: SecretKind| {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    };
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        push(SecretKind::PrivateKeyPem);
    }
    if has_prefixed_run(text, "AKIA", 16, |c| {
        c.is_ascii_uppercase() || c.is_ascii_digit()
    }) {
        push(SecretKind::AwsAccessKeyId);
    }
    for prefix in ["ghp_", "gho_", "ghs_", "ghr_", "github_pat_"] {
        if has_prefixed_run(text, prefix, 20, |c| c.is_ascii_alphanumeric() || c == '_') {
            push(SecretKind::GitHubToken);
            break;
        }
    }
    for prefix in ["xoxb-", "xoxp-", "xoxa-", "xoxs-"] {
        if has_prefixed_run(text, prefix, 10, |c| c.is_ascii_alphanumeric() || c == '-') {
            push(SecretKind::SlackToken);
            break;
        }
    }
    if has_prefixed_run(text, "AIza", 35, |c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    }) {
        push(SecretKind::GoogleApiKey);
    }
    if has_prefixed_run(text, "sk-", 20, |c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    }) {
        push(SecretKind::SkPrefixedKey);
    }
    kinds
}

/// Whether `text` contains `prefix` immediately followed by at least
/// `min_run` characters accepted by `charset`.
fn has_prefixed_run(text: &str, prefix: &str, min_run: usize, charset: fn(char) -> bool) -> bool {
    let mut haystack = text;
    while let Some(pos) = haystack.find(prefix) {
        let rest = &haystack[pos + prefix.len()..];
        if rest.chars().take_while(|c| charset(*c)).count() >= min_run {
            return true;
        }
        haystack = &haystack[pos + prefix.len()..];
    }
    false
}

/// The input fields that carry full file contents. Observable events
/// replace their values with a size summary; only blocking policy handlers
/// (and the emitting caller) see the raw input.
const CONTENT_FIELDS: &[&str] = &["content", "new_string", "old_string"];

/// A copy of a tool input safe for observable payloads: content-bearing
/// string fields are replaced with `"<omitted: N bytes, M lines>"`.
/// Non-object inputs pass through unchanged (they carry no known
/// content fields).
pub fn sanitize_tool_input(input: &Value) -> Value {
    let Some(map) = input.as_object() else {
        return input.clone();
    };
    let mut sanitized = map.clone();
    for field in CONTENT_FIELDS {
        if let Some(Value::String(text)) = map.get(*field) {
            sanitized.insert(
                (*field).to_string(),
                Value::String(format!(
                    "<omitted: {} bytes, {} lines>",
                    text.len(),
                    text.lines().count()
                )),
            );
        }
    }
    Value::Object(sanitized)
}

/// Filename shapes that conventionally hold credentials or private keys —
/// touching one fires `sensitive_operation.detected` (path only, never
/// contents).
pub fn is_sensitive_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    base.starts_with(".env")
        || base.contains("credential")
        || base.contains("secret")
        || base == "id_rsa"
        || base == "id_ed25519"
        || base == ".netrc"
        || base == ".npmrc"
        || base == ".pypirc"
        || base.ends_with(".pem")
        || base.ends_with(".key")
}

// Timestamps — ISO 8601 UTC without a date-time dependency

fn now_iso8601_utc() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso8601_utc_millis(millis)
}

/// Format Unix milliseconds as `YYYY-MM-DDThh:mm:ss.mmmZ`. Fixed-width and
/// zero-padded, so lexicographic order equals time order (same property as
/// `stella-context`'s clock, extended to millisecond precision).
pub fn iso8601_utc_millis(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1_000);
    let millis = unix_millis.rem_euclid(1_000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days since 1970-01-01 → `(year, month, day)` (Howard Hinnant's civil
/// algorithm, same derivation as `stella-context::clock`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Bus + a `Vec` capturing every event a `"*"` observer sees.
    fn observed_bus(session: &str) -> (HookBus, Arc<Mutex<Vec<HookEvent>>>) {
        let bus = HookBus::new(session);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        bus.on("*", move |event| {
            sink.lock().unwrap().push(event.clone());
            Ok(())
        })
        .detach();
        (bus, seen)
    }

    fn names_of(seen: &Arc<Mutex<Vec<HookEvent>>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    // ---- policy-plane bridge (receipts spec §6.4) ----------------------

    #[test]
    fn bridge_maps_the_audit_plane_onto_policy_decision_events() {
        use stella_protocol::PolicyKind;
        let bus = HookBus::new("bridge-test");
        bus.on_blocking(names::TOOL_CALL_REQUESTED, |_| HookDecision::Deny {
            reason: "not on my watch".into(),
        })
        .detach();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _bridge = bridge_policy_plane(&bus, crate::event_sender::EventSender::new(tx));

        // A denied chain → policy.evaluated + policy.blocked, both bridged.
        let outcome = bus.emit_blocking(HookEventDraft::new(
            names::TOOL_CALL_REQUESTED,
            serde_json::json!({ "tool": "bash", "input": {"command": "rm -rf /"} }),
        ));
        assert!(!outcome.allowed());
        // A secret detection is bridged with the path as subject.
        bus.emit_named(
            names::SECRET_DETECTED,
            serde_json::json!({ "path": "src/config.rs", "kinds": ["aws_key"] }),
        );
        // A non-audit event must NOT be bridged.
        bus.emit_named(names::FILE_READ, serde_json::json!({ "path": "a.rs" }));

        let bridged: Vec<AgentEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let kinds: Vec<PolicyKind> = bridged
            .iter()
            .map(|e| match e {
                AgentEvent::PolicyDecision { kind, .. } => *kind,
                other => panic!("only PolicyDecision events cross the bridge: {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                PolicyKind::Evaluated,
                PolicyKind::Blocked,
                PolicyKind::SecretDetected
            ],
            "{bridged:?}"
        );
        match &bridged[1] {
            AgentEvent::PolicyDecision {
                subject, outcome, ..
            } => {
                assert_eq!(subject, names::TOOL_CALL_REQUESTED);
                assert!(outcome.contains("deny"), "{outcome}");
                // Content-free: the decision record, never the tool input.
                assert!(!outcome.contains("rm -rf"), "{outcome}");
            }
            other => panic!("expected PolicyDecision: {other:?}"),
        }
        match &bridged[2] {
            AgentEvent::PolicyDecision {
                subject, outcome, ..
            } => {
                assert_eq!(subject, "src/config.rs");
                assert!(outcome.contains("aws_key"), "{outcome}");
            }
            other => panic!("expected PolicyDecision: {other:?}"),
        }
    }

    #[test]
    fn dropping_the_bridge_subscription_stops_the_flow() {
        let bus = HookBus::new("bridge-drop");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = bridge_policy_plane(&bus, crate::event_sender::EventSender::new(tx));
        drop(bridge); // un-detached subscription unsubscribes on drop
        bus.emit_named(
            names::SECRET_DETECTED,
            serde_json::json!({ "path": "a.rs", "kinds": ["token"] }),
        );
        assert!(rx.try_recv().is_err(), "no bridge, no events");
    }

    // ---- catalog -------------------------------------------------------

    #[test]
    fn catalog_has_every_documented_name_without_duplicates() {
        assert_eq!(names::ALL.len(), 87);
        let unique: std::collections::HashSet<&str> = names::ALL.iter().copied().collect();
        assert_eq!(unique.len(), names::ALL.len(), "duplicate catalog name");
        for name in names::ALL {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "`{name}` is not a dotted lowercase name"
            );
        }
        // Spot-check one name from each group.
        for expected in [
            "session.cancel_requested",
            "agent.thinking.delta",
            "transcript.entry.updated",
            "model.context.compacted",
            "tool.call.cancelled",
            "sensitive_operation.detected",
            "files_touched.updated",
            "diagnostic.resolved",
            "pull_request.created",
            "telemetry.event.failed",
        ] {
            assert!(names::is_known(expected), "missing `{expected}`");
        }
    }

    #[test]
    fn blocking_set_is_exactly_the_documented_nine() {
        assert_eq!(names::BLOCKING.len(), 9);
        for name in names::BLOCKING {
            assert!(names::is_known(name), "blocking `{name}` not in catalog");
            assert!(names::is_blocking(name));
        }
        assert!(!names::is_blocking(names::FILE_READ));
    }

    // ---- matching (spec test 1: exact and wildcard subscriptions) ------

    #[test]
    fn pattern_matching_supports_exact_namespace_and_global() {
        assert!(pattern_matches("file.read", "file.read"));
        assert!(!pattern_matches("file.read", "file.created"));
        assert!(pattern_matches("file.*", "file.read"));
        assert!(pattern_matches("file.*", "file.diff.computed"));
        assert!(pattern_matches("tool.*", "tool.call.requested"));
        assert!(pattern_matches("tool.call.*", "tool.call.completed"));
        assert!(pattern_matches("*", "anything.at.all"));
        // Prefix confusion and degenerate names never match.
        assert!(!pattern_matches("file.*", "filesystem.read"));
        assert!(!pattern_matches("file.*", "file"));
        assert!(!pattern_matches("file.*", "file."));
        assert!(!pattern_matches("tool.call.*", "tool.registered"));
    }

    #[test]
    fn exact_and_wildcard_subscriptions_each_receive_matching_events() {
        let bus = HookBus::new("s");
        let exact = Arc::new(AtomicUsize::new(0));
        let ns = Arc::new(AtomicUsize::new(0));
        let all = Arc::new(AtomicUsize::new(0));
        let (e, n, a) = (exact.clone(), ns.clone(), all.clone());
        let _s1 = bus.on(names::FILE_READ, move |_| {
            e.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let _s2 = bus.on("file.*", move |_| {
            n.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let _s3 = bus.on("*", move |_| {
            a.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        bus.emit_named(names::FILE_READ, serde_json::json!({"path": "a"}));
        bus.emit_named(names::FILE_CREATED, serde_json::json!({"path": "b"}));
        bus.emit_named(names::COMMAND_STARTED, serde_json::json!({"command": "ls"}));

        assert_eq!(exact.load(Ordering::SeqCst), 1);
        assert_eq!(ns.load(Ordering::SeqCst), 2);
        assert_eq!(all.load(Ordering::SeqCst), 3);
    }

    // ---- envelope ------------------------------------------------------

    #[test]
    fn envelope_serializes_to_the_documented_shape() {
        let bus = HookBus::new("sess-1");
        bus.set_turn(Some("turn-9".into()));
        let event = bus.emit(
            HookEventDraft::new(names::FILE_READ, serde_json::json!({"path": "src/a.rs"}))
                .agent("agent-2"),
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["id"], "evt_sess-1_1");
        assert_eq!(json["name"], "file.read");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(
            json["turn_id"], "turn-9",
            "ambient turn context stamps events"
        );
        assert_eq!(json["agent_id"], "agent-2", "draft override wins");
        assert_eq!(json["sequence"], 1);
        assert_eq!(json["payload"]["path"], "src/a.rs");
        let ts = json["timestamp"].as_str().unwrap();
        assert_eq!(ts.len(), "2026-07-16T12:34:56.789Z".len());
        assert!(ts.ends_with('Z') && ts.contains('T'));
        // And it round-trips.
        let back: HookEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn absent_turn_and_agent_ids_are_omitted_from_the_wire_shape() {
        let bus = HookBus::new("s");
        let event = bus.emit_named(names::FILE_READ, Value::Null);
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("turn_id"));
        assert!(!json.contains("agent_id"));
    }

    #[test]
    fn iso8601_formats_known_instants() {
        assert_eq!(iso8601_utc_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_utc_millis(1_600_000_000_123),
            "2020-09-13T12:26:40.123Z"
        );
    }

    #[test]
    fn hook_decisions_serialize_to_the_documented_action_shapes() {
        let cases = [
            (HookDecision::Allow, r#"{"action":"allow"}"#),
            (
                HookDecision::Deny {
                    reason: "nope".into(),
                },
                r#"{"action":"deny","reason":"nope"}"#,
            ),
            (
                HookDecision::RequireApproval {
                    reason: "ask".into(),
                },
                r#"{"action":"require_approval","reason":"ask"}"#,
            ),
            (
                HookDecision::Modify {
                    payload: serde_json::json!({"x": 1}),
                },
                r#"{"action":"modify","payload":{"x":1}}"#,
            ),
        ];
        for (decision, expected) in cases {
            assert_eq!(serde_json::to_string(&decision).unwrap(), expected);
            let back: HookDecision = serde_json::from_str(expected).unwrap();
            assert_eq!(back, decision);
        }
    }

    // ---- ordering + sequence (spec test 4) ------------------------------

    #[test]
    fn sequences_are_monotonic_and_delivery_preserves_emission_order() {
        let (bus, seen) = observed_bus("s");
        for i in 0..50 {
            bus.emit_named(names::AGENT_MESSAGE_DELTA, serde_json::json!({"i": i}));
        }
        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 50);
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.sequence, i as u64 + 1, "monotonic from 1");
            assert_eq!(event.payload["i"], i, "delivered in emission order");
        }
    }

    #[test]
    fn sequences_are_per_session_not_global() {
        let a = HookBus::new("a");
        let b = HookBus::new("b");
        assert_eq!(a.emit_named(names::FILE_READ, Value::Null).sequence, 1);
        assert_eq!(a.emit_named(names::FILE_READ, Value::Null).sequence, 2);
        assert_eq!(b.emit_named(names::FILE_READ, Value::Null).sequence, 1);
    }

    #[test]
    fn concurrent_emitters_never_lose_events_or_reuse_sequence_numbers() {
        let (bus, seen) = observed_bus("s");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let bus = bus.clone();
                std::thread::spawn(move || {
                    for i in 0..100 {
                        bus.emit_named(names::FILE_READ, serde_json::json!({"t": t, "i": i}));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let mut sequences: Vec<u64> = seen.lock().unwrap().iter().map(|e| e.sequence).collect();
        assert_eq!(sequences.len(), 800, "no event lost");
        sequences.sort_unstable();
        assert_eq!(
            sequences,
            (1..=800).collect::<Vec<u64>>(),
            "sequences dense and unique"
        );
    }

    // ---- observer failure isolation (spec test 2) -----------------------

    #[test]
    fn failing_observers_never_stop_delivery_and_surface_extension_error() {
        let bus = HookBus::new("s");
        let delivered = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(Mutex::new(Vec::new()));

        let error_sink = errors.clone();
        bus.on(names::EXTENSION_ERROR, move |event| {
            error_sink.lock().unwrap().push(event.clone());
            Ok(())
        })
        .detach();
        bus.on(names::FILE_READ, |_| Err("returned failure".to_string()))
            .detach();
        bus.on(names::FILE_READ, |_| panic!("panicked failure"))
            .detach();
        let count = delivered.clone();
        bus.on(names::FILE_READ, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .detach();

        bus.emit_named(names::FILE_READ, serde_json::json!({"path": "a"}));

        assert_eq!(
            delivered.load(Ordering::SeqCst),
            1,
            "the healthy handler after two broken ones still ran"
        );
        let errors = errors.lock().unwrap();
        assert_eq!(errors.len(), 2, "one extension.error per failure");
        assert_eq!(errors[0].payload["failed_event"], "file.read");
        assert_eq!(errors[0].payload["error"], "returned failure");
        assert_eq!(errors[1].payload["error"], "panicked failure");
        let log = bus.recent_failures();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].message, "returned failure");
    }

    #[test]
    fn a_broken_extension_error_handler_does_not_recurse() {
        let bus = HookBus::new("s");
        // This handler fails on EVERY event — including extension.error.
        // Without the recursion guard this loops forever.
        bus.on("*", |_| Err("always broken".to_string())).detach();
        bus.emit_named(names::FILE_READ, Value::Null);
        // file.read failed → extension.error emitted → that delivery failed
        // too → logged only. Two log entries, no infinite recursion.
        assert_eq!(bus.recent_failures().len(), 2);
    }

    #[test]
    fn failure_log_is_bounded() {
        let bus = HookBus::new("s");
        bus.on(names::FILE_READ, |_| Err("broken".to_string()))
            .detach();
        for _ in 0..(MAX_RECENT_FAILURES + 20) {
            bus.emit_named(names::FILE_READ, Value::Null);
        }
        assert_eq!(bus.recent_failures().len(), MAX_RECENT_FAILURES);
    }

    // ---- blocking chain (spec test 3) ------------------------------------

    #[test]
    fn blocking_allow_when_no_handler_objects() {
        let (bus, seen) = observed_bus("s");
        let outcome = bus.emit_blocking(HookEventDraft::new(
            names::TOOL_CALL_REQUESTED,
            serde_json::json!({"tool": "bash"}),
        ));
        assert!(outcome.allowed());
        assert!(!outcome.modified);
        // policy.evaluated recorded the (default-allow) decision.
        let names_seen = names_of(&seen);
        assert_eq!(names_seen, vec!["policy.evaluated", "policy.allowed"]);
        let evaluated = &seen.lock().unwrap()[0];
        assert_eq!(evaluated.payload["decision"]["action"], "allow");
        assert_eq!(evaluated.payload["handlers_consulted"], 0);
        assert_eq!(evaluated.payload["event_name"], "tool.call.requested");
    }

    #[test]
    fn blocking_deny_short_circuits_and_records_the_reason() {
        let (bus, seen) = observed_bus("s");
        let later = Arc::new(AtomicUsize::new(0));
        let _a = bus.on_blocking(names::TOOL_CALL_REQUESTED, |_| HookDecision::Deny {
            reason: "workspace is read-only".into(),
        });
        let count = later.clone();
        let _b = bus.on_blocking(names::TOOL_CALL_REQUESTED, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            HookDecision::Allow
        });

        let outcome = bus.emit_blocking(HookEventDraft::new(
            names::TOOL_CALL_REQUESTED,
            serde_json::json!({"tool": "write_file"}),
        ));

        assert_eq!(
            outcome.decision,
            HookDecision::Deny {
                reason: "workspace is read-only".into()
            }
        );
        assert_eq!(
            later.load(Ordering::SeqCst),
            0,
            "handlers after a deny never run"
        );
        // Scope the guard: binding `&seen.lock()...[0]` would hold the lock
        // across the `names_of(&seen)` re-lock below and deadlock (std Mutex
        // is not reentrant).
        {
            let events = seen.lock().unwrap();
            let evaluated = &events[0];
            assert_eq!(evaluated.name, "policy.evaluated");
            assert_eq!(evaluated.payload["decision"]["action"], "deny");
            assert_eq!(
                evaluated.payload["decision"]["reason"],
                "workspace is read-only"
            );
        }
        assert_eq!(names_of(&seen)[1], "policy.blocked");
    }

    #[test]
    fn blocking_require_approval_short_circuits_and_emits_approval_requested() {
        let (bus, seen) = observed_bus("s");
        let _a = bus.on_blocking("deployment.*", |_| HookDecision::RequireApproval {
            reason: "production deploys need a human".into(),
        });
        let outcome = bus.emit_blocking(HookEventDraft::new(
            names::DEPLOYMENT_REQUESTED,
            serde_json::json!({"target": "prod"}),
        ));
        assert_eq!(
            outcome.decision,
            HookDecision::RequireApproval {
                reason: "production deploys need a human".into()
            }
        );
        assert_eq!(
            names_of(&seen),
            vec!["policy.evaluated", "approval.requested"]
        );
    }

    #[test]
    fn blocking_modify_folds_payload_and_later_handlers_see_it() {
        let bus = HookBus::new("s");
        let _a = bus.on_blocking(names::COMMAND_STARTED, |event| {
            let mut payload = event.payload.clone();
            payload["command"] = Value::String("ls -la".into());
            HookDecision::Modify { payload }
        });
        let observed_by_second = Arc::new(Mutex::new(String::new()));
        let sink = observed_by_second.clone();
        let _b = bus.on_blocking(names::COMMAND_STARTED, move |event| {
            *sink.lock().unwrap() = event.payload["command"].as_str().unwrap_or("").to_string();
            HookDecision::Allow
        });

        let outcome = bus.emit_blocking(HookEventDraft::new(
            names::COMMAND_STARTED,
            serde_json::json!({"command": "ls"}),
        ));

        assert!(outcome.allowed(), "modify alone still allows");
        assert!(outcome.modified);
        assert_eq!(outcome.event.payload["command"], "ls -la");
        assert_eq!(
            *observed_by_second.lock().unwrap(),
            "ls -la",
            "the chain is sequential: handler 2 saw handler 1's modification"
        );
    }

    #[test]
    fn blocking_handlers_run_in_registration_order() {
        let bus = HookBus::new("s");
        let order = Arc::new(Mutex::new(Vec::new()));
        for tag in ["first", "second", "third"] {
            let order = order.clone();
            bus.on_blocking(names::GIT_PUSH_REQUESTED, move |_| {
                order.lock().unwrap().push(tag);
                HookDecision::Allow
            })
            .detach();
        }
        let _ = bus.emit_blocking(HookEventDraft::new(names::GIT_PUSH_REQUESTED, Value::Null));
        assert_eq!(*order.lock().unwrap(), vec!["first", "second", "third"]);
    }

    #[test]
    fn a_panicking_policy_handler_fails_closed() {
        let bus = HookBus::new("s");
        let _a = bus.on_blocking(names::FILE_DELETED, |_| panic!("broken extension"));
        let outcome = bus.emit_blocking(HookEventDraft::new(
            names::FILE_DELETED,
            serde_json::json!({"path": "a"}),
        ));
        match &outcome.decision {
            HookDecision::Deny { reason } => {
                assert!(reason.contains("broken extension"), "{reason}");
            }
            other => panic!("a panicking policy handler must deny, got {other:?}"),
        }
        assert_eq!(bus.recent_failures().len(), 1);
    }

    #[test]
    fn blocking_events_consume_session_sequence_numbers_too() {
        let bus = HookBus::new("s");
        let first = bus.emit_named(names::FILE_READ, Value::Null);
        let outcome = bus.emit_blocking(HookEventDraft::new(names::FILE_CREATED, Value::Null));
        let last = bus.emit_named(names::FILE_READ, Value::Null);
        assert_eq!(first.sequence, 1);
        assert_eq!(outcome.event.sequence, 2);
        // seq 3 + 4 were the policy.evaluated / policy.allowed records.
        assert_eq!(last.sequence, 5);
    }

    // ---- disposal (spec test 8) -----------------------------------------

    #[test]
    fn unsubscribe_stops_delivery() {
        let bus = HookBus::new("s");
        let count = Arc::new(AtomicUsize::new(0));
        let sink = count.clone();
        let sub = bus.on(names::FILE_READ, move |_| {
            sink.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        bus.emit_named(names::FILE_READ, Value::Null);
        sub.unsubscribe();
        bus.emit_named(names::FILE_READ, Value::Null);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_the_subscription_unsubscribes() {
        let bus = HookBus::new("s");
        let count = Arc::new(AtomicUsize::new(0));
        {
            let sink = count.clone();
            let _sub = bus.on("*", move |_| {
                sink.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            bus.emit_named(names::FILE_READ, Value::Null);
        } // _sub dropped here
        bus.emit_named(names::FILE_READ, Value::Null);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn detach_keeps_the_handler_for_the_bus_lifetime() {
        let bus = HookBus::new("s");
        let count = Arc::new(AtomicUsize::new(0));
        let sink = count.clone();
        bus.on("*", move |_| {
            sink.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .detach();
        bus.emit_named(names::FILE_READ, Value::Null);
        bus.emit_named(names::FILE_READ, Value::Null);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn off_removes_blocking_handlers_and_outlives_the_bus_safely() {
        let bus = HookBus::new("s");
        let sub = bus.on_blocking(names::FILE_CREATED, |_| HookDecision::Deny {
            reason: "no".into(),
        });
        assert!(
            !bus.emit_blocking(HookEventDraft::new(names::FILE_CREATED, Value::Null))
                .allowed()
        );
        bus.off(sub);
        assert!(
            bus.emit_blocking(HookEventDraft::new(names::FILE_CREATED, Value::Null))
                .allowed()
        );

        // A subscription outliving its bus unsubscribes into nothing.
        let orphan = {
            let short_lived = HookBus::new("gone");
            short_lived.on("*", |_| Ok(()))
        };
        orphan.unsubscribe(); // must not panic
    }

    #[test]
    fn handlers_can_reenter_the_bus_without_deadlocking() {
        let (bus, seen) = observed_bus("s");
        let reentrant = bus.clone();
        bus.on(names::COMMAND_COMPLETED, move |_| {
            // Re-entrant emit from inside a delivery: no lock is held while
            // handlers run, so this must not deadlock.
            reentrant.emit_named(names::TELEMETRY_EVENT_QUEUED, Value::Null);
            Ok(())
        })
        .detach();
        bus.emit_named(names::COMMAND_COMPLETED, Value::Null);
        let names_seen = names_of(&seen);
        assert_eq!(
            names_seen,
            vec!["command.completed", "telemetry.event.queued"]
        );
    }

    // ---- forwarding ------------------------------------------------------

    #[tokio::test]
    async fn forward_to_bridges_events_into_a_tokio_channel() {
        let bus = HookBus::new("s");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bus.on("file.*", forward_to(tx)).detach();
        bus.emit_named(names::FILE_CREATED, serde_json::json!({"path": "a"}));
        let event = rx.recv().await.unwrap();
        assert_eq!(event.name, "file.created");
        // A dropped receiver turns into an isolated failure, not a crash.
        drop(rx);
        bus.emit_named(names::FILE_DELETED, Value::Null);
        assert_eq!(bus.recent_failures().len(), 1);
    }

    // ---- payload hygiene --------------------------------------------------

    #[test]
    fn sanitize_replaces_content_fields_with_summaries() {
        let input = serde_json::json!({
            "path": "src/a.rs",
            "content": "line1\nline2\n",
            "reason": "why"
        });
        let sanitized = sanitize_tool_input(&input);
        assert_eq!(sanitized["path"], "src/a.rs");
        assert_eq!(sanitized["reason"], "why");
        assert_eq!(sanitized["content"], "<omitted: 12 bytes, 2 lines>");
        // Non-objects pass through.
        assert_eq!(sanitize_tool_input(&Value::Null), Value::Null);
    }

    #[test]
    fn secret_scanner_matches_known_shapes_and_nothing_mundane() {
        let hits = scan_for_secrets(concat!(
            "aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n",
            "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\n",
            "token: ghp_abcdefghijklmnopqrstuvwxyz012345\n",
        ));
        assert_eq!(
            hits,
            vec![
                SecretKind::PrivateKeyPem,
                SecretKind::AwsAccessKeyId,
                SecretKind::GitHubToken
            ]
        );
        assert!(scan_for_secrets("let skill = task_id; // AKIA nothing").is_empty());
        assert!(scan_for_secrets("fn main() { println!(\"hello\"); }").is_empty());
        assert_eq!(
            scan_for_secrets("OPENAI_API_KEY=sk-proj-abcdefghij0123456789abcdef"),
            vec![SecretKind::SkPrefixedKey]
        );
    }

    #[test]
    fn sensitive_paths_are_flagged_by_shape() {
        for sensitive in [
            ".env",
            ".env.production",
            "config/credentials.yml",
            "deploy/secrets.json",
            "~/.ssh/id_rsa",
            "certs/server.pem",
            "keys/signing.key",
        ] {
            assert!(is_sensitive_path(sensitive), "`{sensitive}` should flag");
        }
        for mundane in ["src/main.rs", "README.md", "env.rs", "keyboard.rs"] {
            assert!(!is_sensitive_path(mundane), "`{mundane}` should not flag");
        }
    }
}
