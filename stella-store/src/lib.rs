//! `stella-store` — the session's local SQLite database at
//! `.stella/private/store.db`: durable state, full execution records, and
//! analytics-grade telemetry, all on the user's disk (no server, no
//! account — the local-first non-negotiable).
//!
//! What lives here:
//! - **executions** — one row per run/goal/turn: prompt, provider/model,
//!   outcome, total cost, and (since v8) the nullable cross-process session
//!   registry id ([`SessionRecord::id`]) stamping which session the turn ran
//!   under, so [`Store::session_events`] can reassemble a session's full
//!   journal across executions.
//! - **events** — the COMPLETE `AgentEvent` stream per execution, one JSON
//!   row per event in order, including `Reasoning` deltas — the full chain
//!   of thought is replayable against its execution.
//! - **tasks** — the latest task-board snapshot per session, mirrored from
//!   `TaskUpdate` events: one row per (session, task id), upserted whole by
//!   [`Store::record_task_board`] so the board is re-openable cross-process.
//! - **pull_requests** — tracked pull requests keyed by URL: lifecycle
//!   status, CI verdict, and the session that produced them.
//! - **telemetry** — one row per committed model call (from `StepUsage`):
//!   provider, model, tokens in/out, the engine's pre-call input estimate
//!   (the drift sample [`Store::drift_samples`] serves back to calibrate
//!   future estimates), cache read hits, cache misses, cache writes, cost
//!   (computed from the model card's pricing × token counts in the
//!   adapter), duration, retries, tool-call count.
//! - **files_touched** — the file-touch telemetry per execution: one row per
//!   normalized workspace-relative path, carrying its deduplicated CRUD
//!   letters, session line-delta totals, and the ordered JSON audit log of
//!   every individual touch (event, reason, per-touch line delta).
//! - **memory_citations** — the self-improvement feedback loop: one row per
//!   memory the agent explicitly cited as having informed a turn (the
//!   `cite_memory` tool), carrying the agent's usefulness score, whether the
//!   memory's content still held true, and a short remark. Aggregated by
//!   [`Store::memory_citation_stats`] into the rule-promotion eligibility
//!   gate `stella memory` surfaces.
//! - **rules** — extension-authored workspace rules: one row per rule id,
//!   holding the full rule markdown in the `.stella/rules/*.md` authoring
//!   format (the store never parses it — `stella_core::rules` does).
//!   Written by extension providers via [`Store::upsert_rule`]; read at
//!   session start by `stella-cli`, which merges these (lowest precedence)
//!   with the on-disk rule files.
//! - **file_locks** — schema + API for cooperative file claims in
//!   multi-agent work, acquired by the fleet dispatcher and the deck's
//!   sub-session claim-on-first-write path (see # Concurrency below).
//! - **graph_nodes / graph_edges** — schema reserved as a future seam for a
//!   context plane; not written by any shipping command today (`stella-context`
//!   and `stella-graph` currently use their own stores).
//!
//! # Concurrency
//!
//! One embedded SQLite file (`rusqlite`, bundled — the workspace's "one
//! storage engine" invariant; every other embedded store in this workspace
//! is SQLite too). WAL journaling is enabled at open, so a read-only caller
//! (`stella stats`) is never blocked by a live session's writes. SQLite
//! still serializes writers per file; one `Store` per session process is the
//! contract, and internally a `Mutex<Connection>` serializes writers across
//! in-process parallel agents. Multi-PROCESS fleets need a lock-holder (or
//! one store per worker + merge) — `stella fleet` follows exactly that: its
//! workers are threads, and the single orchestrator process holds the one
//! `Store` that acquires and releases their file claims.
//!
//! # Schema versioning
//!
//! `PRAGMA user_version` stamps every database with its schema version
//! (version 0 is the legacy pre-versioning shape). A fresh file is created
//! at [`SCHEMA_VERSION`] directly; an existing file is upgraded by the
//! ordered [`MIGRATIONS`] list, one transaction per step, with the new
//! version stamped inside that same transaction — a crash mid-migration
//! rolls the file back to the old version and old shape, never a mix.
//!
//! # Graceful degradation
//!
//! Every method returns `Result`; the CLI treats a failed store as
//! observability loss, never a work stoppage — it warns once and keeps
//! running (persistence must never take the agent down with it).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use stella_protocol::{AgentEvent, TaskItem, TaskStatus, ToolOutput};

// Module map — this file holds the row types, the `Store` handle and its
// query surface, and the tests; everything else is split by concern:
//   ddl         (crate-private) every table/index DDL at the CURRENT schema
//   migrations  (crate-private) versioned upgrades + the fresh-file bootstrap
//   cache_gaps  per-call cache-gap facts behind the `cache_expired_rewrite`
//               counter (split out to keep this file under its size ratchet)
//   cache_trend per-session cache trend — telemetry already persists these
//               facts; this groups them by session for `stella stats`
//   catalog     `catalog.db` — user-tier model catalog (slugs, pricing)
//   journal     append-only per-session sidecar journal (crash-safe resume)
//   notify      persist-until-read cross-session notifications
//   sessions    cross-process session registry (one JSON file per session)
//   usage       `usage.db` — user-tier cross-project telemetry aggregate
mod ddl;
mod migrations;
mod private;
#[cfg(test)]
mod private_state_tests;
#[cfg(test)]
mod quarantine_tests;
mod receipts;
mod reconstruct;
mod telemetry;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod usage_completeness_tests;

pub mod cache_gaps;
pub mod cache_trend;
pub mod catalog;
pub mod enterprise_telemetry;
pub mod journal;
pub mod notify;
pub mod sessions;
pub mod usage;

use ddl::TABLES;
use migrations::{
    MIGRATIONS, SCHEMA_VERSION, any_store_table_exists, apply_migration, create_latest_schema,
};

pub use cache_gaps::CacheCallGap;
pub use catalog::CatalogStore;
// The sidecar journal's writer is deliberately NOT re-exported at the top
// level: `SessionJournal` here names the DB read-model reassembled by
// [`Store::session_events`] (read-only replay), while
// [`journal::SessionJournal`] is the append-only sidecar writer behind
// live-session durability — two different artifacts that happen to share a
// natural name. Reach the writer through its module.
pub use journal::JournalRecord;
pub use notify::{Notification, NotificationStore};
pub use private::{
    WORKSPACE_PRIVATE_DIR, append_workspace_private_line, existing_workspace_private_sqlite_path,
    existing_workspace_private_state_path, read_sensitive_file_to_string,
    workspace_private_sqlite_path, workspace_private_state_path, write_sensitive_file_atomic,
};
pub(crate) use private::{
    ensure_private_dir, ensure_workspace_generated_ignore, ensure_workspace_state_dir,
    open_private_file, open_private_sqlite, read_private_to_string, write_private_atomic,
};
pub use receipts::{ContextBlockRow, ManifestBlockRow, StepManifestRow};
pub use reconstruct::Reconstruction;
pub use sessions::{SessionRecord, SessionRegistry, SessionStatus};
pub use telemetry::TelemetryRow;

/// FNV-1a/64 hex — a stable, dependency-free digest for prompt hashes and
/// tool-arg fingerprints (loop detection, not security). Also the
/// model-card version content hash (`catalog`) — change detection, not
/// security, there too.
pub(crate) fn fnv_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Wrapper error: everything the store can fail with, rendered.
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store: {}", self.0)
    }
}
impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError(e.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// Reject graph `properties` that are not a valid JSON document before they
/// reach a plain SQLite `TEXT` column. Under the previous DuckDB backend the
/// column was JSON-typed and refused malformed input at the DB boundary;
/// SQLite silently persists arbitrary bytes, so unparseable data would only
/// surface later, when a JSON-expecting reader chokes on it. The empty
/// default `"{}"` is valid JSON and passes.
fn validate_json_properties(properties: &str) -> Result<()> {
    serde_json::from_str::<serde_json::Value>(properties)
        .map(|_| ())
        .map_err(|e| StoreError(format!("graph properties are not valid JSON: {e}")))
}

fn sqlite_i64(name: &str, value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError(format!("{name} exceeds SQLite INTEGER range")))
}

/// One session-level file-touch record, ready to persist: the normalized
/// workspace-relative path, its deduplicated CRUD letters in first-occurrence
/// order (e.g. `"RUD"`), the session line-delta totals, and the ordered
/// audit log serialized as a JSON array of
/// `{event, reason, lines_added, lines_removed}` objects.
#[derive(Debug, Clone, PartialEq)]
pub struct FileTouchRow {
    pub path: String,
    pub ops: String,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub events_json: String,
}

