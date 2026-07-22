//! Every table and index the store owns, as DDL at the current
//! [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — the one place the
//! CURRENT shape of `.stella/private/store.db` is written down. Fresh databases get
//! this whole schema in one shot
//! ([`create_latest_schema`](crate::migrations::create_latest_schema));
//! existing files reach the same shape table by table through
//! [`crate::migrations`] — which is why most statements carry
//! `IF NOT EXISTS` (one batch serves both the fresh path and an additive
//! migration) and three are name-parameterized functions (the
//! lang_altertable §7 table rebuilds create the new shape under a scratch
//! name first).

/// Every table the store owns — the allowlist for [`Store::count`](crate::Store::count) and the
/// fresh-file probe in [`Store::migrate`](crate::Store::migrate).
pub(crate) const TABLES: [&str; 17] = [
    "executions",
    "events",
    "telemetry",
    "files_touched",
    "memory_citations",
    "rules",
    "mcp_usage",
    "file_locks",
    "graph_nodes",
    "graph_edges",
    "agent_uses",
    "skill_usage",
    "tool_calls",
    "execution_reflection",
    "reflections",
    "tasks",
    "pull_requests",
];

/// `executions` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — the spine every other table
/// keys off, one row per run/goal/turn. `session_id` (v8) is the nullable
/// cross-process session registry id ([`SessionRecord::id`](crate::SessionRecord::id)) stamped by
/// [`Store::set_execution_session`](crate::Store::set_execution_session) right after the row is opened, linking
/// per-turn executions back to their session so
/// [`Store::session_events`](crate::Store::session_events) can reassemble the full journal; NULL for rows
/// persisted before v8 or for runs outside a registered session. The
/// by-session index is that reader's access path (filter on session_id,
/// scan in id order). `IF NOT EXISTS` on both so the batch also tolerates a
/// partial file that already grew them.
pub(crate) const EXECUTIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS executions (
       id INTEGER PRIMARY KEY AUTOINCREMENT,
       kind TEXT NOT NULL,
       prompt TEXT NOT NULL,
       provider TEXT NOT NULL,
       model TEXT NOT NULL,
       started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
       finished_at TEXT,
       outcome TEXT,
       cost_usd REAL NOT NULL DEFAULT 0,
       session_id TEXT,
       usage_complete INTEGER NOT NULL DEFAULT 0 CHECK(usage_complete IN (0, 1)),
       usage_status TEXT NOT NULL DEFAULT 'pending'
         CHECK(usage_status IN ('pending', 'complete', 'incomplete'))
     );
     CREATE INDEX IF NOT EXISTS executions_by_session
       ON executions(session_id, id);";

/// Tables whose shape has not changed since v0. `IF NOT EXISTS` keeps one
/// batch usable both for fresh files and for filling gaps in partial legacy
/// files (a v0 file only holds what its era's code created).
pub(crate) const UNCHANGED_TABLES: &str = "CREATE TABLE IF NOT EXISTS file_locks (
       path TEXT PRIMARY KEY,
       holder TEXT NOT NULL,
       acquired_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
     );
     CREATE TABLE IF NOT EXISTS graph_nodes (
       id TEXT PRIMARY KEY,
       label TEXT NOT NULL,
       properties TEXT NOT NULL DEFAULT '{}'
     );
     CREATE TABLE IF NOT EXISTS graph_edges (
       src TEXT NOT NULL,
       dst TEXT NOT NULL,
       edge_type TEXT NOT NULL,
       properties TEXT NOT NULL DEFAULT '{}'
     );";

/// `events` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION), parameterized over the table name
/// because the v0 → v1 rebuild first creates it under a scratch name.
///
/// UNIQUE (execution_id, seq): one row per position in an execution's event
/// stream. The drain loop owns a monotonically increasing `seq` per
/// execution and replay reads `(execution_id, seq)` back in order, so a
/// duplicate position is a double-write, not data — the constraint turns it
/// into an error instead of a silently corrupted replay. Its implicit index
/// is exactly the replay access path (superseding the pre-v1 non-unique
/// `events_by_execution` index, which is why no separate index exists).
pub(crate) fn events_ddl(table: &str) -> String {
    format!(
        "CREATE TABLE {table} (
           execution_id INTEGER NOT NULL,
           seq INTEGER NOT NULL,
           ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
           event_type TEXT NOT NULL,
           payload TEXT NOT NULL,
           UNIQUE (execution_id, seq)
         );"
    )
}

