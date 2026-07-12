//! Workspace skills engine — filesystem-first reusable knowledge
//! (`02-architecture.md` §6: `<workspace>/.stella/skills/`, "skill.md files
//! (ADR-008 filesystem-first)").
//!
//! A **skill** is a reusable procedure, convention, or learned preference —
//! "this repo formats SQL with X", "the user prefers tables over prose for
//! comparisons". It is distinct from a [`crate::rules`] *rule*: a rule is a
//! constraint/guardrail that can hard-block a tool call (Tier 2); a skill is
//! know-how the model *applies* when relevant. Skills are never enforced —
//! they are selected, injected as context, and followed.
//!
//! This module is the local model the CLI's skill machinery operates on. It
//! covers, per the user requirement ("make sure skills can be created and do
//! get created automatically when something has been observed enough like a
//! style preference etc — skill selection and skill search are super
//! important"):
//!
//!   1. **Loading** ecosystem-compatible `SKILL.md` files (both the
//!      `<dir>/<slug>/SKILL.md` `npx skills` layout and the flat
//!      `<dir>/<slug>.md` layout), with per-file tolerance — a malformed
//!      file is skipped with a typed [`SkillDiagnostic`], never fatal.
//!   2. **Selection** ([`select_skills`]) — the "super important" part: pure
//!      lexical + domain scoring of the loaded skills against the current
//!      prompt, returning the top-k with an inspectable *why* (matched terms
//!      and domains).
//!   3. **Rendering** ([`render_skills_section`]) — the volatile context
//!      block the CLI injects **after** the byte-stable system prefix, so
//!      prompt-cache hits on the prefix are preserved (`09-lessons-learned.md`
//!      L-E8: recalled context is a live query at turn start that rides as a
//!      volatile message after the stable system block, never a cached prompt
//!      block).
//!   4. **Auto-creation** ([`mine_skill_candidates`], [`decide_auto_creation`])
//!      — mining recurring observations (style preferences, reflection
//!      lessons) into new skill files once something has been "observed
//!      enough", capped so it feels magical, not spammy.
//!   5. **Install vocabulary** ([`SkillInstallProposal`], [`InstallDecision`])
//!      — the typed shape the registry-search/install glue and a future TUI
//!      speak, so both sides agree on one contract.
//!
//! # No I/O in this module (`02-architecture.md` §1.3)
//!
//! Discovering skill files means reading directories and file contents — real
//! I/O, which `stella-core` never performs directly. [`SkillSource`] is the
//! injectable discovery port, mirroring [`crate::rules::RuleSource`]. A
//! concrete implementation backed by real `std::fs` calls belongs to
//! `stella-cli`/`stella-tools`, never here. Everything downstream — frontmatter
//! parsing, precedence merging, selection, rendering, and candidate mining —
//! is plain synchronous logic over owned data, unit-tested below against a
//! fake `SkillSource`, no real files required.
//!
//! Built-in/seed skills the CLI ships are, per `09-lessons-learned.md` L-L2,
//! embedded as `include_str!` compile-time data on the CLI side — nothing here
//! resolves anything relative to the binary's install path. This module
//! operates purely on already-loaded content regardless of where it came from.
//!
//! # Reuse of `crate::rules`
//!
//! Frontmatter splitting is shared: [`crate::rules::parse_frontmatter`] is
//! already `pub` and does exactly the BOM-strip + `---` fence + single-line
//! `key: value` parse this format needs (no YAML dependency), so it is reused
//! rather than duplicated. The lexical helpers `rules` uses for mining
//! (`terms`/`jaccard`/`slugify`/`hash8` and the stopword list) are *private*
//! to that module; rather than widen its API (which would mean editing
//! `rules.rs`), the ~40 lines are reimplemented locally here with identical
//! behavior.

use std::collections::{HashMap, HashSet};

// ============================================================================
// Types
// ============================================================================

/// Where a skill came from — informs display and the small selection
/// tie-break that nudges freshly-learned preferences up (see
/// [`select_skills`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    /// Authored under the workspace's `.stella/skills/`.
    Workspace,
    /// Authored under the user-global skills directory.
    User,
    /// Written by [`decide_auto_creation`] from mined observations — carried
    /// back through an `origin: auto` frontmatter marker.
    AutoCreated,
    /// Installed from a registry into the `<dir>/<slug>/SKILL.md` layout.
    Installed,
}

/// One workspace skill, parsed from a `SKILL.md`/`<slug>.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Skill name — the frontmatter `name:` or the path-derived slug.
    pub name: String,
    /// Short description (frontmatter `description:`) — the primary signal
    /// [`select_skills`] scores the prompt against.
    pub description: String,
    /// Domain tags (frontmatter `domains:`, comma-separated or `[a, b]`),
    /// as authored — matched case-insensitively.
    pub domains: Vec<String>,
    /// The markdown body: the actual procedure/convention/preference.
    pub body: String,
    /// The file this was loaded from (or any opaque source label).
    pub source_path: String,
    /// Provenance — see [`SkillOrigin`].
    pub origin: SkillOrigin,
}

// ============================================================================
// Discovery port + parsing
// ============================================================================

/// One skill file's raw content, already read from disk by a [`SkillSource`]
/// implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFile {
    /// The file's path — used both as [`Skill::source_path`] and, when no
    /// `name:` frontmatter is present, to derive the slug (both the
    /// `<dir>/<slug>/SKILL.md` and flat `<dir>/<slug>.md` layouts).
    pub path: String,
    pub content: String,
}

/// The filesystem discovery port for skill files (`02-architecture.md`
/// §1.3), mirroring [`crate::rules::RuleSource`]. A real implementation
/// (owned by `stella-cli`/`stella-tools`) walks each directory in `roots`, in
/// the given order, and returns every skill file's contents — discovering
/// **both** layouts: `<root>/<slug>/SKILL.md` (the `npx skills` ecosystem
/// layout; installed skills land this way) and flat `<root>/<slug>.md`.
/// Directories that don't exist are skipped silently. Order matters:
/// [`load_skills`] merges by skill name with **later roots overriding
/// earlier ones**, so the roots must already be in precedence order — see
/// [`skill_search_dirs`], where the workspace directory comes last so it wins
/// over a user-global skill of the same name.
pub trait SkillSource: Send + Sync {
    fn read_skill_files(&self, roots: &[String]) -> Vec<SkillFile>;
}

/// Why one skill file could not be loaded — a skill needs a name, a
/// description, and a body. Typed so the CLI can report it precisely instead
/// of silently dropping the file (house style: inspectable outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillProblem {
    /// No `name:` frontmatter and the path yields no usable slug.
    MissingName,
    /// No `description:` frontmatter (required by the `SKILL.md` shape).
    MissingDescription,
    /// The markdown body is empty — a skill with no procedure is useless.
    EmptyBody,
}

