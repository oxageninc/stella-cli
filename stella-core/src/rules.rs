//! Workspace rules engine ( Phase 2 item 5; ported from
//! `apps/cli/src/rules/{types,loader,enforce,promote}.ts`).
//!
//! A rule is a binding instruction for the agent. The engine models two tiers:
//! **Tier 1** soft adherence, a rule rendered into the system prompt
//! ([`render_rules_section`]); and **Tier 2** hard enforcement, a rule carrying
//! a [`RuleGuard`] checked at the tool boundary ([`evaluate_guards`]) to block a
//! violating tool call before it runs and return the rule text so the model can
//! self-correct. Rules are authored as markdown under `.stella/rules/*.md`
//! (ADR-008 filesystem-first).
//!
//! Status: wired into the shipping CLI (issue #103). The production
//! [`RuleSource`] lives in `stella-cli`'s `rules` module: the on-disk rule
//! files (fs-backed) merged — through this module's precedence merge — with
//! extension-authored rules read from the workspace store
//! (`stella_store::Store::list_rules`). Each session driver loads rules
//! once at assembly, renders the Tier-1 section into its system prompt, and
//! arms Tier-2 guards at the tool boundary by registering an
//! [`evaluate_guards`] policy handler on the tool registry's
//! `tool.call.requested` blocking chain ([`crate::bus`],
//! `stella-tools::registry`) — a violation denies the call and returns the
//! rule text to the model.
//!
//! # No I/O in this module
//!
//! Discovering rule files means reading a directory and its file contents —
//! real I/O, which `stella-core` never performs directly. [`RuleSource`] is
//! the injectable discovery port, mirroring how [`crate::ports::ToolExecutor`]
//! is the injectable *execution* port: a concrete implementation backed by
//! real `std::fs` calls belongs to `stella-cli` (or `stella-tools`), never
//! here. Everything downstream of a `RuleSource` — frontmatter parsing,
//! precedence merging, Tier-1 rendering, Tier-2 enforcement, and candidate
//! mining — is plain synchronous logic over owned data, unit-tested below
//! against a fake `RuleSource`, no real files required.
//!
//! # Deliberately out of scope
//!
//! `apps/cli/src/rules/promote.ts`'s interactive candidate-promotion
//! *workflow* — mining lessons out of local traces/fleet-memory, then
//! prompting a human to approve writing a new rule file — needs a live user
//! prompt / TUI surface that doesn't exist in `stella-core`, correctly so:
//! this crate has no I/O and no UI. Concretely, out of scope here:
//!
//!   - `observationsFromTrace`/`observationsFromMemory` (TS): the adapters
//!     that pull [`RawObservation`]s out of `TurnTrace`/`MemoryRecord`.
//!     Those types don't have a Rust home yet (they land with the trace
//!     store and `stella-fleet` in later phases); once they do, porting
//!     those adapters is a small, mechanical follow-up — [`mine_candidates`]
//!     below already accepts the neutral `RawObservation` shape they would
//!     produce.
//!   - The actual interactive approve/write flow (`stella rules promote`):
//!     prompting the human, calling a filesystem port to check
//!     `already-exists`, and writing [`render_rule_markdown`]'s output to
//!     disk. That belongs to `stella-cli`.
//!
//! What IS ported: the full mining algorithm — lexical clustering, salience
//! override, dedup against existing rules, guard inference from consistent
//! file evidence, and ranking (all pure decision logic, see
//! [`mine_candidates`]) — plus the pure half of `promoteCandidate`:
//! rendering a candidate's exact rule-file content
//! ([`render_rule_markdown`]) and deciding what a promotion attempt *would*
//! do given the caller's own `approve`/`file_exists` facts
//! ([`decide_promotion`]).

use std::collections::HashMap;

use crate::glob::match_glob;

// ============================================================================
// Types (ports `rules/types.ts`)
// ============================================================================

/// A machine-enforceable guard that blocks a tool call violating the rule
/// (TS: `RuleGuard`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleGuard {
    /// Canonical tool the guard applies to (`Bash`, `Write`, `Edit`,
    /// `Read`), or `*`/`None` for any tool.
    pub tool: Option<String>,
    /// Block a file tool (`Write`/`Edit`/`Read`) whose path matches this
    /// glob.
    pub deny_path_glob: Option<String>,
    /// Block a `Bash` command matching this glob.
    pub deny_command_glob: Option<String>,
}

/// Which enforcement tier a rule sits at — computed from whether it carries
/// a [`RuleGuard`], not a stored field, so it can never drift out of sync
/// with `guard` (see `types.ts`'s doc comment: "always injected... Tier 1.
/// When it carries a guard, also hard-enforced... Tier 2").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleTier {
    /// Injected into the system prompt only; the model is asked, not
    /// forced.
    Prompt,
    /// Prompt-injected AND hard-blocked at the tool boundary via `guard`.
    Guarded,
}

/// One workspace rule (TS: `Rule`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Rule name — the filename stem or frontmatter `name`.
    pub id: String,
    /// Short description shown in `rules list`.
    pub description: String,
    /// The rule statement injected into the system prompt.
    pub text: String,
    /// Optional hard guard (Tier 2). Absent ⇒ prompt-only (Tier 1).
    pub guard: Option<RuleGuard>,
    /// Where the rule came from (a file path, or any opaque source label —
    /// TS: `source: string`).
    pub source: String,
}

impl Rule {
    /// This rule's enforcement tier — see [`RuleTier`].
    pub fn tier(&self) -> RuleTier {
        if self.guard.is_some() {
            RuleTier::Guarded
        } else {
            RuleTier::Prompt
        }
    }
}

// ============================================================================
// Discovery port + frontmatter parsing (ports `rules/loader.ts`)
// ============================================================================

/// One markdown file's raw content, already read from disk by a
/// [`RuleSource`] implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleFile {
    /// The file's path (or any opaque source label the implementation
    /// wants to carry through into [`Rule::source`]).
    pub path: String,
    pub contents: String,
}

/// The filesystem discovery port for rule files (
/// §1.3). A real implementation (owned by `stella-cli`/`stella-tools`)
/// walks each directory in `dirs`, in the given order, and returns every
/// `.md` file's contents — files within one directory sorted by name,
/// directories skipped silently if they don't exist (mirrors
/// `loadMarkdownRegistry`'s `existsSync` skip in `markdown-registry.ts`).
/// Order matters: [`load_rules`] merges by rule id with **later entries
/// overriding earlier ones**, so the directories must already be in
/// precedence order when passed to [`RuleSource::read_rule_files`] (see
/// [`rule_search_dirs`]).
pub trait RuleSource: Send + Sync {
    fn read_rule_files(&self, dirs: &[String]) -> Vec<RuleFile>;
}