/// `files_touched` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — see [`events_ddl`] for why it
/// is name-parameterized.
///
/// UNIQUE (execution_id, path): one session record per normalized path —
/// the ledger aggregates every touch of a file into one record before
/// persisting, so a duplicate path is a double-write, not data. `events` is
/// the ordered JSON audit log (`[{event, reason, lines_added,
/// lines_removed}, …]`); rows persisted before v2 carry the backfill
/// defaults (zero deltas, empty log).
pub(crate) fn files_touched_ddl(table: &str) -> String {
    format!(
        "CREATE TABLE {table} (
           execution_id INTEGER NOT NULL,
           path TEXT NOT NULL,
           ops TEXT NOT NULL,
           lines_added INTEGER NOT NULL DEFAULT 0,
           lines_removed INTEGER NOT NULL DEFAULT 0,
           events TEXT NOT NULL DEFAULT '[]',
           UNIQUE (execution_id, path)
         );"
    )
}

/// `telemetry` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — see [`events_ddl`] for why it is
/// name-parameterized.
///
/// UNIQUE (execution_id, step): one row per committed model call —
/// `StepUsage` is emitted exactly once per step that lands. `drift_samples`
/// treats `(execution_id, step)` as insertion order and `usage_stats` sums
/// tokens/cost per execution, so a duplicate step double-counts money.
pub(crate) fn telemetry_ddl(table: &str) -> String {
    format!(
        "CREATE TABLE {table} (
           execution_id INTEGER NOT NULL,
           step INTEGER NOT NULL,
           ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
           provider TEXT NOT NULL,
           call_role TEXT NOT NULL DEFAULT 'unknown',
           model TEXT NOT NULL,
           input_tokens INTEGER NOT NULL,
           estimated_input_tokens INTEGER NOT NULL DEFAULT 0,
           output_tokens INTEGER NOT NULL,
           cache_read_tokens INTEGER NOT NULL,
           cache_miss_tokens INTEGER NOT NULL,
           cache_write_tokens INTEGER NOT NULL DEFAULT 0,
           cost_usd REAL NOT NULL,
           duration_ms INTEGER NOT NULL,
           retries INTEGER NOT NULL,
           tool_calls INTEGER NOT NULL,
           usage_complete INTEGER NOT NULL DEFAULT 0 CHECK(usage_complete IN (0, 1)),
           UNIQUE (execution_id, step)
         );"
    )
}

/// `rules` DDL — one row per extension-authored workspace rule, keyed by
/// rule id (the analog of a rule file's filename stem). `contents` is the
/// FULL rule markdown in the `.stella/rules/*.md` authoring format
/// (optional `---` frontmatter — `description:`/`guard-*:` keys — plus the
/// rule statement body); the store never parses it, `stella_core::rules`
/// does. `source` is an opaque label naming the writer (extension/provider
/// id). `IF NOT EXISTS` so one batch serves both the fresh-file schema and
/// the v2 → v3 migration.
pub(crate) const RULES_TABLE: &str = "CREATE TABLE IF NOT EXISTS rules (
       rule_id TEXT PRIMARY KEY,
       contents TEXT NOT NULL,
       source TEXT NOT NULL,
       created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
       updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
     );";

/// `drift_samples` filters (provider, model) and sorts (execution_id DESC,
/// step DESC) at EVERY session start, over a table that grows one row per
/// model call forever — without this index it full-scans. Non-unique on
/// purpose: uniqueness lives on the (execution_id, step) key; this is the
/// query's covering access path.
pub(crate) const TELEMETRY_INDEX: &str = "CREATE INDEX IF NOT EXISTS telemetry_by_model
       ON telemetry(provider, model, execution_id, step);";

/// `memory_citations` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION).
///
/// UNIQUE (execution_id, memory_id): one citation per memory per execution —
/// the session ledger keeps only the model's latest judgment of a memory
/// before persisting, so a duplicate pair is a double-write, not data.
/// `truthful` is 0/1. The by-memory index is the access path of
/// [`Store::memory_citation_stats`](crate::Store::memory_citation_stats), which scans per memory in citation
/// order; the UNIQUE key's implicit (execution_id, …) index can't serve it.
pub(crate) const MEMORY_CITATIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS memory_citations (
       execution_id INTEGER NOT NULL,
       memory_id TEXT NOT NULL,
       ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
       useful_score INTEGER NOT NULL,
       truthful INTEGER NOT NULL,
       remark TEXT NOT NULL DEFAULT '',
       UNIQUE (execution_id, memory_id)
     );
     CREATE INDEX IF NOT EXISTS memory_citations_by_memory
       ON memory_citations(memory_id, execution_id);";