/// One memory citation, ready to persist: the cited memory's stable public
/// id (the `nod_…` id the recall block showed the model), the agent's
/// usefulness score (1–5), whether the memory's content still held true this
/// turn, and a short free-text remark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCitationRow {
    pub memory_id: String,
    pub useful_score: i64,
    pub truthful: bool,
    pub remark: String,
}

impl MemoryCitationRow {
    /// Whether this citation counts as a *positive* use of the memory for
    /// promotion purposes: the content still held true AND the score clears
    /// [`POSITIVE_SCORE_MIN`]. Anything else is a negative remark.
    pub fn is_positive(&self) -> bool {
        self.truthful && self.useful_score >= POSITIVE_SCORE_MIN
    }
}

/// Minimum usefulness score (on the 1–5 scale `cite_memory` enforces) for a
/// citation to count as positive.
pub const POSITIVE_SCORE_MIN: i64 = 3;

/// A memory must be cited successfully STRICTLY MORE THAN this many times to
/// become rule-promotion eligible (spec: "more than 10 times" — exactly 10
/// is not enough).
pub const PROMOTION_CITATIONS_REQUIRED: i64 = 10;

/// A memory cited untruthful this many times (total, across its history) is
/// quarantined: excluded from recall entirely until a human reviews it or it
/// is re-cited as truthful. The threshold is deliberately low (2) because an
/// untruthful memory is active harm — it misleads future turns. Every
/// untruthful citation counts regardless of score, and one fresh truthful
/// citation does NOT clear quarantine — only `stella memory unquarantine`
/// (or deleting the stale memory) does.
pub const QUARANTINE_NEGATIVES_THRESHOLD: i64 = 2;

/// Per-memory citation aggregate — the data behind `stella memory` and the
/// rule-promotion eligibility gate.
///
/// Eligibility semantics (deliberately strict, per spec): a memory is
/// eligible once its **positive streak** — consecutive positive citations
/// since (and not counting) its most recent negative one, in citation order —
/// strictly exceeds [`PROMOTION_CITATIONS_REQUIRED`]. One negative remark
/// resets the streak to zero, disqualifying the memory until it re-earns
/// MORE THAN 10 fresh all-positive citations. A memory that was never cited
/// negatively has `positive_streak == citations`.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCitationStats {
    pub memory_id: String,
    /// Total citations recorded, positive and negative.
    pub citations: i64,
    /// Mean usefulness score across every citation.
    pub avg_score: f64,
    /// Fraction of citations whose `truthful` flag was set.
    pub truthful_rate: f64,
    /// Citations that were NOT positive ([`MemoryCitationRow::is_positive`]).
    pub negatives: i64,
    /// Consecutive positive citations since the most recent negative one.
    pub positive_streak: i64,
    /// `positive_streak > PROMOTION_CITATIONS_REQUIRED` — the promotion gate.
    pub eligible: bool,
    /// Whether this memory is quarantined from recall: `negatives >=
    /// QUARANTINE_NEGATIVES_THRESHOLD`. A quarantined memory is excluded
    /// from recall entirely — it is active harm to surface a memory multiple
    /// agents verified as wrong. Only `stella memory unquarantine` clears it.
    pub quarantined: bool,
}

/// One MCP tool call, ready to persist: which server + tool, an optional
/// reason (best-effort — external MCP tools rarely carry one), and the call
/// time in epoch millis. Unlike a file-touch (aggregated per path), this is a
/// per-call log row, so repeat calls to the same tool are distinct rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpUsageRow {
    pub server: String,
    pub tool: String,
    pub reason: String,
    pub called_at_ms: i64,
}

/// Per-(server, tool) MCP usage aggregate — the data behind the MCP tab's
/// "N calls" column and `stella mcp usage`. Ordered most-used first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpUsageStat {
    pub server: String,
    pub tool: String,
    /// How many times this tool was called (all executions).
    pub calls: i64,
    /// The most recent non-empty reason recorded, or empty if none ever was.
    pub last_reason: String,
    /// Epoch millis of the most recent call.
    pub last_called_at_ms: i64,
}

/// One extension-authored workspace rule, as stored: the full rule markdown
/// in the `.stella/rules/*.md` authoring format plus the writer's label.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleRow {
    /// The rule id — the analog of a rule file's filename stem.
    pub rule_id: String,
    /// Full rule markdown (optional frontmatter + body). Opaque to the
    /// store; `stella_core::rules::rule_from_file` parses it.
    pub contents: String,
    /// Opaque label naming the writer (extension/provider id).
    pub source: String,
}

/// One agent-invocation row for the `agent_uses` log: which installed agent
/// definition (by name), at which pinned version, was invoked under an
/// execution — with a short free-text reason when one was available. The
/// timestamp column defaults to the insert time; the ledger drains per
/// execution, so insert time is invocation-accurate to within the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentUseRow {
    pub agent: String,
    pub version: u32,
    pub reason: String,
}

/// One skill-invocation row for the `skill_usage` log — the analogue of
/// [`AgentUseRow`] for the SKILLS tab: which skill (by name) at which pinned
/// version was applied under an execution, with a short reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUsageRow {
    pub skill: String,
    pub version: u32,
    pub reason: String,
}

/// One normalized tool-call row for the `tool_calls` log — a queryable
/// per-call record materialized from the `events` stream. Stores shape,
/// timing, and success (never the full output — `bytes_out` is its size).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRow {
    pub call_id: String,
    pub name: String,
    /// `"native"` | `"mcp"` | `"skill"` | `"agent"`.
    pub surface: String,
    pub args_json: String,
    pub args_digest: String,
    pub reason: String,
    pub ok: bool,
    pub error: String,
    pub bytes_out: i64,
    pub duration_ms: i64,
}

/// The agent's self-review of one turn, tied 1:1 to its execution (and thus to
/// `executions.prompt`). The `produced_output`/`wrote_files`/`truncated`
/// objective companions let the dashboard flag a self-silent, zero-output turn
/// as a failure regardless of the model's own rating.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutionReflectionRow {
    pub prompt: String,
    pub delivered: Option<bool>,
    pub self_rating: Option<i64>,
    pub what_went_well: String,
    pub what_to_improve: String,
    pub critique: String,
    pub produced_output: bool,
    pub wrote_files: bool,
    pub truncated: bool,
}

/// One durable reflection/lesson row. `execution_id` is `None` for cross-turn
/// lessons; `domains` is a JSON array of domain tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionRow {
    pub execution_id: Option<i64>,
    pub kind: String,
    pub content: String,
    pub domains: String,
    pub occurred_at: i64,
}

/// One tracked pull request, as stored ([`Store::upsert_pull_request`]):
/// keyed by URL, carrying the latest observed lifecycle `status` and CI
/// verdict strings, the last-update time in epoch millis, and the session
/// that produced it (when known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRecord {
    pub url: String,
    /// The PR number (e.g. 183 for `…/pull/183`); `None` when the producer
    /// could not parse one from the URL.
    pub number: Option<u64>,
    pub status: String,
    pub ci_status: Option<String>,
    pub updated_at: u64,
    pub session_id: Option<String>,
}

/// One replayable event from a session's cross-execution journal
/// ([`Store::session_events`]): which execution it belongs to, its position
/// in that execution's stream, and the deserialized payload.
#[derive(Debug, Clone)]
pub struct SessionEventRecord {
    pub execution_id: i64,
    pub seq: i64,
    pub event: AgentEvent,
}

/// A session's full event journal, reassembled across every execution
/// stamped with its session id. `skipped` counts rows whose stored JSON no
/// longer parses as an [`AgentEvent`] (streams persisted before a variant
/// existed) — replay proceeds without them instead of failing the read.
#[derive(Debug, Clone, Default)]
pub struct SessionJournal {
    pub events: Vec<SessionEventRecord>,
    pub skipped: usize,
}

/// The protocol's serde snake_case token for a task status — the exact
/// string `tasks.status` stores (e.g. `"in_progress"`).
fn task_status_to_string(status: TaskStatus) -> Result<String> {
    match serde_json::to_value(status) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        Ok(other) => Err(StoreError(format!(
            "task status serialized to non-string JSON: {other}"
        ))),
        Err(e) => Err(StoreError(format!("cannot serialize task status: {e}"))),
    }
}