/// A malformed skill file, skipped during loading (never fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: String,
    pub problem: SkillProblem,
}

/// The result of a load: the merged skills plus every file that was skipped
/// and why. [`load_skills`] returns just the skills for the common path;
/// callers wanting to surface problems use [`load_skills_with_diagnostics`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedSkills {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Where to look for skills. Unlike a TS loader, `stella-core` never defaults
/// these from `cwd()`/`homedir()` itself — no I/O, not even the trivial kind —
/// so the caller always supplies both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadSkillsOptions {
    /// `<workspace>/.stella/skills` — highest precedence.
    pub workspace_skills_dir: String,
    /// The user-global skills directory (e.g. `~/.config/stella/skills`).
    pub user_skills_dir: String,
}

/// The skill directories in precedence order, **lowest first**: user-global,
/// then workspace. [`load_skills`] merges later-over-earlier, so the
/// workspace directory (last) wins on a name collision — "workspace beats
/// user-global".
pub fn skill_search_dirs(opts: &LoadSkillsOptions) -> Vec<String> {
    vec![
        opts.user_skills_dir.clone(),
        opts.workspace_skills_dir.clone(),
    ]
}

/// Derive a skill's slug from its path, honoring both layouts: for a
/// `<dir>/<slug>/SKILL.md` file the slug is the *parent directory* name; for
/// a flat `<dir>/<slug>.md` file it is the filename stem. The frontmatter
/// `name:` always overrides this (see [`skill_from_file_with_origin`]).
fn skill_name_from_path(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.eq_ignore_ascii_case("SKILL.md") {
        return path.rsplit('/').nth(1).unwrap_or("").to_string();
    }
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

/// Parse a `domains:` value: a bracketed inline list `[a, b]` or a bare
/// comma-separated `a, b`. Values are trimmed and unquoted; empties dropped.
/// Case is preserved as authored (matching is case-insensitive elsewhere).
fn parse_domains(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    inner
        .split(',')
        .map(|part| {
            part.trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .trim()
                .to_string()
        })
        .filter(|d| !d.is_empty())
        .collect()
}

/// Read an explicit `origin:` frontmatter marker into a [`SkillOrigin`].
/// `None` when absent or unrecognized, so the caller can fall back to the
/// directory-derived default.
fn origin_from_marker(marker: Option<&String>) -> Option<SkillOrigin> {
    match marker.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("auto" | "autocreated" | "auto-created") => Some(SkillOrigin::AutoCreated),
        Some("installed") => Some(SkillOrigin::Installed),
        Some("user") => Some(SkillOrigin::User),
        Some("workspace") => Some(SkillOrigin::Workspace),
        _ => None,
    }
}

/// Parse one skill file into a [`Skill`], tagging it with `default_origin`
/// unless the frontmatter carries an explicit `origin:` marker (which wins).
/// Returns a typed [`SkillDiagnostic`] instead of a [`Skill`] when the file
/// is missing a name, a description, or a body.
pub fn skill_from_file_with_origin(
    path: &str,
    raw: &str,
    default_origin: SkillOrigin,
) -> Result<Skill, SkillDiagnostic> {
    let fm = crate::rules::parse_frontmatter(raw);
    let diag = |problem| SkillDiagnostic {
        path: path.to_string(),
        problem,
    };

    let name = fm
        .data
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| skill_name_from_path(path));
    if name.trim().is_empty() {
        return Err(diag(SkillProblem::MissingName));
    }

    let description = fm
        .data
        .get("description")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| diag(SkillProblem::MissingDescription))?;

    if fm.body.trim().is_empty() {
        return Err(diag(SkillProblem::EmptyBody));
    }

    let domains = fm
        .data
        .get("domains")
        .map(|s| parse_domains(s))
        .unwrap_or_default();
    let origin = origin_from_marker(fm.data.get("origin")).unwrap_or(default_origin);

    Ok(Skill {
        name: name.trim().to_string(),
        description,
        domains,
        body: fm.body.trim().to_string(),
        source_path: path.to_string(),
        origin,
    })
}

/// Parse one skill file, defaulting its origin to [`SkillOrigin::Workspace`]
/// (used standalone and in tests; [`load_skills`] supplies a directory-derived
/// default instead).
pub fn skill_from_file(path: &str, raw: &str) -> Result<Skill, SkillDiagnostic> {
    skill_from_file_with_origin(path, raw, SkillOrigin::Workspace)
}

/// The default origin for a file at `path`, from which configured directory it
/// lives under (frontmatter markers override this in
/// [`skill_from_file_with_origin`]). Longest-matching directory wins so a
/// workspace dir nested under the user dir is still attributed correctly;
/// anything under neither is treated as workspace-local.
fn default_origin_for(path: &str, opts: &LoadSkillsOptions) -> SkillOrigin {
    let ws = opts.workspace_skills_dir.trim_end_matches('/');
    let us = opts.user_skills_dir.trim_end_matches('/');
    let under =
        |dir: &str| !dir.is_empty() && (path == dir || path.starts_with(&format!("{dir}/")));
    match (under(ws), under(us)) {
        (true, true) => {
            if ws.len() >= us.len() {
                SkillOrigin::Workspace
            } else {
                SkillOrigin::User
            }
        }
        (true, false) => SkillOrigin::Workspace,
        (false, true) => SkillOrigin::User,
        (false, false) => SkillOrigin::Workspace,
    }
}

/// Load every skill visible from `opts`, merged by name across sources, and
/// report every file that was skipped and why. Precedence: a later root
/// (workspace) overrides an earlier one (user) on a name collision, keeping
/// the first-seen ordering position (same semantics as JS `Map.set`).
pub fn load_skills_with_diagnostics(
    source: &dyn SkillSource,
    opts: &LoadSkillsOptions,
) -> LoadedSkills {
    let dirs = skill_search_dirs(opts);
    let files = source.read_skill_files(&dirs);

    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, Skill> = HashMap::new();
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();

    for file in files {
        let default_origin = default_origin_for(&file.path, opts);
        match skill_from_file_with_origin(&file.path, &file.content, default_origin) {
            Ok(skill) => {
                if !by_name.contains_key(&skill.name) {
                    order.push(skill.name.clone());
                }
                by_name.insert(skill.name.clone(), skill);
            }
            Err(diag) => diagnostics.push(diag),
        }
    }

    let skills = order
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .collect();
    LoadedSkills {
        skills,
        diagnostics,
    }
}

/// Load every skill visible from `opts`, merged by name (workspace beats
/// user-global). Malformed files are skipped silently; use
/// [`load_skills_with_diagnostics`] to also see why.
pub fn load_skills(source: &dyn SkillSource, opts: &LoadSkillsOptions) -> Vec<Skill> {
    load_skills_with_diagnostics(source, opts).skills
}

// ============================================================================
// Selection — the "super important" part (pure lexical + domain scoring)
// ============================================================================