/// `agent_uses` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — the agent-invocation log
/// ([`AgentUseRow`](crate::AgentUseRow)): one row per invocation of an installed agent
/// definition, attributed to the execution it ran under and to the
/// definition's pinned `version` at invocation time. Deliberately **not**
/// UNIQUE on any key: invoking the same agent-version twice in one execution
/// is two real events, and the drain-per-execution write path never
/// double-writes a drained event. `IF NOT EXISTS` keeps the one DDL usable
/// for both the fresh-file path and the additive v3 → v4 migration.
pub(crate) const AGENT_USES_DDL: &str = "CREATE TABLE IF NOT EXISTS agent_uses (
       execution_id INTEGER NOT NULL,
       agent TEXT NOT NULL,
       version INTEGER NOT NULL,
       reason TEXT NOT NULL DEFAULT '',
       ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
     );
     CREATE INDEX IF NOT EXISTS agent_uses_by_agent
       ON agent_uses(agent, version, execution_id);";

/// `skill_usage` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — per-execution skill-version
/// invocation telemetry (SKILLS tab), the exact analogue of [`AGENT_USES_DDL`].
/// Append-only: one row per skill applied in a turn, no UNIQUE key. The
/// by-skill index serves per-skill/version aggregate queries.
pub(crate) const SKILL_USAGE_DDL: &str = "CREATE TABLE IF NOT EXISTS skill_usage (
       execution_id INTEGER NOT NULL,
       skill TEXT NOT NULL,
       version INTEGER NOT NULL,
       reason TEXT NOT NULL DEFAULT '',
       ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
     );
     CREATE INDEX IF NOT EXISTS skill_usage_by_skill
       ON skill_usage(skill, version, execution_id);";

/// `mcp_usage` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION).
///
/// A per-call log (NOT a per-key aggregate like `files_touched`): the same
/// server+tool called twice is two rows. UNIQUE (execution_id, seq) is the
/// house double-write guard (the `events` pattern) — `seq` is the row's index
/// in an execution's drained batch, so re-persisting the same drained batch is
/// an error, not a silent double-count. `called_at_ms` is the call time
/// captured at the tool call (not the drain time). The by-server index is the
/// access path of [`Store::mcp_usage_stats`](crate::Store::mcp_usage_stats).
pub(crate) const MCP_USAGE_DDL: &str = "CREATE TABLE IF NOT EXISTS mcp_usage (
       execution_id INTEGER NOT NULL,
       seq INTEGER NOT NULL,
       server TEXT NOT NULL,
       tool TEXT NOT NULL,
       reason TEXT NOT NULL DEFAULT '',
       called_at_ms INTEGER NOT NULL,
       UNIQUE (execution_id, seq)
     );
     CREATE INDEX IF NOT EXISTS mcp_usage_by_server
       ON mcp_usage(server, tool);";

/// `tool_calls` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — one queryable row per tool call
/// (native, MCP, skill, or agent), normalized from the append-only `events`
/// stream (`tool_start` + `tool_result`) so the dashboard can query call
/// histograms without JSON-scanning the event log. Large outputs are NOT
/// stored here — only shape, timing, and success (`bytes_out` records the
/// result size, not the result). UNIQUE (execution_id, seq) is the house
/// double-write guard. The by-name index is the access path for usage
/// histograms (e.g. "grep called N times, graph_query zero").
pub(crate) const TOOL_CALLS_DDL: &str = "CREATE TABLE IF NOT EXISTS tool_calls (
       execution_id INTEGER NOT NULL,
       seq INTEGER NOT NULL,
       call_id TEXT NOT NULL DEFAULT '',
       name TEXT NOT NULL,
       surface TEXT NOT NULL DEFAULT 'native',
       args_json TEXT NOT NULL DEFAULT '{}',
       args_digest TEXT NOT NULL DEFAULT '',
       reason TEXT NOT NULL DEFAULT '',
       ok INTEGER NOT NULL DEFAULT 1,
       error TEXT NOT NULL DEFAULT '',
       bytes_out INTEGER NOT NULL DEFAULT 0,
       duration_ms INTEGER NOT NULL DEFAULT 0,
       ts TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
       UNIQUE (execution_id, seq)
     );
     CREATE INDEX IF NOT EXISTS tool_calls_by_name
       ON tool_calls(name, execution_id);";

