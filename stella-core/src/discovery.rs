//! Discovery relevance engine — the shared ranker behind the agent's
//! `tool_search` / `skill_search` / `mcp_search` tools.
//!
//! The product problem: a workspace will eventually carry hundreds of tools
//! (native + MCP + custom) and thousands of skills. Frontloading all of them
//! into every agent burns context and buries the relevant few, so agents are
//! given *search* tools instead — and the search has to be genuinely good at
//! "best fit for the purpose", not a bare substring match.
//!
//! This module is that ranker, once, for every catalog kind. Callers flatten
//! whatever they search (tool schemas, skills, MCP servers) into
//! [`Candidate`]s and get back a deterministic, inspectable ranking. Like the
//! rest of `stella-core` it performs no I/O and holds no state — plain
//! synchronous logic over owned data, so the scoring is unit-testable without
//! a live toolset.
//!
//! # Query language
//!
//! Two forms, mirroring what agents already know from tool-search harnesses:
//!
//! - `select:read_file,bash` — exact id lookup (case-insensitive), returned
//!   in the requested order. For when the caller already knows the name.
//! - free keywords, where a `+` prefix marks a term as **required** and
//!   required terms must hit the candidate's *name or keywords* (not merely
//!   its description): `+slack send message`.
//!
//! # Scoring
//!
//! Lexical, field-weighted, and deliberately boring: per query term the best
//! of (name-token match ≫ keyword match ≫ description match), tiered
//! exact > prefix > substring, averaged over the query's terms, plus small
//! bonuses for whole-query name hits and full term coverage. A noise gate
//! drops candidates that only graze a single description word — with
//! thousands of candidates, one shared common word is meaningless. No
//! embeddings and no network: rankings must be reproducible in tests and
//! cheap enough to run on every call.

use std::collections::HashSet;

/// One searchable catalog entry, flattened by the caller from whatever it
/// indexes (a tool schema, a skill, an MCP server).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Stable identifier `select:` matches against (tool name, skill name,
    /// server name).
    pub id: String,
    /// The name scored as the strongest field. Usually equals `id`; a caller
    /// may enrich it (e.g. an MCP server's alias plus its title).
    pub name: String,
    /// Prose description — the weakest field, still needed for "what does
    /// this do" queries.
    pub description: String,
    /// Extra high-signal terms: skill domains, an MCP server's tool names,
    /// a source label. Scored between name and description.
    pub keywords: Vec<String>,
}

/// One ranked hit: an index into the caller's candidate slice, the score,
/// and *why* — the query terms that matched — so a ranking is inspectable
/// rather than a black box (same discipline as `skills::SelectedSkill`).
#[derive(Debug, Clone, PartialEq)]
pub struct RankedMatch {
    pub index: usize,
    pub score: f64,
    /// Query terms that scored against this candidate, sorted.
    pub matched_terms: Vec<String>,
}

/// A parsed query — see the module docs for the two forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryQuery {
    /// `select:a,b` — exact ids, in the requested order.
    Select(Vec<String>),
    /// Free keywords; terms flagged `required` came with a `+` prefix.
    Keywords(Vec<QueryTerm>),
}

/// One keyword term. `required` terms must match the candidate's name or
/// keywords — a candidate matching them only in its description is excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    pub term: String,
    pub required: bool,
}

/// Score floor — below this a candidate is noise, not a result.
const MIN_SCORE: f64 = 0.1;

/// Per-term match tiers for the name field. Keywords score the same tiers
/// scaled by [`KEYWORD_FACTOR`]; the description has its own flatter tiers.
const NAME_EXACT: f64 = 1.0;
const NAME_TERM_PREFIX: f64 = 0.72;
const NAME_TOKEN_PREFIX: f64 = 0.6;
const NAME_SUBSTRING: f64 = 0.45;
const KEYWORD_FACTOR: f64 = 0.85;
const DESC_EXACT: f64 = 0.5;
const DESC_TERM_PREFIX: f64 = 0.32;
const DESC_TOKEN_PREFIX: f64 = 0.25;

/// Bonus when the whole query IS the candidate's name (`read_file` typed
/// verbatim must beat every partial match), when the raw query appears as a
/// substring of the name, and when every term of a multi-term query matched.
const WHOLE_NAME_BONUS: f64 = 0.6;
const NAME_SUBSTRING_BONUS: f64 = 0.25;
const FULL_COVERAGE_BONUS: f64 = 0.1;

/// Split text into lowercase match tokens: boundaries at any
/// non-alphanumeric character plus lower→upper camelCase transitions
/// (`listCommits` → `list`, `commits`), single-character fragments dropped.
/// This is what makes `mcp__github__search_issues` findable by "github" or
/// "issues" without the caller pre-splitting anything.
pub fn split_terms(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower && !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            prev_lower = ch.is_lowercase() || ch.is_numeric();
            current.extend(ch.to_lowercase());
        } else {
            prev_lower = false;
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens.retain(|t| t.chars().count() >= 2);
    tokens
}

