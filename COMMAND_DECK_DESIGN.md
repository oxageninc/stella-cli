# Stella Command Deck — TUI design contract

Transforms `stella-tui` from a single-session event-log REPL into a multi-tab,
multi-agent **operations deck**, preserving the existing pure-core / thin-shell
discipline (L-T1: render exclusively from events; ephemeral view state never in
the model).

This file is the **frozen contract** leaf-view builders code against. Types
here are authoritative — do not change signatures without updating this doc.

## Module layout (`stella-tui/src/`)

Existing (kept, reused by the Session tab — do not break):
`lib, model, render, shell, ui, composer, scroll, input`.

New:
- `envelope.rs` — the multi-agent wire types: `AgentId`, `AgentMeta`,
  `Inbound`, `AgentStatus`, and the outbound `WorkspaceInput`, `AgentControl`.
- `deck.rs` — `WorkspaceModel` (holds N per-agent `SessionModel`s + shared
  read-models), `AgentEntry`, `DeckTab`, and `apply_inbound()`.
- `diff.rs` — the ONE diff presentation (shared by the Session pane and the
  Files tab): full file path inline in a rule above the body, a line-number
  gutter parsed from `@@` hunks, and a closing rule counting `+/-` lines.
- `theme.rs` — color/style tokens. One source of truth for look (Stella amber
  `#FFAC26` accent, semantic status colors, glyphs, diff add/del tints, the
  ember spinner ramp).
- `resource.rs` — `ResourceMonitor` (sysinfo): global CPU%, per-pid CPU%/MEM.
  `ResourceSample { cpu_pct, mem_bytes }`.
- `fx.rs` — tachyonfx effect helpers (content fade-in, tab transition, sweep).
- `splash.rs` — the animated branded splash (tui-big-text + tachyonfx), skippable.
- `deck_render.rs` — top-level frame: comfy-tabs bar + active view + status bar
  + splash overlay. The tab dispatcher.
- `deck_shell.rs` — `run_deck(...)`: the new async loop (events + keys + a
  ~16ms animation/resource tick).
- `views/{session,agents,traces,graph,files}.rs` — one module per tab.

## Frozen core types

```rust
// envelope.rs
pub type AgentId = String; // stable per agent/run, e.g. "lead", "sub:auth"

#[derive(Clone, Debug, PartialEq)]
pub struct AgentMeta {
    pub id: AgentId,
    pub title: String,        // project/goal shown in the dashboard
    pub role: String,         // "lead" | "subagent" | ...
    pub pid: Option<u32>,     // OS pid for CPU/MEM attribution
    pub model: Option<String>,
    pub started_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus { Queued, Running, Paused, WaitingInput, Done, Failed, Killed }

/// One item on the workspace inbound channel — the multi-agent envelope.
#[derive(Clone, Debug)]
pub enum Inbound {
    Register(AgentMeta),                         // a new agent row appears
    Event { agent: AgentId, event: AgentEvent }, // an AgentEvent for one agent
    Status { agent: AgentId, status: AgentStatus }, // supervisor lifecycle state
}

/// Outbound: what the deck sends back to the caller/engine.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceInput {
    ToAgent { agent: AgentId, input: UserInput }, // route a UserInput to an agent
    Enqueue { text: String },                     // non-blocking new prompt
    QueueRemove { index: usize },                 // delete / pull-to-edit one queued prompt
    QueueClear,                                   // drop the whole backlog (ctrl+d ×2 confirmed)
    Control { agent: AgentId, control: AgentControl },
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentControl { Pause, Resume, Stop, Restart }
```

```rust
// deck.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeckTab { Session, Agents, Traces, Graph, Files }
impl DeckTab { pub const ALL: [DeckTab; 5] = [ /* … */ ]; pub fn title(self) -> &'static str; }

pub struct AgentEntry {
    pub meta: AgentMeta,
    pub model: SessionModel,      // the existing pure per-agent fold
    pub status: AgentStatus,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,            // BudgetTick-owned once ticked; StepUsage fallback before
    pub budget_ticked: bool,      // true once a BudgetTick arrived (tick owns cost_usd)
    pub last_activity_ms: u64,
    pub res: ResourceSample,      // stamped by ResourceMonitor (out-of-band)
    pub activity: ActivitySpark,  // ring buffer of recent intensity for a sparkline
}

// The focused-agent index is EPHEMERAL VIEW STATE and lives in `DeckUi`
// (deck_ui.rs), not here — L-T1 keeps it out of the model.
pub struct WorkspaceModel {
    pub agents: Vec<AgentEntry>,  // insertion-ordered; find by meta.id
    pub ledger: FileLedger,       // cross-agent file CRUD + line +/-
    pub routes: RouteLog,         // prompt→model decisions
    pub queue: PromptQueue,       // pending prompts (out-of-band; see purity boundary)
    pub trace: TraceLog,          // unified cross-agent event ring buffer
    pub now_ms: u64,              // deck clock (out-of-band; stamped by the shell tick)
    pub global_cpu_pct: f32,      // out-of-band; stamped by ResourceMonitor
}

impl WorkspaceModel {
    pub fn apply_inbound(&mut self, inbound: &Inbound);   // the sole fold of Inbound
}
```