/// `execution_reflection` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — the agent's own
/// assessment of ONE turn, tied 1:1 to its execution (and thus to
/// `executions.prompt`). Pairs the model's self-view (`delivered`,
/// `self_rating`, `what_went_well`, `what_to_improve`, `critique`) with the
/// objective companions (`produced_output`, `wrote_files`, `truncated`) so a
/// self-silent, zero-output turn is visibly a failure even if the model would
/// rate itself kindly.
pub(crate) const EXECUTION_REFLECTION_DDL: &str =
    "CREATE TABLE IF NOT EXISTS execution_reflection (
       execution_id INTEGER PRIMARY KEY,
       prompt TEXT NOT NULL DEFAULT '',
       delivered INTEGER,
       self_rating INTEGER,
       what_went_well TEXT NOT NULL DEFAULT '',
       what_to_improve TEXT NOT NULL DEFAULT '',
       critique TEXT NOT NULL DEFAULT '',
       produced_output INTEGER NOT NULL DEFAULT 0,
       wrote_files INTEGER NOT NULL DEFAULT 0,
       truncated INTEGER NOT NULL DEFAULT 0,
       recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
     );";

/// `reflections` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — the durable, unified home for
/// lessons and self-critiques (superset of the loose `.stella/private/reflections.jsonl`
/// and the context.db memory nodes). `execution_id` is NULL for cross-turn
/// lessons; `domains` is a JSON array of domain tags.
pub(crate) const REFLECTIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS reflections (
       id INTEGER PRIMARY KEY AUTOINCREMENT,
       execution_id INTEGER,
       kind TEXT NOT NULL,
       content TEXT NOT NULL,
       domains TEXT NOT NULL DEFAULT '[]',
       occurred_at INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS reflections_by_kind ON reflections(kind);";

/// `tasks` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — the latest task-board snapshot per
/// session, one row per (session, task id), mirrored from the protocol's
/// `TaskUpdate` snapshots by [`Store::record_task_board`](crate::Store::record_task_board). UNIQUE
/// (session_id, task_id) is the upsert key: each snapshot REPLACES a task's
/// row (board state, not history — the `events` stream already keeps every
/// snapshot). NOTE: SQL NULLs are pairwise distinct, so rows recorded
/// without a session id never conflict — dedup only holds per session.
/// `status`/`owner` carry the protocol's serde snake_case strings (e.g.
/// `"in_progress"`); `task_id` is the board's per-session ordinal id
/// ("1", "2", …), read back in `CAST(task_id AS INTEGER)` order.
pub(crate) const TASKS_DDL: &str = "CREATE TABLE IF NOT EXISTS tasks (
       id INTEGER PRIMARY KEY AUTOINCREMENT,
       execution_id INTEGER NOT NULL,
       session_id TEXT,
       task_id TEXT NOT NULL,
       subject TEXT NOT NULL,
       description TEXT,
       status TEXT NOT NULL,
       owner TEXT,
       updated_at INTEGER NOT NULL,
       UNIQUE(session_id, task_id)
     );";

/// `pull_requests` DDL at [`SCHEMA_VERSION`](crate::migrations::SCHEMA_VERSION) — one row per tracked pull
/// request, keyed by URL (the one stable identity across forks/renames).
/// UNIQUE (url) is the upsert key for [`Store::upsert_pull_request`](crate::Store::upsert_pull_request): a
/// later observation of the same PR updates its status/CI verdict in place.
/// `session_id` is the producing session's registry id, NULL when unknown;
/// `updated_at` is epoch millis of the latest observation.
pub(crate) const PULL_REQUESTS_DDL: &str = "CREATE TABLE IF NOT EXISTS pull_requests (
       id INTEGER PRIMARY KEY AUTOINCREMENT,
       session_id TEXT,
       url TEXT NOT NULL,
       number INTEGER,
       status TEXT NOT NULL,
       ci_status TEXT,
       updated_at INTEGER NOT NULL,
       UNIQUE(url)
     );";