/// Parse a raw query into its [`DiscoveryQuery`] form. Empty input yields an
/// empty `Keywords` list — the caller decides how to surface "no query".
pub fn parse_query(raw: &str) -> DiscoveryQuery {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("select:") {
        let ids = rest
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        return DiscoveryQuery::Select(ids);
    }
    let mut terms: Vec<QueryTerm> = Vec::new();
    for word in trimmed.split_whitespace() {
        let required = word.starts_with('+');
        let bare = word.trim_start_matches('+');
        for term in split_terms(bare) {
            match terms.iter_mut().find(|t| t.term == term) {
                // A term both bare and `+`-marked is required — the stricter
                // intent wins.
                Some(existing) => existing.required |= required,
                None => terms.push(QueryTerm { term, required }),
            }
        }
    }
    DiscoveryQuery::Keywords(terms)
}

/// A candidate's pre-tokenized fields, built once per ranking pass.
struct Doc {
    name_tokens: Vec<String>,
    keyword_tokens: Vec<String>,
    desc_tokens: Vec<String>,
    full_name: String,
}

impl Doc {
    fn build(candidate: &Candidate) -> Self {
        let mut keyword_tokens = Vec::new();
        for keyword in &candidate.keywords {
            keyword_tokens.extend(split_terms(keyword));
        }
        Self {
            name_tokens: split_terms(&candidate.name),
            keyword_tokens,
            desc_tokens: split_terms(&candidate.description),
            full_name: candidate.name.trim().to_lowercase(),
        }
    }
}

/// Best score of `term` against one token list, using the given tiers:
/// exact, term-is-prefix-of-token, token-is-prefix-of-term, token-contains-
/// term. The length floors keep 2-char fragments from prefix-matching half
/// the catalog.
fn field_score(term: &str, tokens: &[String], tiers: (f64, f64, f64, f64)) -> f64 {
    let (exact, term_prefix, token_prefix, substring) = tiers;
    let mut best = 0.0f64;
    for token in tokens {
        let score = if token == term {
            exact
        } else if term.len() >= 3 && token.starts_with(term) {
            term_prefix
        } else if token.len() >= 3 && term.starts_with(token.as_str()) {
            token_prefix
        } else if term.len() >= 4 && token.contains(term) {
            substring
        } else {
            0.0
        };
        best = best.max(score);
    }
    best
}

/// Rank `candidates` against `raw_query`, best first, capped at `limit`.
///
/// `select:` queries return exact-id hits (score 1.0) in the requested
/// order; unknown ids are simply absent — compare against
/// [`parse_query`]'s id list to report them. Keyword queries score as the
/// module docs describe. Ties break by candidate name then index, so equal
/// inputs always produce byte-identical output.
pub fn rank(raw_query: &str, candidates: &[Candidate], limit: usize) -> Vec<RankedMatch> {
    match parse_query(raw_query) {
        DiscoveryQuery::Select(ids) => {
            let mut out = Vec::new();
            for id in &ids {
                if let Some(index) = candidates
                    .iter()
                    .position(|c| c.id.eq_ignore_ascii_case(id) || c.name.eq_ignore_ascii_case(id))
                    && out.iter().all(|m: &RankedMatch| m.index != index)
                {
                    out.push(RankedMatch {
                        index,
                        score: 1.0,
                        matched_terms: vec![id.to_lowercase()],
                    });
                }
            }
            out.truncate(limit);
            out
        }
        DiscoveryQuery::Keywords(terms) => rank_keywords(raw_query, &terms, candidates, limit),
    }
}