/// Inverse of [`task_status_to_string`]: parse a stored snake_case token
/// back into the protocol enum.
fn task_status_from_string(s: &str) -> Result<TaskStatus> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| StoreError(format!("unknown task status `{s}`: {e}")))
}

/// One aggregated analytics row per (provider, model): the numbers behind
/// "$-per-resolved-task" receipts, straight from local telemetry.
///
/// Field order is the serialization contract for `stella stats --format
/// json|csv` — append new fields at the end, never reorder.
///
/// `division` is a cost-tier classification *derivable from stored data
/// alone*: provider `local` runs are provably free of API cost (`"off-grid"`);
/// every other provider gets `"-"`. Finer-grained tiers are claims about model
/// class and per-task caps that the store does not record, so they are
/// never inferred here.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageStatsRow {
    pub provider: String,
    pub model: String,
    /// Cost-tier classification: `"off-grid"` for provider `local`, else `"-"`.
    pub division: String,
    /// Executions recorded (any outcome, including still-open ones).
    pub runs: i64,
    /// Executions with outcome `completed` — the "resolved" count.
    pub resolved: i64,
    /// `resolved / runs` (a group always has ≥ 1 run).
    pub resolve_rate: f64,
    /// Sum of `executions.cost_usd` — the authoritative per-run total.
    pub total_cost_usd: f64,
    /// `total_cost_usd / resolved`; `None` when nothing resolved — a
    /// division by zero is never papered over with a fake number.
    pub cost_per_resolved_usd: Option<f64>,
    /// Token sums from `telemetry` (zero when a run recorded none).
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Mean model-call wall time per run: `sum(duration_ms) / runs`.
    pub avg_duration_ms: f64,
}

impl UsageStatsRow {
    /// Cost-tier classification derivable from the provider id alone (see
    /// the struct docs for why only the off-grid tier is ever inferred).
    pub fn division_for_provider(provider: &str) -> &'static str {
        if provider == "local" { "off-grid" } else { "-" }
    }
}

/// Hardening of the workspace `.stella/` directory. The store holds full
/// session transcripts (prompts, tool outputs — which can carry file
/// contents), so:
/// - a directory this call just created gets owner-only permissions on unix,
///   matching the credentials file's 0600-from-birth discipline; a
///   pre-existing directory keeps whatever permissions the user chose. If
///   that chmod FAILS, the error propagates and the store refuses to open:
///   proceeding would write transcripts into a world-readable directory.
///   (The CLI treats a failed open as observability loss — it warns once
///   and the session runs on without persistence.)
/// - a `.gitignore` covering the *generated* artifacts (databases, their WAL
///   siblings, the reflections mining log) is dropped once if absent, so
///   transcripts are never committed and pushed by accident. Deliberately
///   NOT `*`: settings.json, mcp.toml, tools/, skills/ and memories/ are
///   user-authored and meant to be committable. Created with `create_new`
///   so a file that appears concurrently is never truncated (no
///   check-then-write TOCTOU): `AlreadyExists` — pre-existing or racing —
///   means "leave it alone", and any other failure stays best-effort
///   ignored (the DB open right after surfaces a genuinely unusable
///   directory).
fn harden_workspace_dir(dir: &Path, created: bool) -> Result<()> {
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|e| StoreError(format!("cannot inspect {}: {e}", dir.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError(format!(
            "workspace state path {} is not a real directory",
            dir.display()
        )));
    }
    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            StoreError(format!(
                "could not restrict freshly created {}: {e}",
                dir.display()
            ))
        })?;
    }
    ensure_workspace_generated_ignore(dir)?;
    Ok(())
}

pub struct Store {
    conn: Mutex<Connection>,
    /// The workspace root this store was opened for — stashed so a turn's
    /// finalize can roll up into the user-tier `usage.db` (project identity is
    /// path-derived) without threading the root through every call site.
    /// `None` for in-memory/ephemeral stores.
    root: Option<PathBuf>,
}

impl Store {
    /// Open (creating if needed) at `.stella/private/store.db`
    /// and apply the schema.
    pub fn open(workspace_root: &Path) -> Result<Self> {
        let (dir, created) = ensure_workspace_state_dir(workspace_root)?;
        harden_workspace_dir(&dir, created)?;
        let db_path = workspace_private_sqlite_path(workspace_root, "store.db")?;
        Self::init(
            open_private_sqlite(&db_path)?,
            Some(workspace_root.to_path_buf()),
        )
    }