## Read-models (derived; each has a builder subagent)

- `FileLedger` (`views/files.rs` owns the type): per (agent, path) a
  `FileRecord { path, agent, kind: FileChangeKind, added: u32, removed: u32,
  changes: u32 }`. `added`/`removed` are parsed from the `FileChange.diff`
  unified-diff string (count `+`/`-` lines, ignore `+++/---` headers and `@@`).
  Aggregates: total files, total +/-.
- `RouteLog`: append a `RouteEntry { ts, agent, model }` on each `StepUsage`
  (capped ring); surfaces which model handled what.
- `PromptQueue`: `VecDeque<QueuedPrompt { text, ts }>`. Submitting a prompt
  ALWAYS enqueues and returns — never blocks on a busy agent. Dispatch
  (`take_next`) REMOVES the prompt, so the queue holds only the waiting
  backlog and never accumulates dispatched history. The queue is a **list the
  user edits**, never a blob: `remove(index)` (ctrl+x / pull-back-to-edit) and
  `clear()` (ctrl+d twice) exist alongside dispatch, mirrored outbound as
  `QueueRemove` / `QueueClear` so the engine's backlog stays in sync.
- `TraceLog`: ring buffer of `TraceRow { ts, agent, kind, summary }` across all
  agents, filterable by agent.
- `ActivitySpark`: fixed-size ring (e.g. 32) of `u8` intensity, one bar per
  recent tick; rendered as a sparkline in the dashboard.

## Tabs & keybindings

| Tab | # | Contents |
|---|---|---|
| Session | 1 | the existing REPL for the focused agent (reuse `render`) |
| Agents  | 2 | claudectl-style table: agent · status · context% · cost · $/hr · elapsed · **CPU%** · MEM · in/out · activity sparkline |
| Traces  | 3 | unified cross-agent event timeline, filter by agent |
| Graph   | 4 | code-graph inspector (node/edge canvas from `CodeGraph::neighbors`) |
| Files   | 5 | file ledger: every file touched, CRUD op + lines +/−, totals |

- Tab switch: `Tab`/`Shift-Tab` cycle; comfy-tabs also handles mouse click/scroll.
- Global: `Ctrl-C` quit · composer is always focusable and **enqueues without blocking**.
- Global: `!cmd` in the composer runs a shell command **immediately** (its own
  synthetic `shell` agent lane; never enters the prompt queue) · `/` opens the
  command popup (`↑/↓` choose · `Tab` complete · `Enter` run · `Esc` dismiss) ·
  `Ctrl-T` (or `↑` from an empty composer on Session while prompts wait) opens
  the queue editor (`Enter` pull-to-edit · `Ctrl-X` delete · `Ctrl-D` ×2 clear) ·
  `Ctrl-R` expand/collapse thinking (collapsed = one line with a live tail).
- Agents tab: `↑/↓` select · `p` pause · `s` stop · `r` restart · `Enter` focus that agent's Session.
- Graph tab: `↑/↓/←/→` move cursor · `Enter` expand · `/` search symbol.
- Activity strip (one line above the composer, only while working/queued): the
  fast ember **garble spinner** (phase = `now_ms / tick`, replay-deterministic),
  the focused agent's current stage, and `N queued · ctrl+t`.
- Bottom status bar (always visible): routed model · **global CPU% gauge** · total spend + $/hr · active agents · queue depth.

## The purity boundary (L-T1, honored explicitly)

- **Event-pure (replayable):** every per-agent `SessionModel`, plus
  `FileLedger`, `RouteLog`, `TraceLog` — all a deterministic fold of the
  `Inbound` stream via `apply_inbound`, the sole `Inbound` mutator.
