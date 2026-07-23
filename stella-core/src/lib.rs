//! `stella-core` — the step-driver. One model call
//! per step, message accumulation, retry+backoff, context compaction,
//! tool-output budget + eviction, loop detection, USD budget metering.
//!
//! NO I/O of its own: the engine drives through the `Provider` and
//! `ToolExecutor` traits and emits `AgentEvent`s over a channel. All
//! decision logic (compaction, eviction, loop detection, budget) is plain
//! synchronous functions over owned data — easy to property-test
//!

pub mod accounted_call;
pub mod budget;
pub mod bus;
pub mod compaction;
pub mod discovery;
pub mod driver;
pub mod estimator;
pub mod event_sender;
pub mod extensions;
mod glob;
pub mod goal;
pub mod hooks;
pub mod loop_detect;
pub mod mcp_usage;
pub(crate) mod mining;
pub mod ports;
pub mod receipts;
pub mod retry;
pub mod router;
pub mod rules;
pub mod skills;
pub(crate) mod speculation;
mod summarize;
pub mod tasks;

pub use budget::{BudgetGuard, BudgetOutcome};
// `bus::HookEvent` (the extension-bus envelope) stays module-qualified: the
// crate root already exports `hooks::HookEvent` (the shell-hook lifecycle
// enum) and the two must never be confused at a glance.
pub use accounted_call::{AccountedCall, AccountedCallError, run_accounted_call};
pub use bus::{
    ExtensionFailure, HookBus, HookDecision, HookEventDraft, HookSubscription, PolicyOutcome,
};
pub use discovery::{Candidate, DiscoveryQuery, QueryTerm, RankedMatch, parse_query, rank};
pub use driver::{Engine, EngineConfig, SOFT_STOP_REASON, TurnOutcome};
pub use estimator::{Calibration, CalibrationMap};
pub use event_sender::{EventSendError, EventSender};
pub use extensions::{
    AgentDef, CommandDef, ExistingTargets, ExtensionDiagnostic, ExtensionKind, ExtensionProblem,
    PlannedLink, SyncEntry, SyncPlan, SyncSkip, SyncSkipReason, SyncSource, agent_from_file,
    command_from_file, expand_command, merge_by_name, plan_extension_sync,
};
pub use goal::{GoalConfig, GoalJudgeVerdict, GoalOutcome};
pub use hooks::{HookEvent, HookPayload, HookRunOutcome, HookRunner, Hooks, run_hooks};
pub use loop_detect::{LoopDetectionConfig, LoopVerdict, detect_loop};
pub use mcp_usage::{McpUsageLedger, McpUsageRecord, drain_usage, push_usage};
pub use ports::{Clock, ToolExecutor};
pub use retry::{RetryOutcome, RetryPolicy, retry_with_backoff};
pub use router::{RoleTable, Router};
pub use rules::{
    GuardCheck, LoadRulesOptions, ProposedAction, Rule, RuleEnforcement, RuleGuard, RuleMetadata,
    RuleMetadataError, RuleOrigin, RuleRecordKind, RuleSource, evaluate_guards, load_rules,
    render_rule_metadata,
};
pub use skills::{
    AutoCreateConfig, AutoCreateDecision, AutoCreateSkip, InstallDecision, LoadSkillsOptions,
    SelectedSkill, SelectionConfig, Skill, SkillCandidate, SkillInstallProposal, SkillMineConfig,
    SkillObservation, SkillOrigin, SkillSource, decide_auto_creation, load_skills,
    mine_skill_candidates, render_skills_section, select_skills,
};
pub use tasks::{SpawnRequest, TaskBoard, TaskBoardError};