    /// In-memory store — tests and ephemeral runs.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    /// The workspace root this store was opened for, if any.
    pub fn workspace_root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    fn init(conn: Connection, root: Option<PathBuf>) -> Result<Self> {
        // execute_batch tolerates the row PRAGMA journal_mode returns (a
        // plain pragma_update errors on it). WAL means a read-only caller
        // (`stella stats`) is never blocked by a live session's writes.
        // busy_timeout matches the sibling stores (context.db, codegraph.db):
        // without it a second same-workspace session gets an immediate
        // SQLITE_BUSY and its best-effort telemetry writes vanish silently.
        // synchronous=NORMAL is the standard WAL pairing — durability to the
        // last checkpoint rather than one fsync per event insert on the hot
        // render path (matching stella-graph's store).
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )?;
        let store = Self {
            conn: Mutex::new(conn),
            root,
        };
        store.migrate()?;
        enterprise_telemetry::initialize_store_export_schema(&mut store.lock())?;
        Ok(store)
    }

    /// Best-effort: roll this execution up into the per-user `usage.db` at its
    /// default location. A no-op (returns `false`) for in-memory stores or if
    /// the aggregate can't be opened — cross-project usage stats must never
    /// fail a turn. Call AFTER `finish_execution` so the outcome is set.
    pub fn sync_to_usage_default(&self, execution_id: i64) -> bool {
        let Some(root) = self.root.clone() else {
            return false;
        };
        let Ok(usage) = usage::UsageStore::open_default() else {
            return false;
        };
        self.sync_to_usage(execution_id, &root, &usage)
            .unwrap_or(false)
    }

    /// Bring the database to [`SCHEMA_VERSION`]. `PRAGMA user_version` 0 is
    /// both "fresh empty file" and "legacy pre-versioning file",
    /// disambiguated by probing for the store's tables: fresh files get the
    /// latest schema in one transaction and are stamped directly; existing
    /// files run each pending [`MIGRATIONS`] entry in its own transaction
    /// (version stamped inside it — see [`apply_migration`]).
    fn migrate(&self) -> Result<()> {
        let mut conn = self.lock();
        let mut version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            // A downgrade guard, not a formality: older code writing into a
            // newer shape would silently violate whatever invariants the
            // newer schema added.
            return Err(StoreError(format!(
                "store.db is at schema version {version}, but this build only \
                 knows {SCHEMA_VERSION} — your stella binary is out of date, not \
                 the workspace. Upgrade with `brew upgrade stella`, re-run \
                 install.sh, or grab a newer build from \
                 https://github.com/macanderson/stella/releases, then reopen \
                 this workspace."
            )));
        }
        if version == 0 && !any_store_table_exists(&conn)? {
            let tx = conn.transaction()?;
            create_latest_schema(&tx)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            return Ok(());
        }
        while version < SCHEMA_VERSION {
            let target = version + 1;
            // PRAGMA foreign_keys is silently ignored inside a transaction
            // (SQLite pragma docs), so enforcement is suspended out here and
            // restored after commit/rollback — the lang_altertable §7
            // procedure the table-rebuilding migrations rely on.
            conn.pragma_update(None, "foreign_keys", false)?;
            let applied = apply_migration(&mut conn, MIGRATIONS[version as usize], target);
            // Restore enforcement even when the migration failed: this
            // connection lives on serving the session.
            let restored = conn
                .pragma_update(None, "foreign_keys", true)
                .map_err(StoreError::from);
            applied?;
            restored?;
            version = target;
        }
        Ok(())
    }

    /// `pub(crate)`: [`crate::cache_gaps`] is a separate module (split out to
    /// keep this file under its size ratchet) whose `impl Store` block needs
    /// the same connection access every query method here has.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned mutex means a panic mid-write; the connection itself
        // is still usable and refusing all further persistence would turn
        // one bad write into total observability loss.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start an execution record; returns its id.
    pub fn begin_execution(
        &self,
        kind: &str,
        prompt: &str,
        provider: &str,
        model: &str,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO executions (kind, prompt, provider, model, usage_complete, usage_status) \
             VALUES (?, ?, ?, ?, 0, 'pending')",
            params![kind, prompt, provider, model],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Stamp the cross-process session registry id ([`SessionRecord::id`])
    /// onto an execution row — called right after
    /// [`Store::begin_execution`] when the turn runs inside a registered
    /// session, so [`Store::session_events`] can reassemble that session's
    /// full journal across executions.
    pub fn set_execution_session(&self, execution_id: i64, session_id: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE executions SET session_id = ? WHERE id = ?",
            params![session_id, execution_id],
        )?;
        Ok(())
    }

    /// Append one uniquely sequenced event to the execution stream.
    pub fn record_event(&self, execution_id: i64, seq: u64, event: &AgentEvent) -> Result<()> {
        let seq = sqlite_i64("event sequence", seq)?;
        let payload = serde_json::to_string(event).map_err(|e| StoreError(e.to_string()))?;
        // Read the internally-tagged `type` from the parsed value rather than
        // string-scanning for the first `"type":"` literal �� the scan silently
        // yields the wrong tag (or "unknown") if serialization is ever
        // pretty-printed, wrapped, or reordered.
        let event_type = serde_json::from_str::<serde_json::Value>(&payload)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_else(|| "unknown".into());
        self.lock().execute(
            "INSERT INTO events (execution_id, seq, event_type, payload) VALUES (?, ?, ?, ?)",
            params![execution_id, seq, event_type, payload],
        )?;
        Ok(())
    }

    /// Persist one file-touch row per normalized execution path.
    pub fn record_files_touched(&self, execution_id: i64, files: &[FileTouchRow]) -> Result<()> {
        let conn = self.lock();
        for row in files {
            let lines_added = sqlite_i64("file lines added", row.lines_added)?;
            let lines_removed = sqlite_i64("file lines removed", row.lines_removed)?;
            conn.execute(
                "INSERT INTO files_touched \
                 (execution_id, path, ops, lines_added, lines_removed, events) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    execution_id,
                    row.path,
                    row.ops,
                    lines_added,
                    lines_removed,
                    row.events_json,
                ],
            )?;
        }
        Ok(())
    }

    /// Persist one citation row per execution/memory pair.
    pub fn record_memory_citations(
        &self,
        execution_id: i64,
        citations: &[MemoryCitationRow],
    ) -> Result<()> {
        let conn = self.lock();
        for row in citations {
            conn.execute(
                "INSERT INTO memory_citations \
                 (execution_id, memory_id, useful_score, truthful, remark) \
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    execution_id,
                    row.memory_id,
                    row.useful_score,
                    row.truthful,
                    row.remark,
                ],
            )?;
        }
        Ok(())
    }

    /// Record non-aggregated agent invocations from one execution.
    pub fn record_agent_uses(&self, execution_id: i64, uses: &[AgentUseRow]) -> Result<()> {
        let conn = self.lock();
        for row in uses {
            conn.execute(
                "INSERT INTO agent_uses (execution_id, agent, version, reason) \
                 VALUES (?, ?, ?, ?)",
                params![execution_id, row.agent, row.version as i64, row.reason],
            )?;
        }
        Ok(())
    }

    /// Record the skills applied in one execution — one row per skill at its
    /// pinned version, never aggregated (see [`SkillUsageRow`]). The analogue
    /// of [`Self::record_agent_uses`] for the SKILLS tab.
    pub fn record_skill_usage(&self, execution_id: i64, skills: &[SkillUsageRow]) -> Result<()> {
        let conn = self.lock();
        for row in skills {
            conn.execute(
                "INSERT INTO skill_usage (execution_id, skill, version, reason) \
                 VALUES (?, ?, ?, ?)",
                params![execution_id, row.skill, row.version as i64, row.reason],
            )?;
        }
        Ok(())
    }

    /// Per-memory citation aggregates ([`MemoryCitationStats`]) — the data
    /// behind `stella memory` and the rule-promotion eligibility gate. Rows
    /// are ordered most-cited first (ties broken by memory_id, so output is
    /// deterministic).
    ///
    /// Citation order for the positive-streak fold is `(execution_id, rowid)`
    /// ascending: execution ids are AUTOINCREMENT (monotonic across sessions)
    /// and the table is append-only, so this IS chronological order. The fold
    /// itself is [`fold_citation_stats`] — plain logic over owned rows,
    /// boundary-tested directly.
    pub fn memory_citation_stats(&self) -> Result<Vec<MemoryCitationStats>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT memory_id, useful_score, truthful, remark FROM memory_citations
             ORDER BY memory_id ASC, execution_id ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(MemoryCitationRow {
                memory_id: row.get(0)?,
                useful_score: row.get(1)?,
                truthful: row.get(2)?,
                remark: row.get(3)?,
            })
        })?;
        let mut citations = Vec::new();
        for row in rows {
            citations.push(row?);
        }
        let mut stats = fold_citation_stats(&citations);
        stats.sort_by(|a, b| {
            b.citations
                .cmp(&a.citations)
                .then_with(|| a.memory_id.cmp(&b.memory_id))
        });
        Ok(stats)
    }

    /// Public ids (`nod_…`) cited untruthful at least
    /// [`QUARANTINE_NEGATIVES_THRESHOLD`] times and excluded from recall.
    /// A query failure is explicit because an empty set means the query
    /// succeeded and no memories are quarantined.
    pub fn quarantined_memory_ids(&self) -> Result<std::collections::HashSet<String>> {
        Ok(self
            .memory_citation_stats()?
            .into_iter()
            .filter(|s| s.quarantined)
            .map(|s| s.memory_id)
            .collect())
    }

    /// Persist the MCP tool calls recorded during an execution: one row per
    /// call, in drain order. `seq` (the batch index) with UNIQUE
    /// (execution_id, seq) makes re-persisting the same drained batch an error
    /// rather than a silent double-count.
    pub fn record_mcp_usage(&self, execution_id: i64, calls: &[McpUsageRow]) -> Result<()> {
        let conn = self.lock();
        for (seq, row) in calls.iter().enumerate() {
            conn.execute(
                "INSERT INTO mcp_usage \
                 (execution_id, seq, server, tool, reason, called_at_ms) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    execution_id,
                    seq as i64,
                    row.server,
                    row.tool,
                    row.reason,
                    row.called_at_ms,
                ],
            )?;
        }
        Ok(())
    }

    /// Per-(server, tool) MCP usage aggregates ([`McpUsageStat`]) — the data
    /// behind the MCP tab's call counts and `stella mcp usage`. Most-used
    /// first (ties broken by server then tool, so output is deterministic).
    pub fn mcp_usage_stats(&self) -> Result<Vec<McpUsageStat>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT server, tool, reason, called_at_ms FROM mcp_usage \
             ORDER BY called_at_ms ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(McpUsageRow {
                server: row.get(0)?,
                tool: row.get(1)?,
                reason: row.get(2)?,
                called_at_ms: row.get(3)?,
            })
        })?;
        let mut calls = Vec::new();
        for row in rows {
            calls.push(row?);
        }
        Ok(fold_mcp_usage_stats(&calls))
    }

    /// Record the normalized per-call `tool_calls` log for an execution
    /// (materialized from the `events` stream). `seq` is the call's index in
    /// the drained batch; UNIQUE (execution_id, seq) guards double-writes.
    pub fn record_tool_calls(&self, execution_id: i64, calls: &[ToolCallRow]) -> Result<()> {
        let conn = self.lock();
        for (seq, row) in calls.iter().enumerate() {
            conn.execute(
                "INSERT OR REPLACE INTO tool_calls \
                 (execution_id, seq, call_id, name, surface, args_json, args_digest, \
                  reason, ok, error, bytes_out, duration_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    execution_id,
                    seq as i64,
                    row.call_id,
                    row.name,
                    row.surface,
                    row.args_json,
                    row.args_digest,
                    row.reason,
                    row.ok as i64,
                    row.error,
                    row.bytes_out,
                    row.duration_ms,
                ],
            )?;
        }
        Ok(())
    }

    /// Per-tool-name call counts across all executions, most-used first —
    /// the histogram behind "you grep symbols N times but never call
    /// graph_query". Deterministic ordering (count desc, then name).
    pub fn tool_call_name_counts(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT name, COUNT(*) AS n FROM tool_calls \
             GROUP BY name ORDER BY n DESC, name ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Record (or replace) the agent's self-review for one turn, 1:1 with its
    /// execution.
    pub fn record_execution_reflection(
        &self,
        execution_id: i64,
        r: &ExecutionReflectionRow,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO execution_reflection \
             (execution_id, prompt, delivered, self_rating, what_went_well, \
              what_to_improve, critique, produced_output, wrote_files, truncated) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                execution_id,
                r.prompt,
                r.delivered.map(|b| b as i64),
                r.self_rating,
                r.what_went_well,
                r.what_to_improve,
                r.critique,
                r.produced_output as i64,
                r.wrote_files as i64,
                r.truncated as i64,
            ],
        )?;
        Ok(())
    }

    /// Append a durable reflection/lesson, returning its row id.
    pub fn record_reflection(&self, r: &ReflectionRow) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO reflections (execution_id, kind, content, domains, occurred_at) \
             VALUES (?, ?, ?, ?, ?)",
            params![r.execution_id, r.kind, r.content, r.domains, r.occurred_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Materialize the normalized `tool_calls` log for an execution from its
    /// already-persisted `events` stream (`tool_start` + `tool_result`). Rows
    /// are emitted in call order; a `tool_start` with no matching result (turn
    /// cut off mid-tool) is recorded as an incomplete, failed call so the count
    /// stays honest. Idempotent (INSERT OR REPLACE on seq). Returns the count.
    pub fn materialize_tool_calls(&self, execution_id: i64) -> Result<usize> {
        let payloads: Vec<String> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT payload FROM events \
                 WHERE execution_id = ?1 AND event_type IN ('tool_start', 'tool_result') \
                 ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map(params![execution_id], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };

        use std::collections::HashMap;
        let mut order: Vec<String> = Vec::new();
        // call_id -> (name, surface, args_json)
        let mut starts: HashMap<String, (String, String, String)> = HashMap::new();
        // call_id -> (ok, error, bytes_out, duration_ms)
        let mut results: HashMap<String, (bool, String, i64, i64)> = HashMap::new();

        for payload in &payloads {
            let Ok(ev) = serde_json::from_str::<AgentEvent>(payload) else {
                continue;
            };
            match ev {
                AgentEvent::ToolStart { call } => {
                    let args_json =
                        serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".into());
                    let surface = if call.name.starts_with("mcp__") {
                        "mcp"
                    } else {
                        "native"
                    };
                    order.push(call.call_id.clone());
                    starts.insert(call.call_id, (call.name, surface.into(), args_json));
                }
                AgentEvent::ToolResult {
                    call_id,
                    output,
                    duration_ms,
                    ..
                } => {
                    let (ok, error, bytes) = match output {
                        ToolOutput::Ok { content } => (true, String::new(), content.len() as i64),
                        ToolOutput::Error { message } => {
                            let len = message.len() as i64;
                            (false, message, len)
                        }
                    };
                    results.insert(call_id, (ok, error, bytes, duration_ms as i64));
                }
                _ => {}
            }
        }

        let mut rows: Vec<ToolCallRow> = Vec::with_capacity(order.len());
        for call_id in &order {
            let Some((name, surface, args_json)) = starts.get(call_id) else {
                continue;
            };
            let digest = fnv_hex(args_json);
            let (ok, error, bytes_out, duration_ms) = match results.get(call_id) {
                Some((ok, error, bytes, dur)) => (*ok, error.clone(), *bytes, *dur),
                None => (
                    false,
                    "no result (turn ended before the tool returned)".to_string(),
                    0,
                    0,
                ),
            };
            rows.push(ToolCallRow {
                call_id: call_id.clone(),
                name: name.clone(),
                surface: surface.clone(),
                args_json: args_json.clone(),
                args_digest: digest,
                reason: String::new(),
                ok,
                error,
                bytes_out,
                duration_ms,
            });
        }
        let n = rows.len();
        self.record_tool_calls(execution_id, &rows)?;
        Ok(n)
    }

    /// Derive and record the objective half of this turn's
    /// `execution_reflection` — prompt, plus `produced_output` / `wrote_files`
    /// / `truncated` computed from the event and file-touch logs. The model's
    /// self-review fields are left empty here; a producer that captures a
    /// model-emitted self-assessment can `INSERT OR REPLACE` over this row.
    pub fn finalize_execution_reflection(&self, execution_id: i64) -> Result<()> {
        let (prompt, produced_output, wrote_files, truncated) = {
            let conn = self.lock();
            let prompt: String = conn
                .query_row(
                    "SELECT prompt FROM executions WHERE id = ?1",
                    params![execution_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_default();
            let produced_output: bool = conn.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE execution_id = ?1 AND event_type IN ('text', 'tool_start')",
                params![execution_id],
                |r| r.get::<_, i64>(0),
            )? > 0;
            let truncated: bool = conn.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE execution_id = ?1 AND event_type = 'error' \
                   AND (payload LIKE '%output-token limit%' OR payload LIKE '%truncated%')",
                params![execution_id],
                |r| r.get::<_, i64>(0),
            )? > 0;
            let wrote_files: bool = conn.query_row(
                "SELECT COUNT(*) FROM files_touched \
                 WHERE execution_id = ?1 \
                   AND (ops LIKE '%C%' OR ops LIKE '%U%' OR ops LIKE '%D%')",
                params![execution_id],
                |r| r.get::<_, i64>(0),
            )? > 0;
            (prompt, produced_output, wrote_files, truncated)
        };
        self.record_execution_reflection(
            execution_id,
            &ExecutionReflectionRow {
                prompt,
                delivered: None,
                self_rating: None,
                what_went_well: String::new(),
                what_to_improve: String::new(),
                critique: String::new(),
                produced_output,
                wrote_files,
                truncated,
            },
        )
    }

    /// Assemble the user-tier [`usage::ExecutionRollupRow`] for one execution
    /// from this project store (executions + telemetry + tool_calls +
    /// files_touched). Returns `None` if the execution id is unknown. Reads
    /// only — safe for both live finalize and `stella usage sync` backfill.
    pub fn execution_rollup(
        &self,
        execution_id: i64,
        workspace_root: &Path,
    ) -> Result<Option<usage::ExecutionRollupRow>> {
        let conn = self.lock();
        let base = conn
            .query_row(
                "SELECT kind, prompt, provider, model, COALESCE(outcome, ''), cost_usd, started_at, \
                        usage_complete \
                 FROM executions WHERE id = ?1 AND finished_at IS NOT NULL \
                   AND usage_complete = 1 AND usage_status = 'complete'",
                params![execution_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, f64>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, bool>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind, prompt, provider, model, outcome, cost_usd, started_at, usage_complete)) =
            base
        else {
            return Ok(None);
        };
        let (input_tokens, output_tokens, duration_ms): (i64, i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0), \
                    COALESCE(SUM(duration_ms), 0) FROM telemetry WHERE execution_id = ?1",
            params![execution_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let tool_calls: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tool_calls WHERE execution_id = ?1",
            params![execution_id],
            |r| r.get(0),
        )?;
        let files_written: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files_touched WHERE execution_id = ?1 \
               AND (ops LIKE '%C%' OR ops LIKE '%U%' OR ops LIKE '%D%')",
            params![execution_id],
            |r| r.get(0),
        )?;
        let produced_output: bool = conn.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE execution_id = ?1 AND event_type IN ('text', 'tool_start')",
            params![execution_id],
            |r| r.get::<_, i64>(0),
        )? > 0;
        let self_rating: Option<i64> = conn
            .query_row(
                "SELECT self_rating FROM execution_reflection WHERE execution_id = ?1",
                params![execution_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let tool_histogram = {
            let mut stmt = conn.prepare(
                "SELECT name, surface, COUNT(*), \
                        SUM(CASE WHEN ok = 0 THEN 1 ELSE 0 END) \
                 FROM tool_calls WHERE execution_id = ?1 GROUP BY name, surface",
            )?;
            let rows = stmt.query_map(params![execution_id], |r| {
                Ok(usage::ToolBucket {
                    tool: r.get(0)?,
                    surface: r.get(1)?,
                    calls: r.get(2)?,
                    errors: r.get(3)?,
                })
            })?;
            let mut v = Vec::new();
            for row in rows {
                v.push(row?);
            }
            v
        };
        drop(conn);

        let day = started_at.get(0..10).unwrap_or("").to_string();
        let project_root = workspace_root.to_string_lossy().to_string();
        let project_name = workspace_root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".into());
        Ok(Some(usage::ExecutionRollupRow {
            project_id: usage::project_id_for(workspace_root),
            project_name,
            project_root,
            execution_id,
            kind,
            prompt_digest: fnv_hex(&prompt),
            prompt_preview: prompt.chars().take(120).collect(),
            model,
            provider,
            outcome,
            cost_usd,
            input_tokens,
            output_tokens,
            duration_ms,
            tool_calls,
            files_written,
            produced_output,
            usage_complete,
            self_rating,
            started_at,
            day,
            tool_histogram,
        }))
    }

    /// Roll one execution up into the user-tier aggregate. Best-effort: a
    /// missing execution returns `Ok(false)` and never fails a turn.
    pub fn sync_to_usage(
        &self,
        execution_id: i64,
        workspace_root: &Path,
        usage: &usage::UsageStore,
    ) -> Result<bool> {
        match self.execution_rollup(execution_id, workspace_root)? {
            Some(rollup) if rollup.usage_complete => {
                usage.sync_execution(&rollup)?;
                Ok(true)
            }
            Some(_) | None => Ok(false),
        }
    }

    /// Mirror one task-board snapshot into `tasks`: every item is upserted
    /// on (session_id, task_id), so the table always holds each task's
    /// LATEST state (the `events` stream keeps the full snapshot history).
    /// `status`/`owner` are stored as the protocol's serde snake_case
    /// strings (e.g. `"in_progress"`); `description` is stored as-is.
    /// NOTE: with `session_id` `None` the UNIQUE key never conflicts (SQL
    /// NULLs are pairwise distinct), so session-less rows append rather than
    /// replace — dedup is a per-session guarantee.
    pub fn record_task_board(
        &self,
        execution_id: i64,
        session_id: Option<&str>,
        tasks: &[TaskItem],
        now_ms: u64,
    ) -> Result<()> {
        let conn = self.lock();
        for item in tasks {
            let status = task_status_to_string(item.status)?;
            conn.execute(
                "INSERT INTO tasks \
                 (execution_id, session_id, task_id, subject, description, status, owner, \
                  updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (session_id, task_id) DO UPDATE SET \
                 execution_id = excluded.execution_id, subject = excluded.subject, \
                 description = excluded.description, status = excluded.status, \
                 owner = excluded.owner, updated_at = excluded.updated_at",
                params![
                    execution_id,
                    session_id,
                    item.id,
                    item.subject,
                    item.description,
                    status,
                    item.owner,
                    now_ms as i64,
                ],
            )?;
        }
        Ok(())
    }

    /// Read back a session's task board — each task's latest recorded state,
    /// ordered numerically by the board's ordinal task id (`CAST(task_id AS
    /// INTEGER)`, so "10" sorts after "2"). A stored status that no longer
    /// parses as a [`TaskStatus`] is an error: the store wrote it, so a bad
    /// token is corruption, not drift.
    pub fn list_session_tasks(&self, session_id: &str) -> Result<Vec<TaskItem>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT task_id, subject, description, status, owner FROM tasks \
             WHERE session_id = ? ORDER BY CAST(task_id AS INTEGER) ASC, task_id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut board = Vec::new();
        for row in rows {
            let (task_id, subject, description, status, owner) = row?;
            board.push(TaskItem {
                id: task_id,
                subject,
                description,
                status: task_status_from_string(&status)?,
                owner,
            });
        }
        Ok(board)
    }

    /// Upsert one tracked pull request, keyed by URL: a later observation of
    /// the same PR updates its `number`/`status`/`ci_status`/`updated_at` in
    /// place. A `session_id` of `None` never ERASES an existing session
    /// link (COALESCE keeps the stored value) — a monitor that only knows
    /// the URL must not orphan the PR from its producing session.
    pub fn upsert_pull_request(
        &self,
        session_id: Option<&str>,
        url: &str,
        number: Option<u64>,
        status: &str,
        ci_status: Option<&str>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO pull_requests (session_id, url, number, status, ci_status, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (url) DO UPDATE SET \
             session_id = COALESCE(excluded.session_id, session_id), \
             number = excluded.number, status = excluded.status, \
             ci_status = excluded.ci_status, updated_at = excluded.updated_at",
            params![
                session_id,
                url,
                number.map(|n| n as i64),
                status,
                ci_status,
                now_ms as i64,
            ],
        )?;
        Ok(())
    }

    /// Tracked pull requests, freshest first (ties broken by URL for
    /// determinism). `Some(session_id)` filters to one session's PRs;
    /// `None` returns them all.
    pub fn list_pull_requests(&self, session_id: Option<&str>) -> Result<Vec<PullRequestRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT url, number, status, ci_status, updated_at, session_id \
             FROM pull_requests \
             WHERE ?1 IS NULL OR session_id = ?1 \
             ORDER BY updated_at DESC, url ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(PullRequestRecord {
                url: row.get(0)?,
                number: row.get::<_, Option<i64>>(1)?.map(|n| n as u64),
                status: row.get(2)?,
                ci_status: row.get(3)?,
                updated_at: row.get::<_, i64>(4)? as u64,
                session_id: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The replay reader: a session's COMPLETE event journal, reassembled
    /// across every execution stamped with `session_id`
    /// ([`Store::set_execution_session`]), ordered by (execution_id, seq) —
    /// execution ids are AUTOINCREMENT, so this is turn order, and seq is
    /// stream order within a turn. A row whose stored payload no longer
    /// parses as an [`AgentEvent`] (an old stream predating a variant) is
    /// SKIPPED and counted in [`SessionJournal::skipped`] rather than
    /// failing the whole read.
    pub fn session_events(&self, session_id: &str) -> Result<SessionJournal> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.execution_id, e.seq, e.payload FROM events e \
             JOIN executions x ON x.id = e.execution_id \
             WHERE x.session_id = ? \
             ORDER BY e.execution_id ASC, e.seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut journal = SessionJournal::default();
        for row in rows {
            let (execution_id, seq, payload) = row?;
            match serde_json::from_str::<AgentEvent>(&payload) {
                Ok(event) => journal.events.push(SessionEventRecord {
                    execution_id,
                    seq,
                    event,
                }),
                Err(_) => journal.skipped += 1,
            }
        }
        Ok(journal)
    }

    /// Cooperative file lock: succeeds only if `path` is unclaimed or
    /// already held by `holder` (re-entrant). Returns whether the lock is
    /// now held.
    ///
    /// `INSERT … DO NOTHING` + read-back rather than check-then-insert: two
    /// PROCESSES racing for the same path (two fleet runs in one workspace)
    /// resolve to one winner — the losing INSERT is a no-op, never a
    /// primary-key error — and the read-back reports honestly for both.
    pub fn acquire_file_lock(&self, path: &str, holder: &str) -> Result<bool> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO file_locks (path, holder) VALUES (?, ?) \
             ON CONFLICT (path) DO NOTHING",
            params![path, holder],
        )?;
        // A row that vanished between the two statements means the competing
        // holder released cross-process in that window; "not held" is the
        // conservative truthful answer for THIS call.
        let current: Option<String> = conn
            .query_row(
                "SELECT holder FROM file_locks WHERE path = ?",
                params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(current.as_deref() == Some(holder))
    }

    /// Who currently holds the claim on `path`, if anyone — the read half a
    /// consumer uses to NAME the conflicting holder in its error.
    pub fn file_lock_holder(&self, path: &str) -> Result<Option<String>> {
        self.lock()
            .query_row(
                "SELECT holder FROM file_locks WHERE path = ?",
                params![path],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Release a lock (only the holder's release removes it).
    pub fn release_file_lock(&self, path: &str, holder: &str) -> Result<()> {
        self.lock().execute(
            "DELETE FROM file_locks WHERE path = ? AND holder = ?",
            params![path, holder],
        )?;
        Ok(())
    }

    /// Release every lock `holder` still holds — one statement for the whole
    /// claim set, so a worker/session ending (or its supervisor cleaning up
    /// after it) never has to enumerate what was claimed. Returns how many
    /// locks were released.
    pub fn release_file_locks_for_holder(&self, holder: &str) -> Result<usize> {
        let released = self
            .lock()
            .execute("DELETE FROM file_locks WHERE holder = ?", params![holder])?;
        Ok(released)
    }

    /// Crash hygiene: release claims older than `max_age_secs` regardless of
    /// holder. A crashed process cannot release its own claims, and a stale
    /// claim would block every future writer of that path forever — age is
    /// the one signal that works across processes and reboots (mirroring the
    /// session registry's dead-pid downgrade). Returns how many were swept.
    pub fn prune_stale_file_locks(&self, max_age_secs: u64) -> Result<usize> {
        let swept = self.lock().execute(
            "DELETE FROM file_locks WHERE acquired_at < datetime('now', ?)",
            params![format!("-{max_age_secs} seconds")],
        )?;
        Ok(swept)
    }

    /// Upsert one extension-authored workspace rule — the write seam an
    /// extension provider uses to publish a rule without touching
    /// `.stella/rules/`. `contents` is the full rule markdown in the
    /// authoring format `stella_core::rules::rule_from_file` parses; the
    /// store treats it as opaque text. Re-publishing an existing `rule_id`
    /// replaces its contents and source and bumps `updated_at`.
    pub fn upsert_rule(&self, rule_id: &str, contents: &str, source: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO rules (rule_id, contents, source) VALUES (?, ?, ?) \
             ON CONFLICT (rule_id) DO UPDATE SET contents = excluded.contents, \
             source = excluded.source, updated_at = CURRENT_TIMESTAMP",
            params![rule_id, contents, source],
        )?;
        Ok(())
    }

    /// Delete one extension-authored rule; returns whether a row existed.
    pub fn delete_rule(&self, rule_id: &str) -> Result<bool> {
        let deleted = self
            .lock()
            .execute("DELETE FROM rules WHERE rule_id = ?", params![rule_id])?;
        Ok(deleted > 0)
    }

    /// Every stored rule, ordered by rule id — deterministic, so the rules
    /// section assembled from these into a session's system prompt stays
    /// byte-stable (the prompt-cache contract).
    pub fn list_rules(&self) -> Result<Vec<RuleRow>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT rule_id, contents, source FROM rules ORDER BY rule_id")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(RuleRow {
                    rule_id: row.get(0)?,
                    contents: row.get(1)?,
                    source: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Upsert a graph node — the context plane's write seam.
    ///
    /// `properties` must be a valid JSON document. The old DuckDB backend
    /// stored it in a JSON-typed column that rejected malformed input at the
    /// DB boundary; SQLite's `TEXT` column would silently accept arbitrary
    /// bytes, so the JSON-validity invariant is re-asserted here before the
    /// INSERT rather than let unparseable data reach JSON-expecting readers.
    pub fn upsert_graph_node(&self, id: &str, label: &str, properties: &str) -> Result<()> {
        validate_json_properties(properties)?;
        self.lock().execute(
            "INSERT INTO graph_nodes (id, label, properties) VALUES (?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET label = excluded.label, \
             properties = excluded.properties",
            params![id, label, properties],
        )?;
        Ok(())
    }

    /// Insert a graph edge.
    ///
    /// `properties` must be a valid JSON document — see [`Store::upsert_graph_node`]
    /// for why the invariant is enforced here rather than by the column type.
    pub fn insert_graph_edge(
        &self,
        src: &str,
        dst: &str,
        edge_type: &str,
        properties: &str,
    ) -> Result<()> {
        validate_json_properties(properties)?;
        self.lock().execute(
            "INSERT INTO graph_edges (src, dst, edge_type, properties) VALUES (?, ?, ?, ?)",
            params![src, dst, edge_type, properties],
        )?;
        Ok(())
    }

    /// Count helper — currently exercised only by tests; kept `pub` for
    /// ad-hoc introspection.
    pub fn count(&self, table: &str) -> Result<i64> {
        // Table names can't be bound parameters; allowlist them.
        if !TABLES.contains(&table) {
            return Err(StoreError(format!("unknown table `{table}`")));
        }
        let count: i64 =
            self.lock()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
        Ok(count)
    }

    /// Aggregate usage/cost analytics per (provider, model) — the data
    /// behind `stella stats` and every "$-per-resolved-task" receipt.
    ///
    /// Semantics:
    /// - One output row per distinct `executions.(provider, model)` pair;
    ///   telemetry is attributed to its execution's provider/model.
    /// - `runs` counts every execution; `resolved` counts
    ///   `outcome = 'completed'` (aborted and still-open runs are not
    ///   resolved).
    /// - Cost comes from `executions.cost_usd` (the per-run total written
    ///   at finish); token and duration sums come from `telemetry`,
    ///   pre-aggregated per execution before the join so a multi-step run
    ///   can never fan out the executions side.
    /// - `cost_per_resolved_usd` is `None` when `resolved = 0`.
    /// - Rows are ordered by total cost descending (ties broken by
    ///   provider, then model, so output is deterministic).
    ///
    /// Division mapping: only provider `local` maps to the off-grid cost
    /// tier (`off-grid`); all other rows carry `"-"` — see [`UsageStatsRow`].
    pub fn usage_stats(&self) -> Result<Vec<UsageStatsRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.provider,
                    e.model,
                    count(*) AS runs,
                    count(*) FILTER (WHERE e.outcome = 'completed') AS resolved,
                    coalesce(sum(e.cost_usd), 0) AS total_cost_usd,
                    CAST(coalesce(sum(t.input_tokens), 0) AS INTEGER) AS input_tokens,
                    CAST(coalesce(sum(t.output_tokens), 0) AS INTEGER) AS output_tokens,
                    CAST(coalesce(sum(t.cache_read_tokens), 0) AS INTEGER) AS cache_read_tokens,
                    CAST(coalesce(sum(t.cache_write_tokens), 0) AS INTEGER) AS cache_write_tokens,
                    CAST(coalesce(sum(t.duration_ms), 0) AS INTEGER) AS total_duration_ms
             FROM executions e
             LEFT JOIN (
               SELECT execution_id,
                      sum(input_tokens) AS input_tokens,
                      sum(output_tokens) AS output_tokens,
                      sum(cache_read_tokens) AS cache_read_tokens,
                      sum(cache_write_tokens) AS cache_write_tokens,
                      sum(duration_ms) AS duration_ms
               FROM telemetry
               GROUP BY execution_id
             ) t ON t.execution_id = e.id
             GROUP BY e.provider, e.model
             ORDER BY total_cost_usd DESC, e.provider ASC, e.model ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let provider: String = row.get(0)?;
            let model: String = row.get(1)?;
            let runs: i64 = row.get(2)?;
            let resolved: i64 = row.get(3)?;
            let total_cost_usd: f64 = row.get(4)?;
            let input_tokens: i64 = row.get(5)?;
            let output_tokens: i64 = row.get(6)?;
            let cache_read_tokens: i64 = row.get(7)?;
            let cache_write_tokens: i64 = row.get(8)?;
            let total_duration_ms: i64 = row.get(9)?;
            let division = UsageStatsRow::division_for_provider(&provider).to_string();
            Ok(UsageStatsRow {
                provider,
                model,
                division,
                runs,
                resolved,
                // A GROUP BY group always holds ≥ 1 execution row.
                resolve_rate: resolved as f64 / runs as f64,
                total_cost_usd,
                cost_per_resolved_usd: (resolved > 0).then(|| total_cost_usd / resolved as f64),
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                avg_duration_ms: total_duration_ms as f64 / runs as f64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Dump every table's raw rows as JSON arrays — the data behind
    /// `/export`. Each entry is `(table_name, json_array_string)`. The JSON is
    /// constructed in Rust (not by SQLite's json1 extension, which may be
    /// absent from a bundled build), so the shape is stable across platforms.
    /// Rows are ordered by the table's natural key; the exact order per table
    /// is documented in the module-level schema comments.
    pub fn export_all_json(&self) -> Result<Vec<(&'static str, String)>> {
        let conn = self.lock();
        let mut out: Vec<(&'static str, String)> = Vec::new();

        // Executions — the spine everything else keys off.
        {
            let mut stmt = conn.prepare(
                "SELECT id, kind, prompt, provider, model, started_at, finished_at, outcome, \
                 cost_usd, usage_complete FROM executions ORDER BY id ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, i64>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "prompt": row.get::<_, String>(2)?,
                    "provider": row.get::<_, String>(3)?,
                    "model": row.get::<_, String>(4)?,
                    "started_at": row.get::<_, Option<String>>(5)?,
                    "finished_at": row.get::<_, Option<String>>(6)?,
                    "outcome": row.get::<_, Option<String>>(7)?,
                    "cost_usd": row.get::<_, f64>(8)?,
                    "usage_complete": row.get::<_, bool>(9)?,
                }))
            })?;
            let mut arr = Vec::new();
            for r in rows {
                arr.push(r?);
            }
            out.push((
                "executions",
                serde_json::to_string(&arr).unwrap_or_default(),
            ));
        }

        // Telemetry — per-model-call token/cost/duration rows.
        for (name, sql) in [
            (
                "telemetry",
                "SELECT execution_id, step, ts, provider, call_role, model, input_tokens, \
                 estimated_input_tokens, output_tokens, cache_read_tokens, cache_miss_tokens, \
                 cache_write_tokens, cost_usd, duration_ms, retries, tool_calls, usage_complete \
                 FROM telemetry \
                 ORDER BY execution_id ASC, step ASC"
                    .to_string(),
            ),
            (
                "tool_calls",
                "SELECT execution_id, seq, call_id, name, surface, args_json, args_digest, \
                 reason, ok, error, bytes_out, duration_ms FROM tool_calls ORDER BY execution_id \
                 ASC, seq ASC"
                    .to_string(),
            ),
            (
                "files_touched",
                "SELECT execution_id, path, ops, lines_added, lines_removed, events FROM \
                 files_touched ORDER BY execution_id ASC, path ASC"
                    .to_string(),
            ),
            (
                "mcp_usage",
                "SELECT execution_id, server, tool, reason, called_at_ms FROM mcp_usage ORDER BY \
                 called_at_ms ASC"
                    .to_string(),
            ),
            (
                "agent_uses",
                "SELECT execution_id, agent, version, reason, ts FROM agent_uses ORDER BY \
                 execution_id ASC, ts ASC"
                    .to_string(),
            ),
            (
                "skill_usage",
                "SELECT execution_id, skill, version, reason, ts FROM skill_usage ORDER BY \
                 execution_id ASC, ts ASC"
                    .to_string(),
            ),
            (
                "execution_reflection",
                "SELECT execution_id, prompt, delivered, self_rating, what_went_well, \
                 what_to_improve, critique, produced_output, wrote_files, truncated FROM \
                 execution_reflection ORDER BY execution_id ASC"
                    .to_string(),
            ),
            (
                "reflections",
                "SELECT id, execution_id, kind, content, domains, occurred_at FROM reflections \
                 ORDER BY id ASC"
                    .to_string(),
            ),
        ] {
            let arr = self.query_to_json(&conn, &sql)?;
            out.push((name, arr));
        }

        Ok(out)
    }

    /// Execute a `SELECT *`-style query and return a JSON array string, one
    /// object per row. Column names come from the query cursor. Used by
    /// [`export_all_json`] for the uniform tables.
    fn query_to_json(&self, conn: &Connection, sql: &str) -> Result<String> {
        let mut stmt = conn.prepare(sql)?;
        let col_count = stmt.column_count();
        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let rows = stmt.query_map([], |row| {
            let mut obj = serde_json::Map::with_capacity(col_count);
            for (i, name) in col_names.iter().enumerate() {
                // sqlite_type → JSON type: text→string, integer→number,
                // real→number, null/blob→null-or-string. rusqlite's
                // value_ref covers all of them.
                use rusqlite::types::ValueRef;
                let val = match row.get_ref(i)? {
                    ValueRef::Null => serde_json::Value::Null,
                    ValueRef::Integer(n) => serde_json::Value::Number(n.into()),
                    ValueRef::Real(f) => serde_json::Number::from_f64(f)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null),
                    ValueRef::Text(bytes) => {
                        serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())
                    }
                    ValueRef::Blob(bytes) => serde_json::Value::String(
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    ),
                };
                obj.insert(name.clone(), val);
            }
            Ok(serde_json::Value::Object(obj))
        })?;
        let mut arr = Vec::new();
        for r in rows {
            arr.push(r?);
        }
        Ok(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into()))
    }

    /// The timestamp of the most recent log entry across all tables — the
    /// "as-of" watermark for the export. Returns the max of: executions
    /// `finished_at`/`started_at`, telemetry `ts`, reflections `occurred_at`,
    /// and mcp_usage `called_at_ms`. Returns `None` when the store is empty.
    pub fn last_log_timestamp(&self) -> Result<Option<String>> {
        let conn = self.lock();
        // Try the most reliable chronological markers in priority order.
        let candidates: [&str; 4] = [
            "SELECT MAX(ts) FROM telemetry",
            "SELECT MAX(started_at) FROM executions",
            "SELECT MAX(finished_at) FROM executions WHERE finished_at IS NOT NULL",
            "SELECT datetime(MAX(called_at_ms) / 1000, 'unixepoch') FROM mcp_usage",
        ];
        let mut latest: Option<String> = None;
        for sql in candidates {
            let row: rusqlite::Result<Option<String>> = conn.query_row(sql, [], |row| row.get(0));
            if let Ok(Some(ts)) = row
                && !ts.is_empty()
            {
                latest = Some(match latest {
                    Some(prev) if prev >= ts => prev,
                    _ => ts,
                });
            }
        }
        Ok(latest)
    }
}