/// Frontmatter split from a markdown file's body (TS: `Frontmatter`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub data: HashMap<String, String>,
    pub body: String,
}

/// Strip one pair of matching surrounding quotes (`"…"` or `'…'`).
pub(crate) fn strip_matched_quotes(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Split `---\n…\n---\nbody` into single-line key/value frontmatter plus
/// body text. No frontmatter fence ⇒ the whole (trimmed) input is the body
/// with empty data. Ports `parseFrontmatter` in `markdown-registry.ts`
/// (leading-BOM strip, quote-stripping on values), and additionally
/// flattens a YAML block sequence — a key with an empty scalar followed by
/// `- item` lines — onto that key as a comma-separated value, so list-typed
/// fields reach consumers in one shape no matter how the author wrote them.
pub fn parse_frontmatter(raw: &str) -> Frontmatter {
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if !text.starts_with("---") {
        return Frontmatter {
            data: HashMap::new(),
            body: text.trim().to_string(),
        };
    }
    let Some(rel_end) = text.get(3..).and_then(|rest| rest.find("\n---")) else {
        return Frontmatter {
            data: HashMap::new(),
            body: text.trim().to_string(),
        };
    };
    let end = 3 + rel_end;
    let header = text[3..end].trim();
    let after_fence = &text[end + 4..];
    let body = after_fence
        .strip_prefix("\r\n")
        .or_else(|| after_fence.strip_prefix('\n'))
        .unwrap_or(after_fence)
        .trim()
        .to_string();

    let mut data = HashMap::new();
    // The key whose scalar value was empty on its own line — the head of a
    // possible YAML block sequence (`tools:` followed by `- Read` lines).
    let mut pending_list_key: Option<String> = None;
    for line in header.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A `- item` line under an empty-valued key is a block-sequence
        // element: flatten it onto that key. Without a pending key the
        // line falls through to the scalar path (and is skipped when it
        // has no colon), exactly as before.
        if let Some(item) = trimmed.strip_prefix("- ")
            && let Some(key) = &pending_list_key
        {
            let item = strip_matched_quotes(item.trim());
            if !item.is_empty() {
                let entry: &mut String = data.entry(key.clone()).or_default();
                if !entry.is_empty() {
                    entry.push_str(", ");
                }
                entry.push_str(item);
            }
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            continue;
        };
        let key = trimmed[..colon].trim();
        let value = strip_matched_quotes(trimmed[colon + 1..].trim());
        if !key.is_empty() {
            data.insert(key.to_string(), value.to_string());
            pending_list_key = value.is_empty().then(|| key.to_string());
        }
    }
    Frontmatter { data, body }
}

fn file_stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

fn guard_from(data: &HashMap<String, String>) -> Option<RuleGuard> {
    let tool = data.get("guard-tool").cloned();
    let deny_path_glob = data.get("guard-deny-path").cloned();
    let deny_command_glob = data.get("guard-deny-command").cloned();
    if tool.is_none() && deny_path_glob.is_none() && deny_command_glob.is_none() {
        return None;
    }
    Some(RuleGuard {
        tool,
        deny_path_glob,
        deny_command_glob,
    })
}

/// Parse one rule file's raw content into a [`Rule`]. `None` when the file
/// has no usable id or an empty body — "a rule needs a name and a
/// statement" (TS: `ruleFromFile`).
pub fn rule_from_file(path: &str, raw: &str) -> Option<Rule> {
    let fm = parse_frontmatter(raw);
    let id = fm
        .data
        .get("name")
        .cloned()
        .unwrap_or_else(|| file_stem(path));
    if id.is_empty() || fm.body.trim().is_empty() {
        return None;
    }
    Some(Rule {
        id,
        description: fm.data.get("description").cloned().unwrap_or_default(),
        text: fm.body.trim().to_string(),
        guard: guard_from(&fm.data),
        source: path.to_string(),
    })
}

/// Where to look for rules, lowest → highest precedence (TS:
/// `LoadRulesOptions`). Unlike the TS loader, `stella-core` never defaults
/// these from `process.cwd()`/`homedir()` itself — no I/O, not even the
/// trivial kind — so the caller always supplies both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadRulesOptions {
    /// Project root.
    pub cwd: String,
    /// The user rules directory (normally `~/.config/stella/rules`).
    pub user_rules_dir: String,
}

/// The three rule directories in precedence order — user, Claude Code
/// interop, stella project rules — matching `loader.ts`'s comment exactly:
/// a later directory overrides an earlier one by rule id.
pub fn rule_search_dirs(opts: &LoadRulesOptions) -> Vec<String> {
    let cwd = opts.cwd.trim_end_matches('/');
    vec![
        opts.user_rules_dir.clone(),
        format!("{cwd}/.claude/rules"),
        format!("{cwd}/.stella/rules"),
    ]
}

/// Merge parsed rule files by id, preserving each id's *first* insertion
/// position but keeping its *latest* value — the same semantics as JS
/// `Map.set` on an existing key (TS: `[...registry.values()]` after
/// `loadMarkdownRegistry`'s merge loop).
fn merge_rule_files(files: Vec<RuleFile>) -> Vec<Rule> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Rule> = HashMap::new();
    for file in files {
        if let Some(rule) = rule_from_file(&file.path, &file.contents) {
            if !by_id.contains_key(&rule.id) {
                order.push(rule.id.clone());
            }
            by_id.insert(rule.id.clone(), rule);
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// Load every workspace rule visible from `opts.cwd`, merged by id across
/// sources via `source` (TS: `loadRules`).
pub fn load_rules(source: &dyn RuleSource, opts: &LoadRulesOptions) -> Vec<Rule> {
    let dirs = rule_search_dirs(opts);
    let files = source.read_rule_files(&dirs);
    merge_rule_files(files)
}

// ============================================================================
// Enforcement (ports `rules/enforce.ts`)
// ============================================================================

/// The system-prompt section listing active rules (Tier 1: soft adherence;
/// TS: `renderRulesSection`). Empty string when there are no rules.
pub fn render_rules_section(rules: &[Rule]) -> String {
    if rules.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        String::new(),
        "## Workspace rules (binding — follow exactly; guarded rules are hard-blocked)".to_string(),
    ];
    for r in rules {
        let suffix = if r.guard.is_some() {
            "  [enforced]"
        } else {
            ""
        };
        lines.push(format!("- {}{suffix}", r.text));
    }
    lines.join("\n")
}

/// The permission-gate deny entry a guard produces, e.g.
/// `Edit(migrations/**)` or a bare `Bash` (TS: `guardDenyEntry`).
///
/// Returns a single entry, preferring the path glob. A guard carrying BOTH a
/// path and a command glob is lossy here (the command half is dropped) — use
/// [`guards_to_deny`], which emits one entry per glob, when completeness
/// against an external gate matters.
pub fn guard_deny_entry(rule: &Rule) -> Option<String> {
    let guard = rule.guard.as_ref()?;
    let tool = guard.tool.as_deref().unwrap_or("*");
    let pattern = guard
        .deny_path_glob
        .as_deref()
        .or(guard.deny_command_glob.as_deref());
    Some(match pattern {
        Some(p) => format!("{tool}({p})"),
        None => tool.to_string(),
    })
}

/// Deny entries + their human reasons, for interop with an external
/// string-keyed permission gate (TS: `RuleDenies`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleDenies {
    pub deny: Vec<String>,
    pub reasons: HashMap<String, String>,
}