/// How [`select_skills`] scores and trims. All scalar, so cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionConfig {
    /// Top-k cap on the returned skills.
    pub max_skills: usize,
    /// Minimum score a skill must reach to be selected at all.
    pub min_score: f64,
    /// Score added per matched active domain. Set high enough that a domain
    /// match beats a merely-weak lexical overlap (the product requirement:
    /// domain relevance should surface a skill the wording alone would miss).
    pub domain_boost: f64,
    /// Small bonus for an [`SkillOrigin::AutoCreated`] skill — a freshly-mined
    /// preference is, by construction, currently relevant; this is the
    /// recency/`AutoCreated` tie-break, never large enough to select a skill
    /// that is otherwise below `min_score`.
    pub auto_created_bonus: f64,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            max_skills: 5,
            min_score: 0.08,
            domain_boost: 0.5,
            auto_created_bonus: 0.02,
        }
    }
}

/// A skill chosen by [`select_skills`], with the score and the **why** —
/// which prompt terms and which active domains it matched — so the selection
/// is inspectable rather than a black box.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedSkill {
    pub skill: Skill,
    pub score: f64,
    /// Prompt terms that overlapped the skill's name+description (sorted).
    pub matched_terms: Vec<String>,
    /// Active domains the skill is tagged with (as authored on the skill).
    pub matched_domains: Vec<String>,
}

/// Select the skills most relevant to `prompt`, given the currently active
/// `active_domains`. Pure scoring: lexical Jaccard overlap between the prompt
/// and each skill's name+description, plus a per-matched-domain boost, plus
/// the small `AutoCreated` tie-break. Only skills scoring at least
/// `config.min_score` are returned, top-k by `config.max_skills`, highest
/// first (ties broken by name for determinism).
pub fn select_skills(
    skills: &[Skill],
    prompt: &str,
    active_domains: &[String],
    config: &SelectionConfig,
) -> Vec<SelectedSkill> {
    let prompt_terms: HashSet<String> = terms(prompt).into_iter().collect();

    let mut selected: Vec<SelectedSkill> = Vec::new();
    for skill in skills {
        let haystack = format!("{} {}", skill.name, skill.description);
        let skill_terms: HashSet<String> = terms(&haystack).into_iter().collect();

        let mut matched_terms: Vec<String> =
            prompt_terms.intersection(&skill_terms).cloned().collect();
        matched_terms.sort();

        let matched_domains: Vec<String> = skill
            .domains
            .iter()
            .filter(|d| active_domains.iter().any(|a| a.eq_ignore_ascii_case(d)))
            .cloned()
            .collect();

        let lexical = jaccard(&prompt_terms, &skill_terms);
        let domain_score = matched_domains.len() as f64 * config.domain_boost;
        let recency = if skill.origin == SkillOrigin::AutoCreated {
            config.auto_created_bonus
        } else {
            0.0
        };
        let score = lexical + domain_score + recency;

        // A pure-lexical match hanging off a single shared term is noise
        // ("about", "something") no matter what jaccard says — require at
        // least two shared terms unless a domain tag corroborates.
        let corroborated = !matched_domains.is_empty() || matched_terms.len() >= 2;

        if corroborated && score >= config.min_score {
            selected.push(SelectedSkill {
                skill: skill.clone(),
                score,
                matched_terms,
                matched_domains,
            });
        }
    }

    selected.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.skill.name.cmp(&b.skill.name))
    });
    selected.truncate(config.max_skills);
    selected
}

// ============================================================================
// Rendering — the volatile context block injected after the stable prefix
// ============================================================================

/// Chars-per-token divisor, consistent with [`crate::estimator`]'s
/// `CHARS_PER_TOKEN` (3.5 biases the estimate high — code/JSON run denser than
/// prose, and over-estimating trims *earlier*, the safe direction).
const CHARS_PER_TOKEN: f64 = 3.5;
/// Per-skill body budget before the body is truncated with a marker.
const SKILL_BODY_TOKEN_BUDGET: u64 = 400;
/// Total budget for the whole injected section — once exceeded, remaining
/// skills are dropped with an explicit note.
const SKILLS_SECTION_TOKEN_BUDGET: u64 = 1500;
/// Appended to a body cut to fit [`SKILL_BODY_TOKEN_BUDGET`].
const SKILL_BODY_TRUNCATION_MARKER: &str =
    "\n[skill body truncated to fit the context budget — open the skill file for the full text]";
/// Appended when [`SKILLS_SECTION_TOKEN_BUDGET`] is hit and further skills are
/// dropped.
const SKILLS_SECTION_OMISSION_MARKER: &str =
    "\n[additional lower-ranked skills omitted to fit the context budget]\n";

/// Estimate a string's token cost via the chars/3.5 heuristic (see
/// [`CHARS_PER_TOKEN`]). Char-based (not byte-based) so it lines up with the
/// char-boundary truncation in [`truncate_to_tokens`].
fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as u64
}

/// Truncate `text` to `budget` tokens on a char boundary, appending
/// [`SKILL_BODY_TRUNCATION_MARKER`] when it was cut. Returned unchanged when
/// already within budget.
fn truncate_to_tokens(text: &str, budget: u64) -> String {
    if estimate_tokens(text) <= budget {
        return text.to_string();
    }
    let max_chars = (budget as f64 * CHARS_PER_TOKEN) as usize;
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}{SKILL_BODY_TRUNCATION_MARKER}")
}

/// Render the selected skills into the markdown block the CLI injects as a
/// **volatile context message after the byte-stable system prefix** — never
/// baked into the cached system block, so prompt-cache hits on the prefix are
/// preserved (`09-lessons-learned.md` L-E8). Each skill contributes its name,
/// description, and body; bodies over [`SKILL_BODY_TOKEN_BUDGET`] are
/// truncated with a marker, and once the running total exceeds
/// [`SKILLS_SECTION_TOKEN_BUDGET`] the remaining (lower-ranked) skills are
/// dropped with a note. At least the top skill always renders. Empty input ⇒
/// empty string (inject nothing).
pub fn render_skills_section(selected: &[SelectedSkill]) -> String {
    if selected.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("\n## Applicable skills (selected for this task — apply the relevant ones)\n");
    let mut used_tokens: u64 = 0;
    let mut rendered_any = false;

    for sel in selected {
        let body = truncate_to_tokens(&sel.skill.body, SKILL_BODY_TOKEN_BUDGET);
        let block = format!(
            "\n### {}\n{}\n\n{}\n",
            sel.skill.name, sel.skill.description, body
        );
        let block_tokens = estimate_tokens(&block);
        if rendered_any && used_tokens + block_tokens > SKILLS_SECTION_TOKEN_BUDGET {
            out.push_str(SKILLS_SECTION_OMISSION_MARKER);
            break;
        }
        out.push_str(&block);
        used_tokens += block_tokens;
        rendered_any = true;
    }
    out
}