/// Fold chronologically-ordered MCP usage rows into per-(server, tool)
/// aggregates: a call count, the most recent non-empty reason, and the latest
/// call time. Input is expected in ascending call-time order (the shape
/// [`Store::mcp_usage_stats`]'s query produces); output is most-used first,
/// ties broken by server then tool for determinism.
pub fn fold_mcp_usage_stats(rows: &[McpUsageRow]) -> Vec<McpUsageStat> {
    use std::collections::BTreeMap;
    let mut by_key: BTreeMap<(String, String), McpUsageStat> = BTreeMap::new();
    for row in rows {
        let entry = by_key
            .entry((row.server.clone(), row.tool.clone()))
            .or_insert_with(|| McpUsageStat {
                server: row.server.clone(),
                tool: row.tool.clone(),
                calls: 0,
                last_reason: String::new(),
                last_called_at_ms: 0,
            });
        entry.calls += 1;
        // Rows arrive in ascending time order, so the last non-empty reason
        // seen is the most recent one.
        if !row.reason.is_empty() {
            entry.last_reason = row.reason.clone();
        }
        entry.last_called_at_ms = entry.last_called_at_ms.max(row.called_at_ms);
    }
    let mut stats: Vec<McpUsageStat> = by_key.into_values().collect();
    stats.sort_by(|a, b| {
        b.calls
            .cmp(&a.calls)
            .then_with(|| a.server.cmp(&b.server))
            .then_with(|| a.tool.cmp(&b.tool))
    });
    stats
}