- **Out-of-band (labeled, NOT folded from `Inbound`):**
  - `ResourceSample` / `global_cpu_pct` — sampled from the OS via sysinfo on
    the shell tick (self-throttled to ~1 s).
  - the code-graph snapshot — queried from `stella-graph`, held by the view.
  - `now_ms` — the deck clock, stamped by the shell tick (time is not an
    event; elapsed/$-per-hour read it from one place).
  - `queue` — mutated by the shell when the *user* submits (`enqueue`, the
    local echo) and when the dispatcher drains (`take_next`); it is a fold of
    the **outbound** input stream, not of `Inbound`.

  These are the only exceptions and they are named as such — no other state
  may bypass the fold, and no view may read state that isn't one of the two
  folds or a labeled exception. (The focused-agent index is ephemeral view
  state in `DeckUi`, never in the model.)

## Durable sessions (pause / resume / crash-safety)

Every deck session is durable by construction, riding the same L-T1 fold the
deck renders from: because the screen is a pure function of the inbound
envelope stream, persisting the session is journaling that stream, and
resuming is replaying it.

- **Journal tee** (`stella-cli::session_persist`): the driver's `in_tx` reaches
  the deck through one interception task that mirrors every fold-relevant
  `Inbound` into `data_dir()/sessions/<id>/journal.jsonl`
  (`stella-store::journal`, append-only JSONL, torn-tail tolerant). Adjacent
  `Text`/`Reasoning` deltas coalesce per run; conversation transitions
  (`PromptStarted`, `Complete`, `Error`, status, reset, pipeline) fsync.
  Out-of-band view snapshots never journal; ephemeral chrome (boot narration,
  hints) is sent DIRECTLY to the deck so it never replays. `replay:<id>`
  lanes (`SessionOpen`'s read-only streams) are filtered at the tee — another
  session's past must never journal as this session's history. Sub-session
  lanes (`req:<n>`/`sub:<task-id>`) DO journal: they are the session's real
  history and replay with it.
- **Sidecar snapshots**: `history.json` (the LLM `Vec<CompletionMessage>`,
  atomic temp+fsync+rename at every turn boundary and `/clear`) and
  `queue.json` (the prompt backlog, write-through on every mutation).
- **Recovery rule**: every `PromptStarted` with no settle record after it on
  its own lane (`Complete` / non-retryable `Error` / `waiting_input` or a
  terminal lane status) was interrupted mid-turn — the lead's and any
  `req:<n>` worker's alike. Resume puts those prompts back at the FRONT of
  the queue in dispatch order and **parks dispatch** (the existing
  `HoldState`), so reopening a session shows where it stood and spends
  nothing until the user says go.
- **Navigation**: `stella resume [id|--list]` from the CLI;  `⏎` on a
  resumable row in the SESSIONS overlay (`WorkspaceInput::SessionResume`,
  serviced between turns and only while no workers are live) switches THIS
  deck to that session (`⏎` on any other row opens a read-only
  `SessionOpen` replay instead) — the
  current one parks as `Paused` (an untouched shell is removed instead), the
  target's journal replays behind a `SessionReset`, and its registry record
  is re-owned (same id, new pid). Quitting with a pending backlog is a
  `Paused` exit, not a cancellation — the work is durable.
- **Guarantee shape**: user input (prompts, queue) and completed turns survive
  anything up to power loss (fsynced); the worst case loses only the
  in-flight turn's streamed tail, which re-runs from its re-queued prompt.
  Session spend stays monotone: the budget guard reseeds from the journal's
  last `BudgetTick`.

## Backend seams (what's live vs. seam-fed today)

- Live now: per-agent event fold, file +/- from diffs, routing from
  `StepUsage`/`Complete`, graph via `CodeGraph::neighbors`, CPU/MEM via sysinfo.
- Live now: **staged pipeline routing** — `/pipeline` toggles the lead's turns
  between the raw `Engine::run_turn` loop and `stella-pipeline`'s staged flow
  (triage → witness → execute → verify → judge; `docs/pipeline.md`), mirrored
  to the `PIPELINE` stat box via `Inbound::Pipeline`. Named seam inside it:
  scope review **auto-approves** in the deck (the `ScopeReview` event is
  narrated in the transcript, not gated) — a deck-native scope-review card is
  the follow-up, same seam as the driver's `ScopeDecision` no-op.
- Live now: **sub-session workers** (`stella-cli/src/subsession.rs`) — the
  first real producer of multi-agent `Register`/`Status` envelopes. Prompts
  submitted mid-turn dispatch to dedicated `req:<n>` worker sessions instead
  of waiting; `task_assign` spawns `sub:<task-id>` workers; `SessionOpen`
  streams a dead session's persisted journal into a `replay:<id>` lane. The
  task board (`task_*` tools) folds as `AgentEvent::TaskUpdate` snapshots
  into a session-view checklist card, and a `gh`-backed monitor feeds the
  footer's PR cell (`⇢ #183 open ✓`).
- Live now: **per-worker Pause/Resume/Stop/Restart — at both layers.** Deck
  sub-session lanes route `AgentControl` through `service_worker_control`
  (pause parks at the engine's `TurnGate` step boundary, stop is the clean
  drop-at-await cancel, restart respawns from the lane's retained spec).
  Fleet tasks carry the same verbs on the dispatch seam itself:
  `stella_fleet::WorkerControls` ride the `FleetWorker` port, driven by
  `Fleet::pause_task` / `resume_task` / `stop_task` (restart = re-dispatch;
  the fleet keeps no respawn state).
- Still seam-fed: the deck's in-UI hookup to *fleet* tasks (a `stella fleet`
  run's workers are not deck lanes yet), lead-lane Pause/Resume
  (boundary-gating the staged pipeline needs a `PipelinePorts` gate), and
  fleet-worktree isolation for deck workers.

## ISSUES tab

The tracker-backed issue panel: browse/search the connected tracker's issues
(GitHub via `gh` or a `stella connect github` token, Linear via
`LINEAR_API_KEY` / `stella connect linear`), create one through a form,
comment, move status, and start work — without leaving the deck. The TUI does
**no I/O**: every operation is a `WorkspaceInput` the driver services through
`stella_tools::issue_ops` (always `tokio::spawn`ed, so the tab works mid-turn)
and answers with out-of-band `Inbound::IssuesList` / `IssueActDone` /
`EntityHits` snapshots. With no tracker connected, every list request answers
with the `run stella connect …` hint, which the tab renders as its empty
state. The first Tab-visit auto-loads the list (the INSTALLED AGENTS
first-visit idiom).

- Browse (composer stays live; letter verbs gate on a blank composer):
  `↑/↓` select · `r` refresh · `/` tracker search · `n` create form ·
  `c` comment · `s` set status · `w` start work (= status → in-progress;
  branch checkout stays the `start_work_on_issue` tool's job).
- Sub-modes (search line, create form, comment, set-status) are **modal**
  exactly like the INSTALLED AGENTS editor: they own the keyboard — the form
  claims Tab for field cycling ahead of deck tab-nav — and Esc returns to
  Browse from any of them.
- Create form: Title · Body (a plain `Composer` textarea — ⏎ is a line
  break) · Labels (comma-separated) · Assignee. `tab`/`⇧tab` (or `↑/↓` off
  the body) cycle fields; `ctrl+s` submits from anywhere, and ⏎ on the last
  field (popup closed) submits too. A successful create reports the new
  key + url and refreshes the list under the same seq.

**The type-ahead contract** (the Assignee and Labels fields): the popup opens
the INSTANT the first character lands in the field — `@` included; a bare `@`
searches the empty query, which lists all members. Every subsequent edit
(insert/backspace) immediately emits `EntitySearch { field, query, seq }` —
per-keystroke, **no debounce** — where the query is the field text minus a
leading `@` (assignee) / the segment after the last comma (labels). Replies
are seq-guarded: only the newest emitted seq is ever applied, so out-of-order
`EntityHits` can never regress the popup (the same stale-drop rule guards
`IssuesList`/`IssueActDone`). Rows render `Kind: label — description`; the
assignee vocabulary merges four independent sources — tracker members
(kind "Person"), installed agents ("Agent"), workspace memories ("Memory",
with content preview + `observed/valid from` provenance + citation stats),
and code-graph symbol definitions ("Symbol") — tracker first, capped at 20,
each source failing alone. While open the popup owns `↑/↓` (select),
`⏎`/`tab` (insert — assignee replaces the field, a label appends
comma-separated), and `esc` (close, keeping the typed text); every other key
keeps editing the field and re-fires the search. Emptying the field closes
the popup.

Deck-wide (all tabs): a two-row **trace strip** sits directly above the
composer chrome — a hairline rule over one dimmed line summarizing the newest
entry of the cross-agent `TraceLog` (`{kind} {summary}`), the glanceable
"what just happened" refreshed every frame.

## Build/test rules for every subagent (verbatim)

- Work in `/Users/macanderson/Workspaces/stella` on branch
  `feat/stella-command-deck-tui`. Commit on that branch with explicit pathspecs.
- **NEVER run all tests — hard rule.** Run ONLY `cargo test -p stella-tui`
  (scoped). Never a whole-workspace build/test. CI is the full gate.
- New pure logic (folds, diff parsing, ledger math) needs co-located unit tests
  that assert on the model/buffer, not ANSI. No `unwrap` in render paths that a
  malformed event could reach.