fn violation_reason(rule: &Rule) -> String {
    format!("rule \"{}\" — {}", rule.id, rule.text)
}

/// Convert guarded rules into gate deny entries + their human reasons
/// (Tier 2; TS: `guardsToDeny`).
pub fn guards_to_deny(rules: &[Rule]) -> RuleDenies {
    let mut deny = Vec::new();
    let mut reasons = HashMap::new();
    for rule in rules {
        let Some(guard) = rule.guard.as_ref() else {
            continue;
        };
        let tool = guard.tool.as_deref().unwrap_or("*");
        let reason = violation_reason(rule);
        // Emit one entry PER configured glob. A guard may carry both a path and
        // a command condition (`evaluate_guards` enforces both); the single
        // `guard_deny_entry` only surfaces the path, so without this the
        // external string-gate would silently miss the command half — the two
        // enforcement surfaces would then disagree.
        let mut pushed = false;
        for pat in [
            guard.deny_path_glob.as_deref(),
            guard.deny_command_glob.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let entry = format!("{tool}({pat})");
            reasons.insert(entry.clone(), reason.clone());
            deny.push(entry);
            pushed = true;
        }
        if !pushed {
            reasons.insert(tool.to_string(), reason.clone());
            deny.push(tool.to_string());
        }
    }
    RuleDenies { deny, reasons }
}

/// A tool invocation the agent is about to make, checked against guarded
/// rules. There is no TS equivalent by this name — `enforce.ts` stops at
/// producing deny-entry strings and leaves matching to a separate
/// `settings/permissions-gate.ts`; `stella-core` has no such second module
/// to hand off to, so [`evaluate_guards`] below folds `guardsToDeny` and
/// the gate's `matchGlob`-based deny check into one typed, directly
/// consultable decision for the step-driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposedAction<'a> {
    /// Canonical tool name (e.g. `"Edit"`, `"Bash"`, `"Write"`, `"Read"`).
    pub tool: &'a str,
    /// Present for file tools (`Write`/`Edit`/`Read`).
    pub path: Option<&'a str>,
    /// Present for `Bash`.
    pub command: Option<&'a str>,
}

/// One guarded rule a [`ProposedAction`] violated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleViolation {
    pub rule_id: String,
    /// `rule "<id>" — <text>` — the human-readable reason returned to the
    /// model so it can self-correct (same format as `guardsToDeny`'s
    /// `reasons` map).
    pub reason: String,
}

/// The typed result of checking a [`ProposedAction`] against every guarded
/// rule — every violation, not just the first, so the step-driver can log
/// (or surface to the model) the full set of reasons a call was rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardCheck {
    pub violations: Vec<RuleViolation>,
}

impl GuardCheck {
    /// `true` when at least one guard blocked the action.
    pub fn is_blocked(&self) -> bool {
        !self.violations.is_empty()
    }

    /// The violation the step-driver should actually cite when blocking —
    /// the first one found, mirroring first-match-wins deny semantics
    /// (`evaluateLocalPermission` in `permissions-gate.ts`).
    pub fn primary(&self) -> Option<&RuleViolation> {
        self.violations.first()
    }
}

fn guard_matches(guard: &RuleGuard, action: &ProposedAction<'_>) -> bool {
    let tool = guard.tool.as_deref().unwrap_or("*");
    if tool != "*" && tool != action.tool {
        return false;
    }
    match (&guard.deny_path_glob, &guard.deny_command_glob) {
        // A guard carrying BOTH globs denies when EITHER matches — a rule
        // author who wrote two deny conditions meant both of them, not
        // "path wins, command silently ignored".
        (Some(p), Some(c)) => {
            action.path.is_some_and(|path| match_glob(p, path))
                || action.command.is_some_and(|cmd| match_glob(c, cmd))
        }
        (Some(p), None) => action.path.is_some_and(|path| match_glob(p, path)),
        (None, Some(c)) => action.command.is_some_and(|cmd| match_glob(c, cmd)),
        // A guard with no path/command glob blocks the whole tool (TS:
        // `guardDenyEntry` emits the bare tool name in this case).
        (None, None) => true,
    }
}

/// Check `action` against every guarded rule (Tier 2 enforcement). Rules
/// with no `guard` (Tier 1, prompt-only) never appear here — they cannot
/// block anything structurally, only [`render_rules_section`] sees them.
pub fn evaluate_guards(rules: &[Rule], action: &ProposedAction<'_>) -> GuardCheck {
    let violations = rules
        .iter()
        .filter_map(|rule| {
            let guard = rule.guard.as_ref()?;
            guard_matches(guard, action).then(|| RuleViolation {
                rule_id: rule.id.clone(),
                reason: violation_reason(rule),
            })
        })
        .collect();
    GuardCheck { violations }
}

// ============================================================================
// Rule-promotion data model + mining (ports the pure half of `promote.ts`)
// ============================================================================

/// Where one occurrence of a candidate lesson came from (TS:
/// `RuleEvidence["source"]`). `TraceReasoning` is reserved for parity with
/// the TS union — `observationsFromTrace` (not yet ported, see module
/// docs) only ever produces `TraceFinding` today, deliberately: free-form
/// judge reasoning is too verbose to cluster reliably on term overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    TraceFinding,
    TraceReasoning,
    Memory,
}

/// One recurrence of a candidate lesson, with enough context to audit the
/// mining (TS: `RuleEvidence`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEvidence {
    pub source: EvidenceSource,
    /// e.g. `trace:<turnId>#judge<round>.finding<i>` or `memory:<id>` (TS:
    /// `ref` — renamed, `ref` is a Rust keyword).
    pub reference: String,
    pub occurred_at: u64,
    /// The lesson text as it appeared at this occurrence, truncated to 160
    /// chars.
    pub snippet: String,
}

/// A ranked rule-promotion candidate (TS: `RuleCandidate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleCandidate {
    /// Stable id derived from the representative text — also the rule
    /// filename stem.
    pub id: String,
    /// The representative lesson text — becomes the promoted rule's body.
    pub text: String,
    /// One-line summary written as `description:` frontmatter.
    pub description: String,
    pub occurrences: usize,
    /// `true` when at least one occurrence came from an already-salient
    /// observation (the caller decides salience before handing in a
    /// [`RawObservation`] — see its doc comment).
    pub salient: bool,
    pub evidence: Vec<RuleEvidence>,
    /// Best-effort guard inferred from consistent file evidence.
    /// `None` ⇒ prompt-only (Tier 1).
    pub guard: Option<RuleGuard>,
    /// Ranking score, highest first.
    pub score: u32,
}