/// Fold chronologically-ordered citations (grouped by `memory_id` — the shape
/// [`Store::memory_citation_stats`]'s query produces) into per-memory
/// aggregates. The positive streak counts consecutive positive citations from
/// the end of each memory's history back to (not through) its most recent
/// negative one; eligibility is a STRICT `> PROMOTION_CITATIONS_REQUIRED` on
/// that streak, so one negative remark disqualifies the memory until it
/// re-earns more than 10 fresh all-positive citations.
pub fn fold_citation_stats(rows: &[MemoryCitationRow]) -> Vec<MemoryCitationStats> {
    let mut stats: Vec<MemoryCitationStats> = Vec::new();
    let mut score_sum: i64 = 0;
    let mut truthful_count: i64 = 0;
    let mut untruthful_count: i64 = 0;

    for row in rows {
        let fresh = stats.last().is_none_or(|s| s.memory_id != row.memory_id);
        if fresh {
            score_sum = 0;
            truthful_count = 0;
            untruthful_count = 0;
            stats.push(MemoryCitationStats {
                memory_id: row.memory_id.clone(),
                citations: 0,
                avg_score: 0.0,
                truthful_rate: 0.0,
                negatives: 0,
                positive_streak: 0,
                eligible: false,
                quarantined: false,
            });
        }
        // `stats` is non-empty from here on (a fresh group just pushed).
        let entry = stats.last_mut().expect("group entry just ensured");
        entry.citations += 1;
        score_sum += row.useful_score;
        truthful_count += i64::from(row.truthful);
        if !row.truthful {
            untruthful_count += 1;
        }
        if row.is_positive() {
            entry.positive_streak += 1;
        } else {
            entry.negatives += 1;
            entry.positive_streak = 0;
        }
        entry.avg_score = score_sum as f64 / entry.citations as f64;
        entry.truthful_rate = truthful_count as f64 / entry.citations as f64;
        entry.eligible = entry.positive_streak > PROMOTION_CITATIONS_REQUIRED;
        // Quarantine is driven by UNTRUTHFUL citations only (Proposal 3) — a
        // memory verified wrong ≥ threshold times is excluded from recall.
        // A low-usefulness-but-truthful citation does not quarantine; it only
        // affects promotion eligibility via the negative streak.
        entry.quarantined = untruthful_count >= QUARANTINE_NEGATIVES_THRESHOLD;
    }
    stats
}
