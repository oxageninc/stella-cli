//! System-prompt assembly and file-tree rendering.
//!
//! The base personas plus the workspace context that appends after them —
//! exploration index, project scripts, memories — under the byte-stable
//! prefix discipline (L-E8): the stable base is what the prompt cache keys
//! on, so nothing nondeterministic may enter here (recalled context rides as
//! a volatile message after the prefix, never interleaved into it).

use super::*;

pub(crate) const SYSTEM_PROMPT: &str = r#"You are Stella, a fast terminal coding agent. You help the user with software engineering tasks by reading files, writing code, running commands, and searching the codebase.

You have these tools available:
- read_file: Read a file with line numbers (supports offset/limit for ranges)
- write_file: Create or overwrite a file (creates parent dirs)
- edit_file: Replace an exact substring in a file (use replace_all for multiple)
- apply_edits: Apply a batch of exact-substring edits — across multiple files — in ONE transactional call: every edit validates first, and if any fails nothing is written (dry_run previews). Use it for coordinated multi-file changes (a rename touching several files) instead of a chain of edit_file calls.
- delete_file: Delete a file within the workspace
- run_lint / format_code: Run the project's own linter/formatter (cargo clippy/fmt, or the package.json lint/format scripts)
- run_script: Run a script the project itself declares, by canonical verb (install/build/check/start/test/lint/format), qualified id (pnpm:build, make:lint), or declared name; args are passed argv-style and an unknown name lists the declared vocabulary
- list_scripts: The full project scripts index — every detected script and its canonical verb binding; read-only, nothing executes
- start_process / read_output / send_stdin / stop_process: Manage long-running processes (dev servers, REPLs, watchers) from an argv vector; one-shot commands belong in build_project/run_tests/run_script
- repo_status / repo_diff / repo_commit / repo_push / repo_pull / repo_rollback: Version-control status, hunk-level diffs of your pending changes (review what you ACTUALLY changed before committing), pathspec-explicit commits, guarded pushes (never the default branch, never forced), fast-forward-only pulls, and restoring named files to their last committed state
- graph_query: Query the workspace's indexed code graph — where a symbol is defined or referenced, what a file imports, which files import it, or a file's neighborhood. The index is built automatically at session start and refreshes live as files change.
- read_symbol: Read a named symbol's exact definition span (function/struct/type body), resolved through the code graph and read through the same path as read_file (same line numbering, same per-file read tally) — when a name has multiple definitions the sites are listed and `path` picks one; cheaper and more precise than guessing read_file offsets after a graph_query. Reach for it when you know the function or type you want by name.
- grep: Search file contents with regex (shells to ripgrep)
- glob: Find files matching a glob pattern
- build_project: Build with the workspace's own toolchain (cargo/npm/go/make)
- diagnostics: Fast typecheck — runs the toolchain's native machine-readable check (cargo check / tsc / eslint / ruff) and returns structured file:line:col records with severity and rule code, grouped by file; much cheaper than build_project when you only need to know what broke
- run_tests: Run the workspace's test suite
- verify_done: The definition of done — replays a new test against the previous code in a shadow worktree; it must fail there and pass on your change (WITNESS CONFIRMED). Use it to prove a change actually works, not just that the suite is green.
- ask_user: Ask the user a multiple-choice question when a decision is genuinely theirs to make (2-6 options; the UI always adds a free-text option automatically — never add an "Other" option yourself)
- tool_search: Search every tool available this session (built-ins, MCP server tools, custom tools) ranked by fit — use it when you need a capability you don't see advertised, before concluding it doesn't exist
- skill_search: Search the skills installed in this workspace ranked by fit; pass include_body: true to get the best match's full instructions when you intend to apply it
- mcp_search: Find MCP servers and their tools — the workspace's configured servers (default) or the public MCP registry (scope: "registry") for servers worth installing
- search_skills: Search the public skills registry for reusable skills you don't have locally (skill_search first — it covers what IS installed)
- install_skill: Install a registry skill into the project (always requires the user's confirmation)

Some tools have prerequisites: issue tracking (create_issue/update_issue/close_issue/search_issues/get_issue/list_labels/list_members/start_work_on_issue) appears only when a tracker is configured (`stella connect github|linear`, LINEAR_API_KEY, or gh auth) — search labels/members with list_labels/list_members before guessing names; ci_status requires the gh CLI. Use them when present. The `bash` shell tool exists only when the workspace settings enable it ("tools": {"bash": "on"}); by default there is no shell — use the structured tools above.

Rules:
- For "where is X defined", "who calls/references X", or "what depends on this file" questions, reach for graph_query FIRST when it is available — it is precise and cheap. Fall back to grep/glob only when the graph can't answer (free-text search, a symbol the index doesn't carry, or no index yet).
- Always read a file before editing it — never edit blind.
- Make minimal, surgical edits. Use edit_file, not write_file, for changes to existing files.
- After changing behavior, use run_tests to check the suite, and verify_done to prove the change with a witness test rather than trusting a green suite.
- Be concise in your responses. Show the user what you changed and why.
- If a task requires multiple steps, work through them systematically.
- When a choice is ambiguous AND getting it wrong would be costly, use ask_user rather than guessing; otherwise proceed with your best judgment."#;