// ============================================================================
// Auto-creation — mining observations into new skills (the user requirement)
// ============================================================================

/// One mineable observation — a recurring style preference or reflection
/// lesson — already extracted from whatever store it came from. Mirrors
/// [`crate::rules::RawObservation`] but carries `domains` (skills are
/// domain-tagged) and no guard/file machinery (skills never hard-block).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillObservation {
    /// The lesson/preference text as observed.
    pub text: String,
    /// When it was observed (for evidence ordering / recency).
    pub occurred_at: u64,
    /// Domain tags carried by this observation, unioned into the candidate.
    pub domains: Vec<String>,
    /// Already elevated past a raw observation (e.g. an explicit user
    /// preference), decided by the caller — a single salient observation is
    /// enough to mine a candidate.
    pub salient: bool,
    /// An opaque reference (e.g. `trace:<turn>#lesson<i>` or `memory:<id>`)
    /// recorded in the candidate's evidence for auditing.
    pub reference: String,
}

/// Mining thresholds, mirroring [`crate::rules::MineConfig`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkillMineConfig {
    /// Minimum recurrences before a non-salient cluster becomes a candidate.
    pub min_occurrences: usize,
    /// Jaccard term-overlap threshold to cluster two observations as "the
    /// same".
    pub min_similarity: f64,
    /// Max candidates returned, ranked by score.
    pub limit: usize,
}

impl Default for SkillMineConfig {
    fn default() -> Self {
        Self {
            min_occurrences: 3,
            min_similarity: 0.5,
            limit: 10,
        }
    }
}

/// One recurrence backing a candidate, for auditing the mining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEvidence {
    pub reference: String,
    pub occurred_at: u64,
    /// The observation text at this occurrence, truncated to 160 chars.
    pub snippet: String,
}

/// A ranked auto-creation candidate — a skill the observations suggest
/// writing. [`render_skill_markdown`] turns it into a `SKILL.md` file;
/// [`decide_auto_creation`] decides whether to write it.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillCandidate {
    /// Slug — also the frontmatter `name:` and the filename stem.
    pub name: String,
    /// One-line summary written as `description:` frontmatter.
    pub description: String,
    /// Union of the cluster's domains (dedup'd case-insensitively).
    pub domains: Vec<String>,
    /// The representative lesson text — becomes the skill body.
    pub body: String,
    pub occurrences: usize,
    /// `true` when at least one backing observation was salient.
    pub salient: bool,
    pub evidence: Vec<SkillEvidence>,
    /// Ranking score, highest first.
    pub score: f64,
}

/// Mine observations into ranked auto-creation candidates. A cluster of
/// similar-enough observations (Jaccard ≥ `config.min_similarity`) qualifies
/// when either it recurred at least `config.min_occurrences` times, or any
/// occurrence is salient (a single strong signal — e.g. an explicit user
/// preference — is enough). Candidates whose text already matches an existing
/// skill are dropped. Deterministic across reruns on identical input.
pub fn mine_skill_candidates(
    observations: Vec<SkillObservation>,
    existing: &[Skill],
    config: &SkillMineConfig,
) -> Vec<SkillCandidate> {
    let clusters = cluster_observations(observations, config.min_similarity);
    let mut candidates: Vec<SkillCandidate> = Vec::new();

    for cluster in clusters {
        let salient = cluster.iter().any(|o| o.salient);
        if cluster.len() < config.min_occurrences && !salient {
            continue;
        }
        let Some(text) = representative_text(&cluster) else {
            continue;
        };
        if already_captured(&text, existing, config.min_similarity) {
            continue;
        }

        let domains = union_domains(&cluster);
        let mut sorted = cluster;
        sorted.sort_by_key(|o| std::cmp::Reverse(o.occurred_at));
        let occurrences = sorted.len();
        let evidence: Vec<SkillEvidence> = sorted
            .iter()
            .map(|o| SkillEvidence {
                reference: o.reference.clone(),
                occurred_at: o.occurred_at,
                snippet: o.text.chars().take(160).collect(),
            })
            .collect();

        let plural = if occurrences == 1 { "" } else { "s" };
        let salience_note = if salient {
            " (includes a salient signal)"
        } else {
            ""
        };
        let name = format!("{}-{}", slugify(&text), hash8(&text));

        candidates.push(SkillCandidate {
            name,
            description: format!("Learned from {occurrences} observation{plural}{salience_note}."),
            domains,
            occurrences,
            salient,
            evidence,
            score: occurrences as f64 * 10.0 + if salient { 50.0 } else { 0.0 },
            body: text,
        });
    }

    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.name.cmp(&b.name))
    });
    candidates.truncate(config.limit);
    candidates
}

/// Render the exact `SKILL.md` content for `candidate` — the same frontmatter
/// shape [`skill_from_file`] parses back, including the `origin: auto` marker
/// so a reload tags it [`SkillOrigin::AutoCreated`], plus an `## Evidence`
/// section listing the backing occurrences. Writing this to disk is the I/O
/// half `stella-cli` owns; this half is pure and round-trips through the
/// parser above.
pub fn render_skill_markdown(candidate: &SkillCandidate) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", candidate.name));
    out.push_str(&format!("description: {}\n", candidate.description));
    if !candidate.domains.is_empty() {
        out.push_str(&format!("domains: [{}]\n", candidate.domains.join(", ")));
    }
    out.push_str("origin: auto\n");
    out.push_str("---\n\n");
    out.push_str(&candidate.body);
    out.push_str("\n\n## Evidence\n\n");
    for e in &candidate.evidence {
        out.push_str(&format!(
            "- `{}` (observed at {}): {}\n",
            e.reference, e.occurred_at, e.snippet
        ));
    }
    out
}

/// Per-session auto-creation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCreateConfig {
    /// Max skills auto-created in one session. Kept small (default 2) on
    /// purpose: auto-creation must feel *magical, not spammy* — a session
    /// that silently spawns a dozen skill files would erode trust in the
    /// mechanism. The rest wait for the next session's mining pass.
    pub max_per_session: usize,
}

impl Default for AutoCreateConfig {
    fn default() -> Self {
        Self { max_per_session: 2 }
    }
}

/// Why an auto-creation was skipped — typed so the caller can log precisely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoCreateSkip {
    /// The per-session cap ([`AutoCreateConfig::max_per_session`]) was already
    /// reached.
    SessionCapReached { cap: usize },
    /// A file already exists at the target path — never clobber a hand-edited
    /// (or previously auto-created) skill.
    FileExists { path: String },
}

/// The decision for one candidate: write it, or skip with a typed reason.
/// Never performs I/O — the caller writes [`render_skill_markdown`]'s output
/// to `path` when this is `Create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoCreateDecision {
    Create { path: String },
    Skip { reason: AutoCreateSkip },
}