/// Mining thresholds (TS: `MineConfig`, defaults from `DEFAULT_CONFIG`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MineConfig {
    /// Minimum recurrences before a non-salient cluster becomes a
    /// candidate.
    pub min_occurrences: usize,
    /// Jaccard term-overlap threshold to cluster two lessons as "the
    /// same".
    pub min_similarity: f64,
    /// Max candidates returned, ranked by score.
    pub limit: usize,
}

impl Default for MineConfig {
    fn default() -> Self {
        Self {
            min_occurrences: 3,
            min_similarity: 0.5,
            limit: 10,
        }
    }
}

/// One mineable observation, already extracted from whatever domain-
/// specific store it came from. TS's `observationsFromTrace`/
/// `observationsFromMemory` build this shape from `TurnTrace`/
/// `MemoryRecord`; `stella-core` doesn't have those types yet (see module
/// docs), so callers construct `RawObservation` directly. `memory_kind` is
/// intentionally a loose `String` rather than a `MemoryRecord["memoryKind"]`
/// enum for the same reason — only the literal value `"gotcha"` is
/// inspected, by [`infer_guard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObservation {
    pub text: String,
    pub source: EvidenceSource,
    pub reference: String,
    pub occurred_at: u64,
    pub files: Vec<String>,
    /// Already-elevated past a raw observation (TS: `isSalientMemory` —
    /// `memoryClass !== "OBSERVATION" || enforcementScore >= 90`), decided
    /// by the caller before construction.
    pub salient: bool,
    pub memory_kind: Option<String>,
}

/// The longest common leading directory across every path, or `None` if
/// they share none, or any path is a bare filename that can't anchor a
/// safe glob (TS: `commonDirPrefix`).
fn common_dir_prefix(paths: &[String]) -> Option<String> {
    let dirs: Vec<&str> = paths
        .iter()
        .map(|p| match p.rfind('/') {
            Some(idx) => &p[..idx],
            None => "",
        })
        .collect();
    if dirs.is_empty() || dirs.iter().any(|d| d.is_empty()) {
        return None;
    }
    let mut prefix = dirs[0].to_string();
    for d in &dirs[1..] {
        // Segment-aware containment. A raw `starts_with` treats `app/api2` as
        // being under `app/api`, so the inferred guard `app/api/**` would MISS
        // `app/api2/…` — one of the very files the guard was derived from. The
        // prefix must be either the whole dir or a parent *segment* of it, which
        // also makes the result independent of the input order.
        while !(*d == prefix.as_str() || d.starts_with(&format!("{prefix}/"))) {
            let cut = prefix.rfind('/')?;
            prefix.truncate(cut);
        }
    }
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Best-effort guard inference: when a cluster's `"gotcha"`-kind evidence
/// shares a common directory, propose blocking that directory. Anything
/// looser is left prompt-only rather than guessing a guard that could
/// wrongly hard-block unrelated work (TS: `inferGuard`).
fn infer_guard(cluster: &[RawObservation]) -> Option<RuleGuard> {
    let gotchas: Vec<&RawObservation> = cluster
        .iter()
        .filter(|o| o.memory_kind.as_deref() == Some("gotcha") && !o.files.is_empty())
        .collect();
    if gotchas.is_empty() {
        return None;
    }
    let files: Vec<String> = gotchas.iter().flat_map(|o| o.files.clone()).collect();
    common_dir_prefix(&files).map(|prefix| RuleGuard {
        tool: None,
        deny_path_glob: Some(format!("{prefix}/**")),
        deny_command_glob: None,
    })
}

/// Mine [`RawObservation`]s into ranked rule-promotion candidates. A
/// cluster of similar-enough observations (Jaccard ≥
/// `config.min_similarity`) qualifies when either it recurred at least
/// `config.min_occurrences` times, or any occurrence is already salient.
/// Candidates that duplicate an existing rule's text are dropped (TS:
/// `mineCandidates`).
pub fn mine_candidates(
    observations: Vec<RawObservation>,
    existing_rules: &[Rule],
    config: &MineConfig,
) -> Vec<RuleCandidate> {
    let clusters =
        crate::mining::cluster_observations(observations, config.min_similarity, |o| &o.text);
    let mut candidates: Vec<RuleCandidate> = Vec::new();

    for cluster in clusters {
        let salient = cluster.iter().any(|o| o.salient);
        if cluster.len() < config.min_occurrences && !salient {
            continue;
        }
        let Some(text) = crate::mining::representative_text(&cluster, |o| &o.text) else {
            continue;
        };
        if crate::mining::already_captured(
            &text,
            existing_rules.iter().map(|r| r.text.as_str()),
            config.min_similarity,
        ) {
            continue;
        }

        let guard = infer_guard(&cluster);
        let mut sorted = cluster;
        sorted.sort_by_key(|e| std::cmp::Reverse(e.occurred_at));
        let occurrences = sorted.len();
        let evidence: Vec<RuleEvidence> = sorted
            .iter()
            .map(|o| RuleEvidence {
                source: o.source,
                reference: o.reference.clone(),
                occurred_at: o.occurred_at,
                snippet: o.text.chars().take(160).collect(),
            })
            .collect();

        let plural = if occurrences == 1 { "" } else { "s" };
        let salience_note = if salient {
            " (includes an already-salient memory)"
        } else {
            ""
        };

        candidates.push(RuleCandidate {
            id: format!(
                "{}-{}",
                crate::mining::slugify(&text, "lesson"),
                crate::mining::hash8(&text)
            ),
            description: format!(
                "Promoted from {occurrences} recurring observation{plural}{salience_note}."
            ),
            occurrences,
            salient,
            evidence,
            guard,
            score: (occurrences as u32) * 10 + if salient { 50 } else { 0 },
            text,
        });
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.score));
    candidates.truncate(config.limit);
    candidates
}

/// Render the exact `.stella/rules/<id>.md` file content for `candidate` —
/// the same frontmatter shape [`rule_from_file`] parses back (mirrors the
/// frontmatter-building lines in `promoteCandidate`, minus the file write).
/// Writing this to disk is the I/O half `stella-cli` owns; this half is
/// pure and independently testable.
pub fn render_rule_markdown(candidate: &RuleCandidate) -> String {
    let mut lines = vec![
        "---".to_string(),
        format!("description: {}", candidate.description),
    ];
    if let Some(guard) = &candidate.guard {
        if let Some(tool) = &guard.tool {
            lines.push(format!("guard-tool: {tool}"));
        }
        if let Some(deny_path) = &guard.deny_path_glob {
            lines.push(format!("guard-deny-path: {deny_path}"));
        }
        if let Some(deny_command) = &guard.deny_command_glob {
            lines.push(format!("guard-deny-command: {deny_command}"));
        }
    }
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push(candidate.text.clone());
    lines.push(String::new());
    lines.join("\n")
}