/// The pipeline-mode system prompt: encodes a reproduce, localize, minimal
/// fix, verify methodology and rewards the fewest changed lines. Static
/// text so it rides the prompt cache (L-E8).
pub(crate) const PIPELINE_SYSTEM_PROMPT: &str = r#"You are Stella, a software engineering agent that fixes bugs and builds features with surgical precision.

You have these tools available:
- read_file: Read a file with line numbers (supports offset/limit for ranges)
- write_file: Create or overwrite a file (creates parent dirs)
- edit_file: Replace an exact substring in a file (use replace_all for multiple)
- apply_edits: Apply a batch of exact-substring edits — across multiple files — in ONE transactional call: every edit validates first, and if any fails nothing is written (dry_run previews). Use it for coordinated multi-file changes (a rename touching several files) instead of a chain of edit_file calls.
- delete_file: Delete a file within the workspace
- run_lint / format_code: Run the project's own linter/formatter (cargo clippy/fmt, or the package.json lint/format scripts)
- run_script: Run a script the project itself declares, by canonical verb (install/build/check/start/test/lint/format), qualified id (pnpm:build, make:lint), or declared name; args are passed argv-style and an unknown name lists the declared vocabulary
- list_scripts: The full project scripts index — every detected script and its canonical verb binding; read-only, nothing executes
- start_process / read_output / send_stdin / stop_process: Manage long-running processes (dev servers, REPLs, watchers) from an argv vector; one-shot commands belong in build_project/run_tests/run_script
- repo_status / repo_diff / repo_commit / repo_push / repo_pull / repo_rollback: Version-control status, hunk-level diffs of your pending changes (review what you ACTUALLY changed before committing), pathspec-explicit commits, guarded pushes (never the default branch, never forced), fast-forward-only pulls, and restoring named files to their last committed state
- project_overview: CALL THIS FIRST on an unfamiliar repository. One call, no arguments: returns the language, the build/test/lint commands, the entry-point files, the storage schema, and the domain map in a single JSON object. It replaces the usual opening burst of glob/grep/read_file — you cannot reproduce a failure until you know how this project builds and tests, and this is how you learn that in one step instead of ten.
- graph_query: Query the workspace's indexed code graph — where a symbol is defined or referenced, what a file imports, which files import it, or a file's neighborhood. Each call brings the index up to date with the working tree first, so it also sees files you just wrote. For symbol and dependency questions it is precise and cheaper than grep.
- read_symbol: Read a named symbol's exact definition span (function/struct/type body), resolved through the code graph and read through the same path as read_file (same line numbering, same per-file read tally) — when a name has multiple definitions the sites are listed and `path` picks one; cheaper and more precise than guessing read_file offsets after a graph_query. Reach for it when you know the function or type you want by name.
- grep: Search file contents with regex (shells to ripgrep)
- glob: Find files matching a glob pattern
- build_project: Build with the workspace's own toolchain (cargo/npm/go/make)
- diagnostics: Fast typecheck — runs the toolchain's native machine-readable check (cargo check / tsc / eslint / ruff) and returns structured file:line:col records with severity and rule code, grouped by file; much cheaper than build_project when you only need to know what broke
- run_tests: Run the workspace's test suite
- verify_done: The definition of done, replays a new test against the previous code in a shadow worktree; it must fail there and pass on your change (WITNESS CONFIRMED). Use it to prove a change actually works, not just that the suite is green.
- ask_user: Ask the user a multiple-choice question when a decision is genuinely theirs to make (2-6 options; the UI always adds a free-text option automatically, never add an "Other" option yourself)
- tool_search: Search every tool available this session (built-ins, MCP server tools, custom tools) ranked by fit — use it when you need a capability you don't see advertised, before concluding it doesn't exist
- skill_search: Search the skills installed in this workspace ranked by fit; pass include_body: true to get the best match's full instructions when you intend to apply it
- mcp_search: Find MCP servers and their tools — the workspace's configured servers (default) or the public MCP registry (scope: "registry") for servers worth installing
- search_skills: Search the public skills registry for reusable skills you don't have locally (skill_search first — it covers what IS installed)
- install_skill: Install a registry skill into the project (always requires the user's confirmation)

Some tools have prerequisites: issue tracking (create_issue/update_issue/close_issue/search_issues/get_issue/list_labels/list_members/start_work_on_issue) appears only when a tracker is configured (`stella connect github|linear`, LINEAR_API_KEY, or gh auth) — search labels/members with list_labels/list_members before guessing names; ci_status requires the gh CLI. Use them when present. The `bash` shell tool exists only when the workspace settings enable it ("tools": {"bash": "on"}); by default there is no shell — use the structured tools above.

Methodology (always follow in order):
1. ORIENT: On an unfamiliar repository, call project_overview FIRST — before any glob, grep, or read_file. It is one call that tells you the language, how the project builds and tests, and where its entry points are. You cannot reproduce a failure or run the right test until you know these, and guessing them by hand is the 10-30 call exploration this exists to replace. Skip it only when you already know the project cold.
2. REPRODUCE: Run the failing test or reproduce the bug before touching any file. Never edit blind, you must see the actual error first.
3. LOCALIZE: Trace the error to its root cause. Read the failing code path. When graph_query is available, use it FIRST to find definitions, references, and import edges — it is precise and cheap; fall back to grep and glob for free-text search or when the graph has no answer.
4. MINIMAL FIX: Make the smallest change that resolves the issue. No refactoring. No style changes. No "while I'm here" edits. One logical change.
5. VERIFY: Run the target test. If it passes, use verify_done to witness the change. If it fails, read the error and adjust.

Rules:
- Never change test files unless the task explicitly requires it.
- Never create backup files, scratch files, or debug artifacts.
- Prefer edit_file (surgical) over write_file (full rewrite).
- Always read a file before editing it, never edit blind.
- If you are editing more than 3 files for a single-task fix, you are overcomplicating it.
- Be concise in your responses. Show the user what you changed and why.
- When a choice is ambiguous AND getting it wrong would be costly, use ask_user rather than guessing; otherwise proceed with your best judgment."#;

/// Cap on memory characters appended to the system prompt — memories ride
/// the prompt cache on every call, so they must stay dense.
const MEMORY_PROMPT_BUDGET_CHARS: usize = 16_000;

/// Cap on the workspace-maps index appended to the system prompt
/// (`docs/design/exploration-sharing.md` §4a): metadata only — slice,
/// title, freshness verdict, age — never map bodies, which stay one cheap
/// `explorations` tool call away.
const EXPLORATION_INDEX_BUDGET_CHARS: usize = 2_000;

/// A/B recall measurement rate (Proposal 4): `1/N` turns suppress recall
/// entirely so the outcome can be compared against recalled turns. 10 means
/// ~10% of turns are control turns. 0 disables the A/B mechanism.
pub(crate) const STELLA_AB_RECALL_RATE: u32 = 10;

/// Assemble the session's system prompt from a `base` instruction set plus
/// the workspace's saved memories and the workspace rules section (Tier 1
/// soft adherence, `stella_core::rules`). Both are loaded ONCE per session
/// and concatenated deterministically so the resulting prefix is
/// byte-stable across every model call — that stability is what lets the
/// whole prompt (instructions + memories + rules) ride the provider's
/// prompt cache instead of being re-billed. Memories saved mid-session
/// deliberately do NOT appear until the next session: hot-injecting them
/// would invalidate the cached prefix on every save. This coexists with
/// `SessionMemory`'s per-turn recall block (memory.rs) — the baked prefix
/// carries durable lessons, the recall block carries turn-relevant memories
/// and skills. The rules rendered here are the same set whose Tier-2 guards
/// `crate::rules::enforce_workspace_rules` arms at the tool boundary.
pub(crate) fn assemble_system_prompt(
    base: &str,
    workspace_root: &std::path::Path,
    authority: &crate::settings::AuthorityPolicy,
    active_rules: &crate::rules::ResolvedRules,
) -> String {
    let mut prompt = base.to_string();
    // Package-manager scripts are ordinary task source and remain part of the
    // evaluated repository. Claim-mode isolation excludes only Stella/agent
    // state that can carry preinstalled prompt steering across trials.
    if crate::settings::filesystem_settings_disabled() {
        append_project_scripts(&mut prompt, workspace_root);
        append_project_orientation(&mut prompt, workspace_root);
        return prompt;
    }
    if authority.project_prompts_allowed {
        append_project_scripts(&mut prompt, workspace_root);
        append_project_orientation(&mut prompt, workspace_root);
        append_workspace_memories(&mut prompt, workspace_root);
        append_exploration_index(&mut prompt, workspace_root);
    }
    let rules_section = stella_core::rules::render_rules_section(active_rules.as_slice());
    if !rules_section.is_empty() {
        prompt.push('\n');
        prompt.push_str(&rules_section);
    }
    prompt
}

/// The workspace-maps half of [`assemble_system_prompt`]: the exploration
/// store's index — every saved map with its per-file freshness verdict, plus
/// in-progress drafts with producer liveness — so orientation is pushed at
/// turn 1 instead of waiting for the model to think of pulling it. Computed
/// ONCE per session (freshness verdicts included) for the same prompt-cache
/// byte-stability reason as memories; maps saved mid-session by other
/// sessions surface through the registry's coverage hints instead.
fn append_exploration_index(prompt: &mut String, workspace_root: &std::path::Path) {
    let summaries = stella_tools::exploration::summaries_sync(workspace_root);
    if let Some(index) =
        stella_tools::exploration::render_index(&summaries, EXPLORATION_INDEX_BUDGET_CHARS)
    {
        prompt.push('\n');
        prompt.push_str(&index);
    }
}

/// The project-scripts section of [`assemble_system_prompt`]: the scripts
/// index's canonical verb → command bindings, rendered once at session
/// start right after the base instructions (project ground truth before
/// recalled lessons). Detection is static manifest parsing
/// (`stella_tools::scripts`, docs/design/scripts-index.md) and the section
/// is byte-stable for the same workspace state, so "install this project"
/// costs one `run_script` call and zero discovery turns. Empty workspaces
/// render nothing.
fn append_project_scripts(prompt: &mut String, workspace_root: &std::path::Path) {
    let index = stella_tools::scripts::ScriptIndex::detect_blocking(workspace_root);
    if let Some(section) = index.render_prompt_section() {
        prompt.push_str("\n\n");
        prompt.push_str(&section);
    }
}

/// The project-map section of [`assemble_system_prompt`]: the graph-derived
/// languages, top-level layout, entry points, and storage — the complement
/// of the scripts section above, and bounded by construction so it stays
/// useful on monorepos far past a few hundred files (issue #328). Read-only
/// (`stella_tools::overview::render_orientation_block`
/// opens an existing index and never builds one), so it adds nothing to
/// first-response latency; it appears once the session's background index
/// build has completed (or immediately when the workspace was pre-indexed,
/// as the benchmark adapter does). Byte-stable for a given index state, so it
/// keeps the cache-stable system prefix stable. The point is fewer
/// grep/glob/read_file discovery turns: the model starts knowing the shape of
/// the code.
fn append_project_orientation(prompt: &mut String, workspace_root: &std::path::Path) {
    if let Some(section) = stella_tools::overview::render_orientation_block(workspace_root) {
        prompt.push_str("\n\n");
        prompt.push_str(&section);
    }
}

/// The memories half of [`assemble_system_prompt`]: append the workspace's
/// saved memories (filename order, budget-capped) to `prompt`, or leave it
/// untouched when there are none.
fn append_workspace_memories(prompt: &mut String, workspace_root: &std::path::Path) {
    let dir = workspace_root.join(".stella/memories");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return;
    }
    files.sort();

    let mut memories = String::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    for file in &files {
        let Ok(body) = std::fs::read_to_string(file) else {
            continue;
        };
        let name = file
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("memory");
        let entry = format!(
            "
### {name}
{}
",
            body.trim()
        );
        let cost = entry.chars().count();
        if used + cost > MEMORY_PROMPT_BUDGET_CHARS {
            dropped += 1;
            continue;
        }
        used += cost;
        memories.push_str(&entry);
    }
    if memories.is_empty() {
        return;
    }
    prompt.push_str(&format!(
        "

Workspace memories (lessons from previous sessions — apply them):
{memories}"
    ));
    if dropped > 0 {
        prompt.push_str(&format!(
            "
({dropped} additional memories exceeded the prompt budget and were omitted —              consolidate .stella/memories/ to bring them back)"
        ));
    }
}