/// Decide whether to auto-create `candidate` as `<target_dir>/<name>.md`,
/// given how many skills were already created this session and which paths
/// already exist. The session cap is checked first (a capped session skips
/// regardless), then the no-clobber guard. Pure: writing is the caller's job.
pub fn decide_auto_creation(
    candidate: &SkillCandidate,
    target_dir: &str,
    existing_paths: &[String],
    created_this_session: usize,
    config: &AutoCreateConfig,
) -> AutoCreateDecision {
    if created_this_session >= config.max_per_session {
        return AutoCreateDecision::Skip {
            reason: AutoCreateSkip::SessionCapReached {
                cap: config.max_per_session,
            },
        };
    }
    let path = format!("{}/{}.md", target_dir.trim_end_matches('/'), candidate.name);
    if existing_paths.iter().any(|p| p == &path) {
        return AutoCreateDecision::Skip {
            reason: AutoCreateSkip::FileExists { path },
        };
    }
    AutoCreateDecision::Create { path }
}

// ============================================================================
// Install-proposal vocabulary (registry search + confirmed install)
// ============================================================================

/// A proposal to install a skill from a registry, produced by the
/// search/install glue after querying the registry and shown to the user for
/// confirmation. Kept minimal — the actual `npx skills`-style subprocess and
/// the `ask_user` confirmation live in `stella-cli`; this is just the shared
/// shape the glue and a future TUI both speak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInstallProposal {
    /// What the user searched the registry for.
    pub query: String,
    /// A human-readable summary of what the registry returned (e.g. the top
    /// match's name + description), for the confirmation prompt.
    pub registry_result_summary: String,
    /// Where the glue would install it (e.g. the workspace skills dir).
    pub target_dir_hint: String,
}

/// The user's answer to a [`SkillInstallProposal`]. Minimal by design — the
/// glue maps this onto the actual install/no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDecision {
    /// Proceed with the install into the proposal's target directory.
    Confirmed,
    /// Do nothing.
    Declined,
}

// ============================================================================
// Local lexical helpers (reimplemented from `crate::rules`'s private ones —
// see the module doc: `rules.rs` must not be edited to widen their visibility)
// ============================================================================

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "of", "in", "on", "for", "with", "is", "are", "be",
    "this", "that", "it", "as", "at", "by", "from", "into", "we", "you", "i", "was", "were", "has",
    "have", "had", "not", "but", "if", "so", "then", "than", "when", "where", "which", "will",
    "would", "should", "did",
];

/// Split text into lowercased, de-stopped terms (>2 chars) for lexical
/// scoring/clustering.
fn terms(text: &str) -> Vec<String> {
    let stopwords: HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            if current.len() > 2 && !stopwords.contains(current.as_str()) {
                out.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() > 2 && !stopwords.contains(current.as_str()) {
        out.push(current);
    }
    out
}

/// Jaccard similarity of two term sets — 0 when either is empty.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Filesystem/name-safe slug: lowercase, alnum + dashes, capped short.
fn slugify(text: &str) -> String {
    let mut collapsed = String::new();
    let mut prev_dash = false;
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            collapsed.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            collapsed.push('-');
            prev_dash = true;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    let truncated: String = trimmed.chars().take(40).collect();
    let truncated = truncated.trim_end_matches('-');
    if truncated.is_empty() {
        "skill".to_string()
    } else {
        truncated.to_string()
    }
}

/// Short deterministic content hash (FNV-1a 64-bit, lower 32 bits as 8 hex
/// chars) so re-mining identical data yields the same candidate name. Not
/// cryptographic — purely a stable id, and dependency-free (no hash crate is
/// a workspace dependency).
fn hash8(text: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:08x}", (hash & 0xffff_ffff) as u32)
}

/// The cluster's most representative wording: the most-repeated exact text,
/// longest wins ties. `None` only for an empty cluster (never constructed by
/// [`mine_skill_candidates`]).
fn representative_text(cluster: &[SkillObservation]) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for o in cluster {
        *counts.entry(o.text.as_str()).or_insert(0) += 1;
    }
    let mut best = cluster.first()?.text.as_str();
    let mut best_count = 0usize;
    for (text, count) in &counts {
        if *count > best_count || (*count == best_count && text.len() > best.len()) {
            best = text;
            best_count = *count;
        }
    }
    Some(best.to_string())
}

/// Union of every domain across a cluster, dedup'd case-insensitively,
/// first-seen casing preserved, order stable.
fn union_domains(cluster: &[SkillObservation]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for o in cluster {
        for d in &o.domains {
            if !out.iter().any(|existing| existing.eq_ignore_ascii_case(d)) {
                out.push(d.clone());
            }
        }
    }
    out
}

/// `true` when an existing skill already says essentially the same thing —
/// its name+description+body terms overlap the candidate text past
/// `min_similarity`. Compared against the whole existing skill (not just its
/// name+description) so a differently-titled but same-content skill still
/// suppresses the duplicate.
fn already_captured(text: &str, existing: &[Skill], min_similarity: f64) -> bool {
    let t: HashSet<String> = terms(text).into_iter().collect();
    existing.iter().any(|s| {
        let haystack = format!("{} {} {}", s.name, s.description, s.body);
        let st: HashSet<String> = terms(&haystack).into_iter().collect();
        jaccard(&t, &st) >= min_similarity
    })
}

