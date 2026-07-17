//! `stella-core` — the step-driver (`02-architecture.md` §2). One model call
//! per step, message accumulation, retry+backoff, context compaction,
//! tool-output budget + eviction, loop detection, USD budget metering.
//!
//! NO I/O of its own: the engine drives through the `Provider` and
//! `ToolExecutor` traits and emits `AgentEvent`s over a channel. All
//! decision logic (compaction, eviction, loop detection, budget) is plain
//! synchronous functions over owned data — easy to property-test
//! (`02-architecture.md` §1.3).

pub mod budget;
pub mod bus;
pub mod compaction;
pub mod driver;
pub mod estimator;
pub mod extensions;
mod glob;
pub mod goal;
pub mod hooks;
pub mod loop_detect;
pub mod mcp_usage;
pub(crate) mod mining;
pub mod ports;
pub mod retry;
pub mod router;
pub mod rules;
pub mod skills;

pub use budget::{BudgetGuard, BudgetOutcome};
// `bus::HookEvent` (the extension-bus envelope) stays module-qualified: the
// crate root already exports `hooks::HookEvent` (the shell-hook lifecycle
// enum) and the two must never be confused at a glance.
pub use bus::{
    ExtensionFailure, HookBus, HookDecision, HookEventDraft, HookSubscription, PolicyOutcome,
};
pub use driver::{Engine, EngineConfig, TurnOutcome};
pub use estimator::{Calibration, CalibrationMap};
pub use extensions::{
    AgentDef, CommandDef, ExistingTargets, ExtensionDiagnostic, ExtensionKind, ExtensionProblem,
    PlannedLink, SyncEntry, SyncPlan, SyncSkip, SyncSkipReason, SyncSource, agent_from_file,
    command_from_file, expand_command, merge_by_name, plan_extension_sync,
};
pub use goal::{GoalConfig, GoalOutcome};
pub use hooks::{HookEvent, HookPayload, HookRunOutcome, HookRunner, Hooks, run_hooks};
pub use loop_detect::{LoopDetectionConfig, LoopVerdict, detect_loop};
pub use mcp_usage::{McpUsageLedger, McpUsageRecord, drain_usage, push_usage};
pub use ports::{Clock, ToolExecutor};
pub use retry::{RetryOutcome, RetryPolicy, retry_with_backoff};
pub use router::{RoleTable, Router};
pub use rules::{
    GuardCheck, LoadRulesOptions, ProposedAction, Rule, RuleGuard, RuleSource, evaluate_guards,
    load_rules,
};
pub use skills::{
    AutoCreateConfig, AutoCreateDecision, AutoCreateSkip, InstallDecision, LoadSkillsOptions,
    SelectedSkill, SelectionConfig, Skill, SkillCandidate, SkillInstallProposal, SkillMineConfig,
    SkillObservation, SkillOrigin, SkillSource, decide_auto_creation, load_skills,
    mine_skill_candidates, render_skills_section, select_skills,
};