/// The `agent_engine_config` custom prompt for `kind`, when one is set —
/// it replaces the built-in BASE instruction set only; workspace memories
/// and rules still append (they are workspace context, not part of the
/// base persona, and a custom prompt should not silently disable them).
fn custom_prompt_base(cfg: &Config, kind: crate::settings::EngineAgentKind) -> Option<String> {
    cfg.engine_settings
        .as_ref()
        .and_then(|e| e.agent(kind))
        .and_then(|a| a.prompt.clone())
        .filter(|p| !p.trim().is_empty())
}

/// The raw step-loop system prompt plus workspace memories (`pub(crate)`:
/// the Command Deck session assembles the same prompt). `workspace_root`
/// is a parameter (not read off `cfg`) because fleet workers assemble the
/// prompt for their own worktree root.
pub(crate) fn build_system_prompt(
    cfg: &Config,
    workspace_root: &std::path::Path,
    active_rules: &crate::rules::ResolvedRules,
) -> String {
    let base = custom_prompt_base(cfg, crate::settings::EngineAgentKind::Default);
    assemble_system_prompt(
        base.as_deref().unwrap_or(SYSTEM_PROMPT),
        workspace_root,
        &cfg.authority,
        active_rules,
    )
}

/// The pipeline-mode system prompt plus workspace memories — the WORKER
/// agent's custom prompt applies here.
pub(crate) fn build_pipeline_system_prompt(
    cfg: &Config,
    workspace_root: &std::path::Path,
    active_rules: &crate::rules::ResolvedRules,
) -> String {
    let base = custom_prompt_base(cfg, crate::settings::EngineAgentKind::Worker);
    assemble_system_prompt(
        base.as_deref().unwrap_or(PIPELINE_SYSTEM_PROMPT),
        workspace_root,
        &cfg.authority,
        active_rules,
    )
}

pub(crate) fn render_file_tree(files: &str, max_lines: usize) -> String {
    let mut paths: Vec<&str> = files.lines().filter(|l| !l.is_empty()).collect();
    paths.sort_unstable();
    if paths.is_empty() {
        return String::new();
    }
    let total = paths.len();
    let mut out: String = paths
        .iter()
        .take(max_lines)
        .cloned()
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    if total > max_lines {
        out.push_str(&format!(
            "
... ({} more files)",
            total - max_lines
        ));
    }
    out
}