/// Greedy single-pass clustering: each observation joins the first cluster
/// whose *first* member's term set overlaps it enough, else starts a new one.
/// `O(n × clusters)` — fine at CLI-local data volumes.
fn cluster_observations(
    observations: Vec<SkillObservation>,
    min_similarity: f64,
) -> Vec<Vec<SkillObservation>> {
    let mut clusters: Vec<Vec<SkillObservation>> = Vec::new();
    for obs in observations {
        let obs_terms: HashSet<String> = terms(&obs.text).into_iter().collect();
        let home = clusters.iter().position(|c| {
            let head_terms: HashSet<String> = terms(&c[0].text).into_iter().collect();
            jaccard(&obs_terms, &head_terms) >= min_similarity
        });
        match home {
            Some(idx) => clusters[idx].push(obs),
            None => clusters.push(vec![obs]),
        }
    }
    clusters
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parsing: frontmatter, layouts, diagnostics ----

    #[test]
    fn parses_name_description_domains_and_body() {
        let raw = "---\nname: sql-style\ndescription: Format SQL with lowercase keywords\ndomains: sql, formatting\n---\nUse lowercase keywords and two-space indentation.";
        let skill = skill_from_file(".stella/skills/sql-style.md", raw).unwrap();
        assert_eq!(skill.name, "sql-style");
        assert_eq!(skill.description, "Format SQL with lowercase keywords");
        assert_eq!(skill.domains, vec!["sql", "formatting"]);
        assert!(skill.body.contains("lowercase keywords"));
    }

    #[test]
    fn parses_bracketed_domains_list_and_strips_quotes() {
        let raw =
            "---\nname: t\ndescription: \"quoted desc\"\ndomains: [\"sql\", 'graph']\n---\nbody";
        let skill = skill_from_file("t.md", raw).unwrap();
        assert_eq!(skill.description, "quoted desc");
        assert_eq!(skill.domains, vec!["sql", "graph"]);
    }

    #[test]
    fn ignores_unknown_frontmatter_keys() {
        let raw = "---\nname: t\ndescription: d\nunknown-key: whatever\nanother: 123\n---\nbody";
        let skill = skill_from_file("t.md", raw).unwrap();
        assert_eq!(skill.name, "t");
        assert_eq!(skill.description, "d");
    }

    #[test]
    fn missing_description_is_a_typed_diagnostic() {
        let err = skill_from_file("t.md", "---\nname: t\n---\nbody").unwrap_err();
        assert_eq!(err.problem, SkillProblem::MissingDescription);
        assert_eq!(err.path, "t.md");
    }

    #[test]
    fn empty_body_is_a_typed_diagnostic() {
        let err = skill_from_file("t.md", "---\nname: t\ndescription: d\n---\n").unwrap_err();
        assert_eq!(err.problem, SkillProblem::EmptyBody);
    }

    #[test]
    fn missing_name_with_unusable_path_is_a_typed_diagnostic() {
        // Bare `SKILL.md` with no parent dir and no frontmatter name yields
        // no usable slug.
        let err = skill_from_file("SKILL.md", "just a body, no frontmatter").unwrap_err();
        assert_eq!(err.problem, SkillProblem::MissingName);
    }

    #[test]
    fn nested_layout_derives_slug_from_parent_directory() {
        // The `npx skills` ecosystem layout: <dir>/<slug>/SKILL.md.
        let skill = skill_from_file(
            "/ws/.stella/skills/pdf-extract/SKILL.md",
            "---\ndescription: d\n---\nbody",
        )
        .unwrap();
        assert_eq!(skill.name, "pdf-extract");
    }

    #[test]
    fn flat_layout_derives_slug_from_filename_stem() {
        let skill = skill_from_file(
            "/ws/.stella/skills/pdf-extract.md",
            "---\ndescription: d\n---\nbody",
        )
        .unwrap();
        assert_eq!(skill.name, "pdf-extract");
    }

    #[test]
    fn frontmatter_name_overrides_the_path_slug() {
        let skill = skill_from_file(
            "/ws/.stella/skills/whatever/SKILL.md",
            "---\nname: real-name\ndescription: d\n---\nbody",
        )
        .unwrap();
        assert_eq!(skill.name, "real-name");
    }

    #[test]
    fn origin_marker_auto_and_installed_are_read_back() {
        let auto = skill_from_file(
            "a.md",
            "---\nname: a\ndescription: d\norigin: auto\n---\nbody",
        )
        .unwrap();
        assert_eq!(auto.origin, SkillOrigin::AutoCreated);
        let installed = skill_from_file(
            "b.md",
            "---\nname: b\ndescription: d\norigin: installed\n---\nbody",
        )
        .unwrap();
        assert_eq!(installed.origin, SkillOrigin::Installed);
    }

    // ---- discovery + precedence, against a fake SkillSource ----

    struct FakeSkillSource {
        by_dir: HashMap<String, Vec<SkillFile>>,
    }

    impl SkillSource for FakeSkillSource {
        fn read_skill_files(&self, roots: &[String]) -> Vec<SkillFile> {
            let mut out = Vec::new();
            for root in roots {
                if let Some(files) = self.by_dir.get(root) {
                    out.extend(files.iter().cloned());
                }
            }
            out
        }
    }

    fn opts() -> LoadSkillsOptions {
        LoadSkillsOptions {
            workspace_skills_dir: "/ws/.stella/skills".to_string(),
            user_skills_dir: "/home/u/.config/stella/skills".to_string(),
        }
    }

    fn skill_file(dir: &str, name: &str, content: &str) -> SkillFile {
        SkillFile {
            path: format!("{dir}/{name}"),
            content: content.to_string(),
        }
    }

    #[test]
    fn loads_a_skill_end_to_end_with_workspace_origin() {
        let o = opts();
        let dirs = skill_search_dirs(&o);
        let mut by_dir = HashMap::new();
        by_dir.insert(
            dirs[1].clone(), // workspace dir
            vec![skill_file(
                &dirs[1],
                "tables.md",
                "---\nname: prefer-tables\ndescription: Prefer tables over prose for comparisons\n---\nWhen comparing options, render a table.",
            )],
        );
        let source = FakeSkillSource { by_dir };
        let skills = load_skills(&source, &o);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "prefer-tables");
        assert_eq!(skills[0].origin, SkillOrigin::Workspace);
    }

    #[test]
    fn workspace_beats_user_global_on_a_name_collision() {
        let o = opts();
        let dirs = skill_search_dirs(&o);
        let mut by_dir = HashMap::new();
        by_dir.insert(
            dirs[0].clone(), // user dir
            vec![skill_file(
                &dirs[0],
                "s.md",
                "---\nname: s\ndescription: user version\n---\nuser body",
            )],
        );
        by_dir.insert(
            dirs[1].clone(), // workspace dir
            vec![skill_file(
                &dirs[1],
                "s.md",
                "---\nname: s\ndescription: workspace version\n---\nworkspace body",
            )],
        );
        let source = FakeSkillSource { by_dir };
        let skills = load_skills(&source, &o);
        let s = skills.iter().find(|s| s.name == "s").unwrap();
        assert_eq!(s.description, "workspace version");
        assert_eq!(s.origin, SkillOrigin::Workspace);
    }

    #[test]
    fn load_with_diagnostics_surfaces_skipped_files() {
        let o = opts();
        let dirs = skill_search_dirs(&o);
        let mut by_dir = HashMap::new();
        by_dir.insert(
            dirs[1].clone(),
            vec![
                skill_file(
                    &dirs[1],
                    "ok.md",
                    "---\nname: ok\ndescription: d\n---\nbody",
                ),
                skill_file(&dirs[1], "bad.md", "---\nname: bad\n---\nbody"), // no description
            ],
        );
        let source = FakeSkillSource { by_dir };
        let loaded = load_skills_with_diagnostics(&source, &o);
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.diagnostics.len(), 1);
        assert_eq!(
            loaded.diagnostics[0].problem,
            SkillProblem::MissingDescription
        );
    }

    fn skill(name: &str, description: &str, domains: &[&str], origin: SkillOrigin) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            domains: domains.iter().map(|d| d.to_string()).collect(),
            body: format!("body of {name}"),
            source_path: format!("{name}.md"),
            origin,
        }
    }

    // ---- selection ----

    #[test]
    fn selection_populates_the_why_fields() {
        let skills = vec![skill(
            "sql-style",
            "format sql queries nicely",
            &["sql"],
            SkillOrigin::Workspace,
        )];
        let selected = select_skills(
            &skills,
            "please format the sql for me",
            &["sql".to_string()],
            &SelectionConfig::default(),
        );
        assert_eq!(selected.len(), 1);
        assert!(selected[0].matched_terms.contains(&"sql".to_string()));
        assert!(selected[0].matched_terms.contains(&"format".to_string()));
        assert_eq!(selected[0].matched_domains, vec!["sql"]);
    }

    #[test]
    fn domain_boost_wins_over_a_weak_lexical_match() {
        let skills = vec![
            // No domain, weak lexical overlap with the prompt.
            skill(
                "verbose-logging",
                "add verbose logging everywhere",
                &[],
                SkillOrigin::Workspace,
            ),
            // Domain match, but its wording barely overlaps the prompt.
            skill(
                "migration-safety",
                "irreversible ddl needs review",
                &["database"],
                SkillOrigin::Workspace,
            ),
        ];
        let selected = select_skills(
            &skills,
            "add verbose stuff",
            &["database".to_string()],
            &SelectionConfig::default(),
        );
        // The domain-matched skill should rank first despite weaker wording.
        assert_eq!(selected[0].skill.name, "migration-safety");
    }

    #[test]
    fn below_threshold_skills_are_excluded() {
        let skills = vec![skill(
            "unrelated",
            "something about kubernetes pods",
            &[],
            SkillOrigin::Workspace,
        )];
        let selected = select_skills(
            &skills,
            "help me write a poem about the ocean",
            &[],
            &SelectionConfig::default(),
        );
        assert!(selected.is_empty());
    }

    #[test]
    fn top_k_is_respected() {
        let skills: Vec<Skill> = (0..5)
            .map(|i| {
                skill(
                    &format!("sql-skill-{i}"),
                    "format sql queries",
                    &["sql"],
                    SkillOrigin::Workspace,
                )
            })
            .collect();
        let config = SelectionConfig {
            max_skills: 2,
            ..SelectionConfig::default()
        };
        let selected = select_skills(&skills, "format sql", &["sql".to_string()], &config);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn auto_created_skill_wins_the_tie_break() {
        // Two skills with identical wording/domain → identical base score; the
        // AutoCreated one (a freshly-learned preference) sorts first.
        let skills = vec![
            skill(
                "z-workspace",
                "prefer tables for comparisons",
                &[],
                SkillOrigin::Workspace,
            ),
            skill(
                "a-auto",
                "prefer tables for comparisons",
                &[],
                SkillOrigin::AutoCreated,
            ),
        ];
        let selected = select_skills(
            &skills,
            "prefer tables for comparisons please",
            &[],
            &SelectionConfig::default(),
        );
        assert_eq!(selected[0].skill.name, "a-auto");
        assert!(selected[0].score > selected[1].score);
    }

    // ---- rendering ----

    #[test]
    fn render_section_is_empty_with_no_skills() {
        assert_eq!(render_skills_section(&[]), "");
    }

    #[test]
    fn render_section_includes_name_description_and_body() {
        let selected = vec![SelectedSkill {
            skill: skill("s", "the description", &[], SkillOrigin::Workspace),
            score: 1.0,
            matched_terms: vec![],
            matched_domains: vec![],
        }];
        let out = render_skills_section(&selected);
        assert!(out.contains("Applicable skills"));
        assert!(out.contains("### s"));
        assert!(out.contains("the description"));
        assert!(out.contains("body of s"));
    }

    #[test]
    fn render_section_truncates_an_over_budget_body_with_a_marker() {
        let big_body = "word ".repeat(2000); // ~2857 tokens, over the 400 budget
        let mut s = skill("s", "d", &[], SkillOrigin::Workspace);
        s.body = big_body;
        let selected = vec![SelectedSkill {
            skill: s,
            score: 1.0,
            matched_terms: vec![],
            matched_domains: vec![],
        }];
        let out = render_skills_section(&selected);
        assert!(out.contains("truncated to fit the context budget"));
    }

    #[test]
    fn render_section_drops_extra_skills_past_the_total_budget() {
        // Six skills each near the per-skill budget overrun the section total.
        let selected: Vec<SelectedSkill> = (0..6)
            .map(|i| {
                let mut s = skill(&format!("s{i}"), "d", &[], SkillOrigin::Workspace);
                s.body = "word ".repeat(300); // ~430 tokens each after estimate
                SelectedSkill {
                    skill: s,
                    score: 1.0,
                    matched_terms: vec![],
                    matched_domains: vec![],
                }
            })
            .collect();
        let out = render_skills_section(&selected);
        assert!(out.contains("additional lower-ranked skills omitted"));
        // At least the first skill always renders.
        assert!(out.contains("### s0"));
    }

    // ---- mining ----

    fn observation(text: &str, occurred_at: u64) -> SkillObservation {
        SkillObservation {
            text: text.to_string(),
            occurred_at,
            domains: vec![],
            salient: false,
            reference: format!("trace:t#{occurred_at}"),
        }
    }

    #[test]
    fn mines_a_recurring_preference_at_the_threshold() {
        let obs = vec![
            observation("the user prefers tables over prose for comparisons", 1),
            observation("the user prefers tables over prose for comparisons", 2),
            observation("the user prefers tables over prose for comparisons", 3),
        ];
        let candidates = mine_skill_candidates(obs, &[], &SkillMineConfig::default());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].occurrences, 3);
        assert_eq!(candidates[0].evidence.len(), 3);
        assert!(candidates[0].body.contains("prefers tables"));
    }

    #[test]
    fn does_not_mine_a_one_off_below_the_threshold() {
        let obs = vec![observation("a one-time thing nobody repeated", 1)];
        assert!(mine_skill_candidates(obs, &[], &SkillMineConfig::default()).is_empty());
    }

    #[test]
    fn mines_a_single_salient_observation() {
        let obs = vec![SkillObservation {
            text: "always use pnpm, never npm, in this repo".to_string(),
            occurred_at: 1,
            domains: vec!["tooling".to_string()],
            salient: true,
            reference: "memory:m1".to_string(),
        }];
        let candidates = mine_skill_candidates(obs, &[], &SkillMineConfig::default());
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].salient);
        assert_eq!(candidates[0].occurrences, 1);
        assert_eq!(candidates[0].domains, vec!["tooling"]);
    }

    #[test]
    fn dedups_against_an_existing_skill() {
        let obs = vec![
            observation("prefer tables over prose for comparisons", 1),
            observation("prefer tables over prose for comparisons", 2),
            observation("prefer tables over prose for comparisons", 3),
        ];
        let existing = vec![skill(
            "tables",
            "Prefer tables over prose for comparisons",
            &[],
            SkillOrigin::Workspace,
        )];
        assert!(mine_skill_candidates(obs, &existing, &SkillMineConfig::default()).is_empty());
    }

    #[test]
    fn mining_unions_domains_across_the_cluster() {
        let obs = vec![
            SkillObservation {
                text: "format sql with lowercase keywords".to_string(),
                occurred_at: 1,
                domains: vec!["sql".to_string()],
                salient: false,
                reference: "t#1".to_string(),
            },
            SkillObservation {
                text: "format sql with lowercase keywords".to_string(),
                occurred_at: 2,
                domains: vec!["formatting".to_string()],
                salient: false,
                reference: "t#2".to_string(),
            },
            SkillObservation {
                text: "format sql with lowercase keywords".to_string(),
                occurred_at: 3,
                domains: vec!["SQL".to_string()], // case-insensitive dup of "sql"
                salient: false,
                reference: "t#3".to_string(),
            },
        ];
        let candidates = mine_skill_candidates(obs, &[], &SkillMineConfig::default());
        assert_eq!(candidates[0].domains, vec!["sql", "formatting"]);
    }

    #[test]
    fn mining_respects_the_limit() {
        let lessons = [
            "always add a regression test for every bug fix",
            "database credentials must never appear in log output",
            "team members must not force push shared branches",
            "response handlers should validate required null fields",
            "queries must always use the correct tenant scope",
        ];
        let mut obs = Vec::new();
        for lesson in lessons {
            for occurrence in 0..3u64 {
                obs.push(observation(lesson, occurrence));
            }
        }
        let config = SkillMineConfig {
            limit: 2,
            ..SkillMineConfig::default()
        };
        assert_eq!(mine_skill_candidates(obs, &[], &config).len(), 2);
    }

    #[test]
    fn mining_is_deterministic_across_reruns() {
        let obs = vec![
            observation("prefer tables over prose for comparisons", 1),
            observation("prefer tables over prose for comparisons", 2),
            observation("prefer tables over prose for comparisons", 3),
        ];
        let first = mine_skill_candidates(obs.clone(), &[], &SkillMineConfig::default());
        let second = mine_skill_candidates(obs, &[], &SkillMineConfig::default());
        assert_eq!(first[0].name, second[0].name);
    }

    // ---- markdown round-trip ----

    #[test]
    fn rendered_markdown_round_trips_through_the_parser() {
        let obs = vec![SkillObservation {
            text: "prefer tables over prose for comparisons".to_string(),
            occurred_at: 1,
            domains: vec!["formatting".to_string(), "docs".to_string()],
            salient: true,
            reference: "memory:m1".to_string(),
        }];
        let candidates = mine_skill_candidates(obs, &[], &SkillMineConfig::default());
        let candidate = &candidates[0];
        let markdown = render_skill_markdown(candidate);
        let parsed = skill_from_file(&format!("{}.md", candidate.name), &markdown).unwrap();
        assert_eq!(parsed.name, candidate.name);
        assert_eq!(parsed.description, candidate.description);
        assert_eq!(parsed.domains, candidate.domains);
        // The `origin: auto` marker means a reload tags it AutoCreated.
        assert_eq!(parsed.origin, SkillOrigin::AutoCreated);
        assert!(parsed.body.contains("prefer tables"));
        assert!(parsed.body.contains("## Evidence"));
    }

    // ---- auto-creation decision ----

    fn a_candidate() -> SkillCandidate {
        SkillCandidate {
            name: "prefer-tables".to_string(),
            description: "Learned from 3 observations.".to_string(),
            domains: vec![],
            body: "prefer tables".to_string(),
            occurrences: 3,
            salient: false,
            evidence: vec![],
            score: 30.0,
        }
    }

    #[test]
    fn auto_create_writes_when_absent_and_under_the_cap() {
        let decision = decide_auto_creation(
            &a_candidate(),
            "/ws/.stella/skills",
            &[],
            0,
            &AutoCreateConfig::default(),
        );
        assert_eq!(
            decision,
            AutoCreateDecision::Create {
                path: "/ws/.stella/skills/prefer-tables.md".to_string()
            }
        );
    }

    #[test]
    fn auto_create_refuses_to_clobber_an_existing_file() {
        let existing = vec!["/ws/.stella/skills/prefer-tables.md".to_string()];
        let decision = decide_auto_creation(
            &a_candidate(),
            "/ws/.stella/skills",
            &existing,
            0,
            &AutoCreateConfig::default(),
        );
        assert!(matches!(
            decision,
            AutoCreateDecision::Skip {
                reason: AutoCreateSkip::FileExists { .. }
            }
        ));
    }

    #[test]
    fn auto_create_stops_at_the_session_cap() {
        let decision = decide_auto_creation(
            &a_candidate(),
            "/ws/.stella/skills",
            &[],
            2, // already created 2 this session
            &AutoCreateConfig::default(),
        );
        assert_eq!(
            decision,
            AutoCreateDecision::Skip {
                reason: AutoCreateSkip::SessionCapReached { cap: 2 }
            }
        );
    }

    // ---- install vocabulary ----

    #[test]
    fn install_proposal_and_decision_are_constructible() {
        let proposal = SkillInstallProposal {
            query: "pdf extraction".to_string(),
            registry_result_summary: "pdf-extract — extract text and tables from PDFs".to_string(),
            target_dir_hint: "/ws/.stella/skills".to_string(),
        };
        assert_eq!(proposal.query, "pdf extraction");
        assert_ne!(InstallDecision::Confirmed, InstallDecision::Declined);
    }

    // ---- proptest: render/parse round-trip over awkward content ----

    proptest::proptest! {
        #[test]
        fn render_parse_round_trip_preserves_name_description_domains(
            name in "[a-z][a-z0-9-]{0,29}",
            description in "[a-zA-Z0-9 .,!?()-]{1,60}",
            domains in proptest::collection::vec("[a-z][a-z0-9-]{0,15}", 0..4),
        ) {
            // `description:` is trimmed on parse, so an all-whitespace one
            // would fail to load — skip those (never produced by mining).
            proptest::prop_assume!(!description.trim().is_empty());

            let candidate = SkillCandidate {
                name: name.clone(),
                description: description.clone(),
                domains: domains.clone(),
                body: "some body text".to_string(),
                occurrences: 1,
                salient: false,
                evidence: vec![],
                score: 10.0,
            };
            let markdown = render_skill_markdown(&candidate);
            let parsed = skill_from_file(&format!("{name}.md"), &markdown).unwrap();
            proptest::prop_assert_eq!(&parsed.name, &name);
            proptest::prop_assert_eq!(&parsed.description, description.trim());
            proptest::prop_assert_eq!(&parsed.domains, &domains);
            proptest::prop_assert_eq!(parsed.origin, SkillOrigin::AutoCreated);
        }
    }
}