/// What should happen to a candidate's rule file (TS:
/// `PromoteResult["status"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteStatus {
    /// The candidate should be written — the caller now writes
    /// [`render_rule_markdown`]'s output to disk.
    Written,
    /// `approve` was `false`; nothing should be touched (the decline
    /// path).
    Declined,
    /// A file already exists at the target path; leave it untouched
    /// rather than clobbering a hand-edit (idempotent re-promotion).
    AlreadyExists,
}

/// The pure half of `promoteCandidate`'s decision: given the caller's own
/// `approve` flag and its own I/O-derived `file_exists` fact (there is no
/// filesystem port for this in `stella-core` — see the module doc
/// comment), decide what should happen. Never writes anything itself.
pub fn decide_promotion(approve: bool, file_exists: bool) -> PromoteStatus {
    if !approve {
        PromoteStatus::Declined
    } else if file_exists {
        PromoteStatus::AlreadyExists
    } else {
        PromoteStatus::Written
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- frontmatter parsing ----

    #[test]
    fn parses_description_guard_and_body() {
        let raw = "---\ndescription: Never edit an applied migration\nguard-tool: Edit\nguard-deny-path: packages/database/migrations/*-applied/**\n---\nAdd a new forward migration instead of editing an applied one.";
        let fm = parse_frontmatter(raw);
        assert_eq!(
            fm.data.get("description").unwrap(),
            "Never edit an applied migration"
        );
        assert_eq!(fm.data.get("guard-tool").unwrap(), "Edit");
        assert!(fm.body.contains("Add a new forward migration"));
    }

    #[test]
    fn no_fence_means_whole_trimmed_text_is_the_body() {
        let fm = parse_frontmatter("  just a plain rule, no frontmatter  ");
        assert!(fm.data.is_empty());
        assert_eq!(fm.body, "just a plain rule, no frontmatter");
    }

    #[test]
    fn strips_a_leading_bom() {
        let fm = parse_frontmatter("\u{feff}---\ndescription: d\n---\nbody text");
        assert_eq!(fm.data.get("description").unwrap(), "d");
        assert_eq!(fm.body, "body text");
    }

    #[test]
    fn strips_matching_quotes_from_values() {
        let fm = parse_frontmatter(
            "---\ndescription: \"quoted value\"\nother: 'single quoted'\n---\nbody",
        );
        assert_eq!(fm.data.get("description").unwrap(), "quoted value");
        assert_eq!(fm.data.get("other").unwrap(), "single quoted");
    }

    #[test]
    fn ignores_comment_and_blank_frontmatter_lines() {
        let fm = parse_frontmatter("---\n# a comment\n\ndescription: d\n---\nbody");
        assert_eq!(fm.data.len(), 1);
        assert_eq!(fm.data.get("description").unwrap(), "d");
    }

    #[test]
    fn flattens_block_sequences_onto_their_key() {
        let fm = parse_frontmatter(
            "---\ntools:\n  - Read\n  - 'Grep'\n  - \"Web Search\"\ndescription: d\n---\nbody",
        );
        assert_eq!(fm.data.get("tools").unwrap(), "Read, Grep, Web Search");
        assert_eq!(
            fm.data.get("description").unwrap(),
            "d",
            "the key after the sequence parses normally"
        );
    }

    #[test]
    fn dash_lines_without_a_pending_list_key_stay_ignored() {
        let fm = parse_frontmatter("---\ndescription: d\n- stray item\n---\nbody");
        assert_eq!(fm.data.len(), 1);
        assert_eq!(fm.data.get("description").unwrap(), "d");
    }

    // ---- rule_from_file ----

    #[test]
    fn rule_from_file_uses_frontmatter_name_over_filename() {
        let r =
            rule_from_file(".stella/rules/style.md", "---\nname: custom-id\n---\nbody").unwrap();
        assert_eq!(r.id, "custom-id");
    }

    #[test]
    fn rule_from_file_falls_back_to_filename_stem() {
        let r = rule_from_file(".stella/rules/no-force-push.md", "Never force-push.").unwrap();
        assert_eq!(r.id, "no-force-push");
    }

    #[test]
    fn rule_from_file_returns_none_for_empty_body() {
        assert!(rule_from_file(".stella/rules/empty.md", "---\ndescription: d\n---\n").is_none());
    }

    #[test]
    fn rule_from_file_parses_a_bash_command_guard() {
        let r = rule_from_file(
            ".stella/rules/no-force-push.md",
            "---\nguard-tool: Bash\nguard-deny-command: git push --force*\n---\nNever force-push.",
        )
        .unwrap();
        assert_eq!(
            r.guard,
            Some(RuleGuard {
                tool: Some("Bash".to_string()),
                deny_path_glob: None,
                deny_command_glob: Some("git push --force*".to_string()),
            })
        );
    }

    #[test]
    fn rule_with_no_guard_frontmatter_is_prompt_only() {
        let r = rule_from_file(
            ".stella/rules/style.md",
            "---\ndescription: d\n---\nMatch the surrounding code style.",
        )
        .unwrap();
        assert!(r.guard.is_none());
        assert_eq!(r.tier(), RuleTier::Prompt);
    }

    #[test]
    fn a_guarded_rule_is_tier_guarded() {
        let r = rule_from_file(
            ".stella/rules/x.md",
            "---\nguard-tool: Edit\n---\nNever edit generated files.",
        )
        .unwrap();
        assert_eq!(r.tier(), RuleTier::Guarded);
    }

    // ---- discovery + precedence merge, against a fake RuleSource ----

    struct FakeRuleSource {
        by_dir: HashMap<String, Vec<RuleFile>>,
    }

    impl RuleSource for FakeRuleSource {
        fn read_rule_files(&self, dirs: &[String]) -> Vec<RuleFile> {
            let mut out = Vec::new();
            for dir in dirs {
                if let Some(files) = self.by_dir.get(dir) {
                    out.extend(files.iter().cloned());
                }
            }
            out
        }
    }

    fn rule_file(dir: &str, name: &str, contents: &str) -> RuleFile {
        RuleFile {
            path: format!("{dir}/{name}"),
            contents: contents.to_string(),
        }
    }

    fn opts() -> LoadRulesOptions {
        LoadRulesOptions {
            cwd: "/proj".to_string(),
            user_rules_dir: "/home/u/.config/stella/rules".to_string(),
        }
    }

    #[test]
    fn loads_a_rule_with_description_guard_and_body_end_to_end() {
        let o = opts();
        let dirs = rule_search_dirs(&o);
        let mut by_dir = HashMap::new();
        by_dir.insert(
            dirs[2].clone(),
            vec![rule_file(
                &dirs[2],
                "no-applied-migration.md",
                "---\ndescription: Never edit an applied migration\nguard-tool: Edit\nguard-deny-path: packages/database/migrations/*-applied/**\n---\nAdd a new forward migration instead of editing an applied one.",
            )],
        );
        let source = FakeRuleSource { by_dir };
        let rules = load_rules(&source, &o);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "no-applied-migration");
        assert_eq!(rules[0].description, "Never edit an applied migration");
        assert!(rules[0].text.contains("Add a new forward migration"));
    }

    #[test]
    fn stella_overrides_claude_overrides_user_by_id() {
        let o = opts();
        let dirs = rule_search_dirs(&o);
        let mut by_dir = HashMap::new();
        by_dir.insert(
            dirs[0].clone(),
            vec![rule_file(&dirs[0], "r.md", "user text")],
        );
        by_dir.insert(
            dirs[1].clone(),
            vec![rule_file(&dirs[1], "r.md", "claude text")],
        );
        let source_no_stella = FakeRuleSource {
            by_dir: by_dir.clone(),
        };
        let rules = load_rules(&source_no_stella, &o);
        assert_eq!(
            rules.iter().find(|r| r.id == "r").unwrap().text,
            "claude text"
        );

        by_dir.insert(
            dirs[2].clone(),
            vec![rule_file(&dirs[2], "r.md", "stella text")],
        );
        let source_with_stella = FakeRuleSource { by_dir };
        let rules = load_rules(&source_with_stella, &o);
        assert_eq!(
            rules.iter().find(|r| r.id == "r").unwrap().text,
            "stella text"
        );
    }

    #[test]
    fn ignores_files_with_an_empty_body() {
        let o = opts();
        let dirs = rule_search_dirs(&o);
        let mut by_dir = HashMap::new();
        by_dir.insert(
            dirs[2].clone(),
            vec![rule_file(
                &dirs[2],
                "empty.md",
                "---\ndescription: d\n---\n",
            )],
        );
        let source = FakeRuleSource { by_dir };
        assert!(load_rules(&source, &o).is_empty());
    }

    #[test]
    fn returns_empty_list_when_no_rules_exist() {
        let o = opts();
        let source = FakeRuleSource {
            by_dir: HashMap::new(),
        };
        assert_eq!(load_rules(&source, &o), Vec::new());
    }

    // ---- enforce: Tier 1 rendering ----

    fn rule(id: &str, text: &str, guard: Option<RuleGuard>) -> Rule {
        Rule {
            id: id.to_string(),
            description: String::new(),
            text: text.to_string(),
            guard,
            source: "test".to_string(),
        }
    }

    #[test]
    fn render_rules_section_is_empty_with_no_rules() {
        assert_eq!(render_rules_section(&[]), "");
    }

    #[test]
    fn render_rules_section_lists_rules_and_marks_enforced_ones() {
        let rules = vec![
            rule("a", "Always read before editing.", None),
            rule(
                "b",
                "Never edit applied migrations.",
                Some(RuleGuard {
                    tool: Some("Edit".to_string()),
                    deny_path_glob: Some("m/**".to_string()),
                    deny_command_glob: None,
                }),
            ),
        ];
        let out = render_rules_section(&rules);
        assert!(out.contains("Workspace rules"));
        assert!(out.contains("Always read before editing."));
        assert!(out.contains("Never edit applied migrations.  [enforced]"));
    }

    // ---- enforce: guard_deny_entry / guards_to_deny ----

    #[test]
    fn guard_deny_entry_builds_tool_glob_for_a_path_guard() {
        let r = rule(
            "x",
            "t",
            Some(RuleGuard {
                tool: Some("Edit".to_string()),
                deny_path_glob: Some("m/**".to_string()),
                deny_command_glob: None,
            }),
        );
        assert_eq!(guard_deny_entry(&r).unwrap(), "Edit(m/**)");
    }

    #[test]
    fn guard_deny_entry_builds_tool_glob_for_a_command_guard() {
        let r = rule(
            "x",
            "t",
            Some(RuleGuard {
                tool: Some("Bash".to_string()),
                deny_path_glob: None,
                deny_command_glob: Some("rm -rf*".to_string()),
            }),
        );
        assert_eq!(guard_deny_entry(&r).unwrap(), "Bash(rm -rf*)");
    }

    #[test]
    fn guard_deny_entry_builds_a_bare_tool_when_no_pattern() {
        let r = rule(
            "x",
            "t",
            Some(RuleGuard {
                tool: Some("Bash".to_string()),
                deny_path_glob: None,
                deny_command_glob: None,
            }),
        );
        assert_eq!(guard_deny_entry(&r).unwrap(), "Bash");
    }

    #[test]
    fn guard_deny_entry_is_none_without_a_guard() {
        assert!(guard_deny_entry(&rule("x", "t", None)).is_none());
    }

    #[test]
    fn guards_to_deny_produces_entries_and_a_reason_map() {
        let rules = vec![
            rule(
                "no-mig",
                "Add a forward migration.",
                Some(RuleGuard {
                    tool: Some("Edit".to_string()),
                    deny_path_glob: Some("mig/*-applied/**".to_string()),
                    deny_command_glob: None,
                }),
            ),
            rule("style", "prompt only", None),
        ];
        let denies = guards_to_deny(&rules);
        assert_eq!(denies.deny, vec!["Edit(mig/*-applied/**)".to_string()]);
        let reason = denies.reasons.get("Edit(mig/*-applied/**)").unwrap();
        assert!(reason.contains("rule \"no-mig\""));
        assert!(reason.contains("Add a forward migration."));
    }

    #[test]
    fn guards_to_deny_emits_both_globs_when_a_guard_sets_both() {
        // A guard with BOTH a path and a command condition must surface BOTH to
        // the external gate — dropping either lets the gate disagree with
        // `evaluate_guards`.
        let rules = vec![rule(
            "locked",
            "do not touch",
            Some(RuleGuard {
                tool: Some("Bash".to_string()),
                deny_path_glob: Some("secrets/**".to_string()),
                deny_command_glob: Some("rm -rf*".to_string()),
            }),
        )];
        let denies = guards_to_deny(&rules);
        assert!(denies.deny.contains(&"Bash(secrets/**)".to_string()));
        assert!(denies.deny.contains(&"Bash(rm -rf*)".to_string()));
        assert_eq!(denies.deny.len(), 2);
    }

    #[test]
    fn common_dir_prefix_stops_on_a_segment_boundary_not_mid_segment() {
        // `app/api2` is NOT under `app/api`; the common prefix must be `app`,
        // and the result must not depend on input order.
        let forward = common_dir_prefix(&["app/api/x.ts".into(), "app/api2/y.ts".into()]);
        let reverse = common_dir_prefix(&["app/api2/y.ts".into(), "app/api/x.ts".into()]);
        assert_eq!(forward.as_deref(), Some("app"));
        assert_eq!(reverse.as_deref(), Some("app"));
    }

    // ---- enforce: evaluate_guards (Tier 2, the actual block decision) ----

    #[test]
    fn evaluate_guards_blocks_a_matching_path() {
        let rules = vec![rule(
            "no-mig",
            "Add a forward migration.",
            Some(RuleGuard {
                tool: Some("Edit".to_string()),
                deny_path_glob: Some("packages/database/migrations/*-applied/**".to_string()),
                deny_command_glob: None,
            }),
        )];
        let blocked = evaluate_guards(
            &rules,
            &ProposedAction {
                tool: "Edit",
                path: Some("packages/database/migrations/0001-applied/up.sql"),
                command: None,
            },
        );
        assert!(blocked.is_blocked());
        assert_eq!(blocked.primary().unwrap().rule_id, "no-mig");

        let allowed = evaluate_guards(
            &rules,
            &ProposedAction {
                tool: "Edit",
                path: Some("src/app.ts"),
                command: None,
            },
        );
        assert!(!allowed.is_blocked());
    }

    #[test]
    fn evaluate_guards_ignores_a_mismatched_tool() {
        let rules = vec![rule(
            "no-mig",
            "t",
            Some(RuleGuard {
                tool: Some("Edit".to_string()),
                deny_path_glob: Some("mig/**".to_string()),
                deny_command_glob: None,
            }),
        )];
        let check = evaluate_guards(
            &rules,
            &ProposedAction {
                tool: "Write",
                path: Some("mig/0001.sql"),
                command: None,
            },
        );
        assert!(!check.is_blocked());
    }

    #[test]
    fn evaluate_guards_wildcard_tool_applies_to_any_tool() {
        let rules = vec![rule(
            "no-force-push",
            "Never force-push.",
            Some(RuleGuard {
                tool: None,
                deny_path_glob: None,
                deny_command_glob: Some("git push --force*".to_string()),
            }),
        )];
        let check = evaluate_guards(
            &rules,
            &ProposedAction {
                tool: "Bash",
                path: None,
                command: Some("git push --force origin main"),
            },
        );
        assert!(check.is_blocked());
    }

    #[test]
    fn evaluate_guards_bare_tool_guard_blocks_the_whole_tool() {
        let rules = vec![rule(
            "no-bash",
            "No shell access.",
            Some(RuleGuard {
                tool: Some("Bash".to_string()),
                deny_path_glob: None,
                deny_command_glob: None,
            }),
        )];
        let check = evaluate_guards(
            &rules,
            &ProposedAction {
                tool: "Bash",
                path: None,
                command: Some("ls"),
            },
        );
        assert!(check.is_blocked());
    }

    #[test]
    fn evaluate_guards_collects_every_violation_not_just_the_first() {
        let rules = vec![
            rule(
                "r1",
                "t1",
                Some(RuleGuard {
                    tool: Some("Bash".to_string()),
                    deny_path_glob: None,
                    deny_command_glob: Some("rm*".to_string()),
                }),
            ),
            rule(
                "r2",
                "t2",
                Some(RuleGuard {
                    tool: None,
                    deny_path_glob: None,
                    deny_command_glob: Some("rm*".to_string()),
                }),
            ),
        ];
        let check = evaluate_guards(
            &rules,
            &ProposedAction {
                tool: "Bash",
                path: None,
                command: Some("rm -rf /"),
            },
        );
        assert_eq!(check.violations.len(), 2);
    }

    #[test]
    fn tier_1_only_rules_never_block_anything() {
        let rules = vec![rule("style", "Match the surrounding code style.", None)];
        let check = evaluate_guards(
            &rules,
            &ProposedAction {
                tool: "Edit",
                path: Some("anything.ts"),
                command: None,
            },
        );
        assert!(!check.is_blocked());
    }

    // ---- promotion mining ----

    fn observation(text: &str, occurred_at: u64) -> RawObservation {
        RawObservation {
            text: text.to_string(),
            source: EvidenceSource::TraceFinding,
            reference: format!("trace:t#{occurred_at}"),
            occurred_at,
            files: Vec::new(),
            salient: false,
            memory_kind: None,
        }
    }

    fn salient_observation(text: &str, id: &str) -> RawObservation {
        RawObservation {
            text: text.to_string(),
            source: EvidenceSource::Memory,
            reference: format!("memory:{id}"),
            occurred_at: 1,
            files: Vec::new(),
            salient: true,
            memory_kind: None,
        }
    }

    #[test]
    fn detects_a_recurring_lesson_as_a_candidate_with_evidence() {
        let obs = vec![
            observation("forgot to add a test for the new route", 1),
            observation("forgot to add a test for the new route", 2),
            observation("forgot to add a test for the new route", 3),
        ];
        let candidates = mine_candidates(obs, &[], &MineConfig::default());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].text, "forgot to add a test for the new route");
        assert_eq!(candidates[0].occurrences, 3);
        assert_eq!(candidates[0].evidence.len(), 3);
        assert!(
            candidates[0]
                .evidence
                .iter()
                .all(|e| e.reference.starts_with("trace:"))
        );
    }

    #[test]
    fn does_not_surface_a_one_off_observation_below_the_threshold() {
        let obs = vec![observation("one-off issue nobody else hit", 1)];
        assert!(mine_candidates(obs, &[], &MineConfig::default()).is_empty());
    }

    #[test]
    fn promotes_a_single_salient_observation_regardless_of_occurrence_count() {
        let obs = vec![salient_observation(
            "database credentials leaked in a log line",
            "mem_1",
        )];
        let candidates = mine_candidates(obs, &[], &MineConfig::default());
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].salient);
        assert_eq!(candidates[0].occurrences, 1);
        assert_eq!(candidates[0].evidence[0].reference, "memory:mem_1");
    }

    #[test]
    fn ranks_a_salient_single_observation_above_a_merely_recurring_one() {
        let mut obs = vec![
            observation("forgot to add a test for the new route", 1),
            observation("forgot to add a test for the new route", 2),
            observation("forgot to add a test for the new route", 3),
        ];
        obs.push(salient_observation(
            "database credentials leaked in a log line",
            "mem_1",
        ));
        let candidates = mine_candidates(obs, &[], &MineConfig::default());
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].salient);
    }

    #[test]
    fn does_not_surface_a_candidate_that_duplicates_an_existing_rule() {
        let obs = vec![
            observation("never force-push to a shared branch", 1),
            observation("never force-push to a shared branch", 2),
            observation("never force-push to a shared branch", 3),
        ];
        let existing = vec![rule(
            "no-force-push",
            "Never force-push to a shared branch.",
            None,
        )];
        assert!(mine_candidates(obs, &existing, &MineConfig::default()).is_empty());
    }

    #[test]
    fn infers_a_deny_path_guard_from_consistent_file_evidence() {
        let obs = vec![
            RawObservation {
                text: "never edit an applied migration file directly".to_string(),
                source: EvidenceSource::Memory,
                reference: "memory:m1".to_string(),
                occurred_at: 1,
                files: vec!["packages/database/migrations/0001-applied/up.sql".to_string()],
                salient: true,
                memory_kind: Some("gotcha".to_string()),
            },
            RawObservation {
                text: "never edit an applied migration file directly".to_string(),
                source: EvidenceSource::Memory,
                reference: "memory:m2".to_string(),
                occurred_at: 2,
                files: vec!["packages/database/migrations/0002-applied/up.sql".to_string()],
                salient: true,
                memory_kind: Some("gotcha".to_string()),
            },
        ];
        let candidates = mine_candidates(obs, &[], &MineConfig::default());
        assert_eq!(
            candidates[0].guard,
            Some(RuleGuard {
                tool: None,
                deny_path_glob: Some("packages/database/migrations/**".to_string()),
                deny_command_glob: None,
            })
        );
    }

    #[test]
    fn leaves_candidate_prompt_only_when_files_share_no_common_directory() {
        let obs = vec![
            RawObservation {
                text: "shared lesson text here".to_string(),
                source: EvidenceSource::Memory,
                reference: "memory:m1".to_string(),
                occurred_at: 1,
                files: vec!["a/one.ts".to_string()],
                salient: true,
                memory_kind: Some("gotcha".to_string()),
            },
            RawObservation {
                text: "shared lesson text here".to_string(),
                source: EvidenceSource::Memory,
                reference: "memory:m2".to_string(),
                occurred_at: 2,
                files: vec!["b/two.ts".to_string()],
                salient: true,
                memory_kind: Some("gotcha".to_string()),
            },
        ];
        let candidates = mine_candidates(obs, &[], &MineConfig::default());
        assert!(candidates[0].guard.is_none());
    }

    #[test]
    fn mine_candidates_respects_the_limit() {
        // Five lessons with deliberately disjoint vocabulary (a shared
        // boilerplate template — e.g. "lesson about {word} handling" —
        // would keep 3 of 4 terms identical across "different" lessons,
        // pushing Jaccard similarity above the default 0.5 threshold and
        // collapsing them into one cluster instead of five), each
        // recurring three times so all five clear the occurrence
        // threshold and become candidates before truncation.
        let lessons = [
            "forgot to add a test for the new route",
            "database credentials appeared in application logs",
            "team members must not force push shared branches",
            "response handler skipped a required null check",
            "queries used the wrong tenant scope entirely",
        ];
        let mut obs = Vec::new();
        for lesson in lessons {
            for occurrence in 0..3u64 {
                obs.push(observation(lesson, occurrence));
            }
        }
        let config = MineConfig {
            limit: 2,
            ..MineConfig::default()
        };
        let candidates = mine_candidates(obs, &[], &config);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn mine_candidates_is_deterministic_across_reruns() {
        let obs = vec![
            observation("forgot to add a test for the new route", 1),
            observation("forgot to add a test for the new route", 2),
            observation("forgot to add a test for the new route", 3),
        ];
        let first = mine_candidates(obs.clone(), &[], &MineConfig::default());
        let second = mine_candidates(obs, &[], &MineConfig::default());
        assert_eq!(first[0].id, second[0].id);
    }

    // ---- render_rule_markdown + decide_promotion ----

    #[test]
    fn render_rule_markdown_round_trips_through_rule_from_file() {
        let obs = vec![
            observation("forgot to add a test for the new route", 1),
            observation("forgot to add a test for the new route", 2),
            observation("forgot to add a test for the new route", 3),
        ];
        let candidates = mine_candidates(obs, &[], &MineConfig::default());
        let candidate = &candidates[0];
        let markdown = render_rule_markdown(candidate);
        let parsed = rule_from_file(&format!("{}.md", candidate.id), &markdown).unwrap();
        assert_eq!(parsed.id, candidate.id);
        assert_eq!(parsed.text, candidate.text);
        assert!(parsed.description.contains("3 recurring observations"));
        assert!(parsed.guard.is_none());
    }

    #[test]
    fn render_rule_markdown_includes_an_inferred_guard() {
        let obs = vec![
            RawObservation {
                text: "never edit an applied migration file directly".to_string(),
                source: EvidenceSource::Memory,
                reference: "memory:m1".to_string(),
                occurred_at: 1,
                files: vec!["packages/database/migrations/0001-applied/up.sql".to_string()],
                salient: true,
                memory_kind: Some("gotcha".to_string()),
            },
            RawObservation {
                text: "never edit an applied migration file directly".to_string(),
                source: EvidenceSource::Memory,
                reference: "memory:m2".to_string(),
                occurred_at: 2,
                files: vec!["packages/database/migrations/0002-applied/up.sql".to_string()],
                salient: true,
                memory_kind: Some("gotcha".to_string()),
            },
        ];
        let candidates = mine_candidates(obs, &[], &MineConfig::default());
        let candidate = &candidates[0];
        let markdown = render_rule_markdown(candidate);
        let parsed = rule_from_file(&format!("{}.md", candidate.id), &markdown).unwrap();
        assert_eq!(
            parsed.guard,
            Some(RuleGuard {
                tool: None,
                deny_path_glob: Some("packages/database/migrations/**".to_string()),
                deny_command_glob: None,
            })
        );

        // And the round-tripped rule feeds guards_to_deny + evaluate_guards
        // exactly like a hand-authored one would.
        let denies = guards_to_deny(std::slice::from_ref(&parsed));
        assert_eq!(
            denies.deny,
            vec!["*(packages/database/migrations/**)".to_string()]
        );
        assert!(denies.reasons.values().next().unwrap().contains(&parsed.id));
    }

    #[test]
    fn decide_promotion_declines_without_approval() {
        assert_eq!(decide_promotion(false, false), PromoteStatus::Declined);
        assert_eq!(decide_promotion(false, true), PromoteStatus::Declined);
    }

    #[test]
    fn decide_promotion_refuses_to_clobber_an_existing_file() {
        assert_eq!(decide_promotion(true, true), PromoteStatus::AlreadyExists);
    }

    #[test]
    fn decide_promotion_writes_when_approved_and_absent() {
        assert_eq!(decide_promotion(true, false), PromoteStatus::Written);
    }
}