fn rank_keywords(
    raw_query: &str,
    terms: &[QueryTerm],
    candidates: &[Candidate],
    limit: usize,
) -> Vec<RankedMatch> {
    if terms.is_empty() {
        return Vec::new();
    }
    let raw_lower = raw_query.trim().to_lowercase();
    let query_joined = terms
        .iter()
        .map(|t| t.term.as_str())
        .collect::<Vec<_>>()
        .join("_");

    let mut hits: Vec<(RankedMatch, String)> = Vec::new();
    'candidates: for (index, candidate) in candidates.iter().enumerate() {
        let doc = Doc::build(candidate);
        let mut sum = 0.0f64;
        let mut matched: HashSet<String> = HashSet::new();
        let mut strong_hit = false; // any term that hit name/keywords or an exact description word
        for qt in terms {
            let name = field_score(
                &qt.term,
                &doc.name_tokens,
                (
                    NAME_EXACT,
                    NAME_TERM_PREFIX,
                    NAME_TOKEN_PREFIX,
                    NAME_SUBSTRING,
                ),
            );
            let keyword = KEYWORD_FACTOR
                * field_score(
                    &qt.term,
                    &doc.keyword_tokens,
                    (
                        NAME_EXACT,
                        NAME_TERM_PREFIX,
                        NAME_TOKEN_PREFIX,
                        NAME_SUBSTRING,
                    ),
                );
            let desc = field_score(
                &qt.term,
                &doc.desc_tokens,
                (DESC_EXACT, DESC_TERM_PREFIX, DESC_TOKEN_PREFIX, 0.0),
            );
            // A required term that misses the name AND keywords excludes the
            // candidate outright — `+github` means github, not a description
            // that happens to mention it.
            if qt.required && name <= 0.0 && keyword <= 0.0 {
                continue 'candidates;
            }
            let best = name.max(keyword).max(desc);
            if best > 0.0 {
                matched.insert(qt.term.clone());
                sum += best;
            }
            if name > 0.0 || keyword > 0.0 || best >= DESC_EXACT {
                strong_hit = true;
            }
        }
        // Noise gate: one grazed description word is not a result.
        if matched.is_empty() || (!strong_hit && matched.len() < 2) {
            continue;
        }

        let mut score = sum / terms.len() as f64;
        if !doc.full_name.is_empty()
            && (doc.full_name == raw_lower || doc.name_tokens.join("_") == query_joined)
        {
            score += WHOLE_NAME_BONUS;
        } else if raw_lower.len() >= 3 && doc.full_name.contains(&raw_lower) {
            score += NAME_SUBSTRING_BONUS;
        }
        if terms.len() >= 2 && matched.len() == terms.len() {
            score += FULL_COVERAGE_BONUS;
        }
        if score < MIN_SCORE {
            continue;
        }

        let mut matched_terms: Vec<String> = matched.into_iter().collect();
        matched_terms.sort();
        hits.push((
            RankedMatch {
                index,
                score,
                matched_terms,
            },
            candidate.name.clone(),
        ));
    }

    hits.sort_by(|(a, a_name), (b, b_name)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a_name.cmp(b_name))
            .then_with(|| a.index.cmp(&b.index))
    });
    hits.truncate(limit);
    hits.into_iter().map(|(m, _)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> Candidate {
        Candidate {
            id: name.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            keywords: Vec::new(),
        }
    }

    /// A realistic mixed catalog: native tools plus MCP-namespaced ones.
    fn catalog() -> Vec<Candidate> {
        vec![
            tool("read_file", "Read a file with line numbers"),
            tool("write_file", "Create or overwrite a file"),
            tool("edit_file", "Replace an exact substring in a file"),
            tool("bash", "Run a shell command in the workspace root"),
            tool("grep", "Search file contents with regex"),
            tool("screenshot", "Capture a screenshot of the workspace app"),
            tool(
                "search_issues",
                "Search the configured issue tracker by keywords and labels",
            ),
            tool("create_issue", "Create an issue in the configured tracker"),
            tool(
                "mcp__github__list_commits",
                "List commits on a branch of a GitHub repository",
            ),
            tool(
                "mcp__github__search_pull_requests",
                "Search pull requests with filters",
            ),
            tool(
                "mcp__linear__list_issues",
                "List Linear issues in the workspace",
            ),
            tool("generate_svg", "Author and validate an SVG image"),
        ]
    }

    fn names(matches: &[RankedMatch], candidates: &[Candidate]) -> Vec<String> {
        matches
            .iter()
            .map(|m| candidates[m.index].name.clone())
            .collect()
    }

    #[test]
    fn split_terms_handles_namespaces_and_camel_case() {
        assert_eq!(
            split_terms("mcp__github__search_issues"),
            vec!["mcp", "github", "search", "issues"]
        );
        assert_eq!(split_terms("listCommits"), vec!["list", "commits"]);
        // Single-char fragments are dropped, casing normalized.
        assert_eq!(split_terms("a B2b-Test"), vec!["b2b", "test"]);
    }

    #[test]
    fn exact_name_query_ranks_that_tool_first() {
        let cat = catalog();
        let hits = rank("read_file", &cat, 5);
        assert_eq!(names(&hits, &cat)[0], "read_file");
    }

    #[test]
    fn multi_term_query_prefers_the_tool_matching_all_terms() {
        let cat = catalog();
        let hits = rank("search issues", &cat, 5);
        let ranked = names(&hits, &cat);
        assert_eq!(
            ranked[0], "search_issues",
            "both terms in the name must beat single-term hits: {ranked:?}"
        );
        // The Linear issue tool and GitHub search tool are still in the list.
        assert!(ranked.contains(&"mcp__linear__list_issues".to_string()));
    }

    #[test]
    fn required_term_excludes_candidates_missing_it_in_the_name() {
        let cat = catalog();
        let hits = rank("+github search", &cat, 10);
        let ranked = names(&hits, &cat);
        assert!(!ranked.is_empty());
        assert!(
            ranked.iter().all(|n| n.contains("github")),
            "+github must exclude non-github tools: {ranked:?}"
        );
    }

    #[test]
    fn prefix_typing_finds_the_tool() {
        let cat = catalog();
        let hits = rank("screensho", &cat, 3);
        assert_eq!(names(&hits, &cat)[0], "screenshot");
    }

    #[test]
    fn description_only_words_still_find_tools() {
        let cat = catalog();
        let hits = rank("shell command", &cat, 3);
        assert_eq!(names(&hits, &cat)[0], "bash");
    }

    #[test]
    fn keywords_outrank_description_matches() {
        let cat = vec![
            Candidate {
                id: "a".into(),
                name: "list_tables".into(),
                description: "List database tables".into(),
                keywords: vec!["postgres".into()],
            },
            Candidate {
                id: "b".into(),
                name: "run_query".into(),
                description: "Run a query; supports postgres flavors".into(),
                keywords: vec![],
            },
        ];
        let hits = rank("postgres", &cat, 2);
        assert_eq!(cat[hits[0].index].name, "list_tables");
    }

    #[test]
    fn a_single_grazed_common_word_is_noise_not_a_result() {
        let cat = catalog();
        // "the" is dropped by tokenization (short words survive only at 2+
        // chars, but "of"-grade words still hit descriptions); a term hitting
        // exactly one weak description slot must not surface everything.
        let hits = rank("configured", &cat, 10);
        // Only description-exact hits (>= DESC_EXACT) survive the gate.
        for m in &hits {
            assert!(m.score >= MIN_SCORE);
        }
        assert!(hits.len() <= 3, "a common word must not match the catalog");
    }

    #[test]
    fn select_form_returns_exact_ids_in_requested_order() {
        let cat = catalog();
        let hits = rank("select:bash,read_file,unknown_tool", &cat, 10);
        assert_eq!(names(&hits, &cat), vec!["bash", "read_file"]);
        assert!(hits.iter().all(|m| (m.score - 1.0).abs() < f64::EPSILON));
    }

    #[test]
    fn select_form_is_case_insensitive_and_dedupes() {
        let cat = catalog();
        let hits = rank("select:BASH,bash", &cat, 10);
        assert_eq!(names(&hits, &cat), vec!["bash"]);
    }

    #[test]
    fn empty_query_returns_nothing() {
        let cat = catalog();
        assert!(rank("", &cat, 10).is_empty());
        assert!(rank("   ", &cat, 10).is_empty());
        assert!(rank("select:", &cat, 10).is_empty());
    }

    #[test]
    fn ranking_is_deterministic_across_equal_scores() {
        let cat = vec![
            tool("apply_b", "apply a change"),
            tool("apply_a", "apply a change"),
        ];
        let first = rank("apply", &cat, 5);
        let second = rank("apply", &cat, 5);
        assert_eq!(first, second);
        // Equal scores break ties by name.
        assert_eq!(names(&first, &cat), vec!["apply_a", "apply_b"]);
    }

    #[test]
    fn matched_terms_expose_why_a_candidate_ranked() {
        let cat = catalog();
        let hits = rank("github commits", &cat, 5);
        let top = &hits[0];
        assert_eq!(cat[top.index].name, "mcp__github__list_commits");
        assert_eq!(top.matched_terms, vec!["commits", "github"]);
    }

    #[test]
    fn scales_to_a_large_catalog_and_still_finds_the_needle() {
        // Hundreds of near-identical candidates: the one real match must
        // surface first — the "thousands of skills" requirement in miniature.
        let mut cat: Vec<Candidate> = (0..800)
            .map(|i| {
                tool(
                    &format!("tool_{i}"),
                    "A generic workspace helper for common chores",
                )
            })
            .collect();
        cat.push(tool(
            "deploy_preview",
            "Deploy a preview environment for the current branch",
        ));
        let hits = rank("deploy preview environment", &cat, 5);
        assert_eq!(cat[hits[0].index].name, "deploy_preview");
    }

    #[test]
    fn parse_query_marks_required_terms_and_merges_duplicates() {
        match parse_query("+slack send +send") {
            DiscoveryQuery::Keywords(terms) => {
                assert_eq!(terms.len(), 2);
                assert!(terms.iter().any(|t| t.term == "slack" && t.required));
                assert!(
                    terms.iter().any(|t| t.term == "send" && t.required),
                    "bare + required duplicate must merge to required"
                );
            }
            other => panic!("expected keywords, got {other:?}"),
        }
    }
}
