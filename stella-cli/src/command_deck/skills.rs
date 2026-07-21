//! SKILLS-tab helper cluster split out of `command_deck.rs` to keep that
//! file under the file-size ratchet (see `scripts/file-size-ratchet.txt`).
//! Pure relocation — no behavior change.
use super::*;

/// The deck's slash vocabulary: the productized commands (🔒) followed by
/// every custom command/skill (⚡) currently on disk. Rebuilt after `/init`
/// so just-adopted definitions appear without a restart.
pub(super) fn deck_slash_commands(
    custom: &crate::extensions::CustomExtensions,
) -> Vec<SlashCommand> {
    let mut commands: Vec<SlashCommand> = DECK_BUILTINS
        .iter()
        .map(|(name, description)| SlashCommand::new(*name, *description))
        .collect();
    let customs = custom.slash_entries(&commands);
    commands.extend(customs);
    commands
}

// ── SKILLS tab: driver-side ops (the deck routes `WorkspaceInput::Skill`) ────

/// Snapshot the installed skills across BOTH scopes into an [`Inbound::Skills`].
pub(super) fn skills_snapshot(workspace_root: &std::path::Path, status: Option<String>) -> Inbound {
    Inbound::Skills(SkillsView {
        rows: crate::skill_manager::enumerate(workspace_root),
        status,
        busy: false,
    })
}

/// Parse `npx skills find` output into structured hits (cap 50). The output is
/// ANSI-colored and — under a TTY — carries a banner + an "Install with" line +
/// per-hit `└ url` continuation lines. We strip the escapes and **allowlist**
/// only the result rows: a leading `owner/repo@skill` token, optionally
/// followed by an `<N> installs` popularity string, with the following URL line
/// attached. Everything else (banner, instructions, blanks) is ignored, so no
/// raw escape codes or ASCII-art ever reach the UI.
pub(super) fn parse_skill_hits(out: &str) -> Vec<SkillSearchHit> {
    let mut hits: Vec<SkillSearchHit> = Vec::new();
    for raw in out.lines() {
        if hits.len() >= 50 {
            break;
        }
        let line = strip_ansi(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A URL continuation line belongs to the hit just above it.
        if let Some(url) = skill_url_in(line) {
            if let Some(last) = hits.last_mut()
                && last.url.is_empty()
            {
                last.url = url;
            }
            continue;
        }
        // Otherwise, only a genuine `owner/repo@skill …` result row is kept.
        if let Some((id, installs, rank)) = parse_result_line(line) {
            hits.push(SkillSearchHit {
                id,
                installs,
                installs_rank: rank,
                url: String::new(),
            });
        }
    }
    hits
}

/// Strip ANSI/CSI escape sequences (`ESC [ … final`) from a line, leaving the
/// visible text. Robust to the SGR color codes `npx skills` emits.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // A CSI sequence: '[' then params/intermediates, then a final byte
            // in 0x40..=0x7e. Consume through the final byte.
            if chars.next() == Some('[') {
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// If a (de-ANSI'd) line is a URL continuation (`└ https://skills.sh/…`),
/// return the URL. Matching on `http` is robust to the leading box-drawing
/// glyph, which the registry sometimes emits mojibake'd.
fn skill_url_in(line: &str) -> Option<String> {
    let pos = line.find("https://").or_else(|| line.find("http://"))?;
    Some(line[pos..].split_whitespace().next()?.to_string())
}

/// Parse a result row into `(id, installs_display, installs_rank)`. The row's
/// first whitespace token must be an `owner/repo@skill` id (has both `/` and
/// `@`, no angle-bracket placeholder); the rest, if any, is the installs
/// string (`"15.8K installs"`). Non-result lines (banner, "Install with …")
/// return `None`.
fn parse_result_line(line: &str) -> Option<(String, String, u64)> {
    let mut toks = line.split_whitespace();
    let id = toks.next()?;
    if !id.contains('/') || !id.contains('@') || id.contains('<') || id.contains('>') {
        return None;
    }
    let rest = toks.collect::<Vec<_>>().join(" ");
    let rest = rest.trim();
    if rest.is_empty() {
        Some((id.to_string(), String::new(), 0))
    } else {
        Some((id.to_string(), rest.to_string(), parse_installs_count(rest)))
    }
}

/// The numeric install count from a string like `"15.8K installs"` — the first
/// token parseable as a number with an optional K/M/B suffix. `0` if none.
pub(super) fn parse_installs_count(s: &str) -> u64 {
    s.split_whitespace()
        .find_map(parse_count_token)
        .unwrap_or(0)
}

/// Parse one token like `15.8K` / `9K` / `342` into an absolute count.
fn parse_count_token(tok: &str) -> Option<u64> {
    let t = tok.trim();
    let (num, mult) = match t.chars().last() {
        Some('K') | Some('k') => (&t[..t.len() - 1], 1_000.0),
        Some('M') | Some('m') => (&t[..t.len() - 1], 1_000_000.0),
        Some('B') | Some('b') => (&t[..t.len() - 1], 1_000_000_000.0),
        _ => (t, 1.0),
    };
    let v: f64 = num.parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * mult) as u64)
}

/// Run `npx skills add <id>` in an isolated temp dir, then adopt the produced
/// skill into `scope`. Running in a temp cwd (not the workspace) makes the
/// destination ours to control — that is how "install for me →
/// ~/.config/stella/skills" lands there despite the registry CLI's fixed cwd.
async fn install_skill(
    registry: &SkillRegistry,
    scope: SkillScope,
    id: &str,
    workspace_root: &std::path::Path,
) -> String {
    let tmp = match tempfile::Builder::new().prefix("stella-skill-").tempdir() {
        Ok(t) => t,
        Err(e) => return format!("install failed: {e}"),
    };
    let mut reg = registry.clone();
    reg.workspace_root = tmp.path().to_path_buf();
    let argv = SkillRegistry::render(&reg.install_cmd, "{id}", id);
    if let Err(e) = reg.run(argv, 300).await {
        return format!("install failed: {e}");
    }
    match crate::skill_manager::adopt_tree(scope, workspace_root, tmp.path(), id) {
        Ok(name) => format!("installed {name} ({})", scope.label()),
        Err(e) => format!("install produced nothing usable: {e}"),
    }
}

/// Fetch a not-yet-installed skill's `SKILL.md` for the ctrl+o preview via
/// `npx skills use <id>`, which prints the body wrapped in `<SKILL.md>…`. A
/// larger output cap than search keeps the full body. Returns `(body, status)`
/// — on failure `body` is empty and `status` carries the reason.
async fn fetch_skill_markdown(registry: &SkillRegistry, id: &str) -> (String, Option<String>) {
    let argv = SkillRegistry::render(&registry.use_cmd, "{id}", id);
    match registry.run_capped(argv, 120, 200_000).await {
        Ok(out) => (extract_skill_md_from_use(&out), None),
        Err(e) => (String::new(), Some(format!("preview failed: {e}"))),
    }
}

/// Pull the `SKILL.md` body out of `npx skills use` output. Prefer the content
/// between the `<SKILL.md>` / `</SKILL.md>` markers; if the format drifts, fall
/// back to the text after a leading preamble (from the first `---` frontmatter
/// or `#` heading), never a blank preview.
pub(super) fn extract_skill_md_from_use(out: &str) -> String {
    let out = strip_ansi(out);
    if let Some(start) = out.find("<SKILL.md>") {
        let after = &out[start + "<SKILL.md>".len()..];
        let body = match after.find("</SKILL.md>") {
            Some(end) => &after[..end],
            None => after,
        };
        return body.trim().to_string();
    }
    // Fallback: drop the preamble by starting at the frontmatter or first heading.
    if let Some(fm) = out.find("\n---").or_else(|| out.find("---")) {
        return out[fm..].trim().to_string();
    }
    if let Some(h) = out.find("\n#").or_else(|| out.find('#')) {
        return out[h..].trim().to_string();
    }
    out.trim().to_string()
}

/// Rank registry hits for LLM-assisted creation by (a) relevance — how many of
/// the request's words appear in the hit's id — then (b) popularity
/// (`installs_rank`) as a usefulness signal. Returns the top few as
/// `"id (installs)"` labels, most useful first.
pub(super) fn rank_hits(hits: &[SkillSearchHit], request: &str) -> Vec<String> {
    let want: Vec<String> = request
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let mut scored: Vec<(usize, u64, &SkillSearchHit)> = hits
        .iter()
        .map(|h| {
            let lower = h.id.to_ascii_lowercase();
            let relevance = want.iter().filter(|w| lower.contains(w.as_str())).count();
            (relevance, h.installs_rank, h)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    scored
        .into_iter()
        .take(6)
        .map(|(_, _, h)| {
            if h.installs.is_empty() {
                h.id.clone()
            } else {
                format!("{} ({})", h.id, h.installs)
            }
        })
        .collect()
}

/// The system prompt for one-shot skill authoring.
const SKILL_AUTHOR_SYSTEM: &str = "You author `SKILL.md` files for a coding agent. A skill is reusable \
know-how (a convention, procedure, or preference) the agent applies when relevant. Output ONLY the \
file content: YAML frontmatter delimited by `---` with `name:` (a short kebab-case slug), \
`description:` (one line — the primary selection signal), and optional `domains:` (comma-separated \
tags), followed by a concise markdown body. No commentary before or after.";

/// Assemble the user prompt for LLM-assisted creation: the request plus the
/// ranked registry candidates the model may borrow from (whole or in part,
/// across several) to deliver ONE coherent skill. Pure — unit-tested.
pub(super) fn build_skill_creation_prompt(request: &str, ranked_candidates: &[String]) -> String {
    let mut p = String::new();
    p.push_str("Create ONE new skill for this request:\n\n");
    p.push_str(request.trim());
    p.push_str("\n\n");
    if ranked_candidates.is_empty() {
        p.push_str(
            "No existing skills were found in the registry. Author the skill from scratch.\n",
        );
    } else {
        p.push_str(
            "Existing registry skills, ranked by usefulness (relevance, then popularity). You may \
             borrow whole or in part from any of them, and assemble bits from several into one \
             coherent skill — but deliver a SINGLE skill:\n",
        );
        for (i, c) in ranked_candidates.iter().enumerate() {
            p.push_str(&format!("{}. {}\n", i + 1, c));
        }
    }
    p.push_str(
        "\nWrite the SKILL.md now. Keep the body focused and actionable; the description must make \
         it easy to select for the right task.",
    );
    p
}

/// Extract the `SKILL.md` content from a model reply: prefer the first fenced
/// code block; otherwise, from the first `---` (frontmatter) onward; otherwise
/// the trimmed whole reply.
pub(super) fn extract_skill_md(text: &str) -> String {
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        let after = after
            .split_once('\n')
            .map(|(_, rest)| rest)
            .unwrap_or(after);
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    if let Some(fm) = text.find("---") {
        return text[fm..].trim().to_string();
    }
    text.trim().to_string()
}

/// LLM-assisted creation: search the registry for the request, rank the hits,
/// have the model assemble ONE `SKILL.md` (reusing the existing provider path),
/// and write it into `scope` as version 1. Returns a status string.
async fn create_skill_llm(
    cfg: &Config,
    registry: &SkillRegistry,
    scope: SkillScope,
    description: &str,
    workspace_root: &std::path::Path,
) -> String {
    // 1. Search existing skills for inspiration (best-effort — a registry
    //    failure just means authoring from scratch).
    let argv = SkillRegistry::render(&registry.search_cmd, "{query}", description);
    let search_out = registry.run(argv, 90).await.unwrap_or_default();
    let ranked = rank_hits(&parse_skill_hits(&search_out), description);
    // 2. Assemble the prompt and run a one-shot model call (the same provider
    //    path the rest of the session uses — never hand-rolled HTTP).
    let provider = match agent::build_provider(cfg) {
        Ok(p) => p,
        Err(e) => return format!("create failed: {e}"),
    };
    let req = CompletionRequest {
        messages: vec![
            CompletionMessage::system(SKILL_AUTHOR_SYSTEM),
            CompletionMessage::user(build_skill_creation_prompt(description, &ranked)),
        ],
        max_output_tokens: Some(1200),
        temperature: Some(0.2),
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    };
    let content = match provider.complete(req).await {
        Ok(r) => extract_skill_md(&r.text),
        Err(e) => return format!("model call failed: {e}"),
    };
    // 3. Validate it parses as a real skill, then write it as v1.
    let name = match stella_core::skills::skill_from_file("SKILL.md", &content) {
        Ok(s) => s.name,
        Err(_) => return "the model did not return a valid SKILL.md — try again".to_string(),
    };
    match crate::skill_manager::create(scope, &name, &content, workspace_root) {
        Ok(n) => format!("created {n} ({}) — v1", scope.label()),
        Err(e) => format!("create failed: {e}"),
    }
}

/// Route one SKILLS-tab op. Disk ops run inline and answer immediately with a
/// refreshed [`Inbound::Skills`]; npx/model ops spawn a task (like `!` shell
/// commands) so a slow child never stalls the driver, then answer on
/// completion. Called at both driver recv sites so the tab works mid-turn.
pub(super) fn handle_skills_input(
    op: &SkillOp,
    cfg: &Config,
    in_tx: &UnboundedSender<Inbound>,
    registry: &SkillRegistry,
) {
    let root = cfg.workspace_root.clone();
    match op {
        SkillOp::List => {
            let _ = in_tx.send(skills_snapshot(&root, None));
        }
        SkillOp::SetEnabled {
            scope,
            name,
            enabled,
        } => {
            let status = crate::skill_manager::set_enabled(*scope, name, *enabled, &root)
                .unwrap_or_else(|e| e);
            let _ = in_tx.send(skills_snapshot(&root, Some(status)));
        }
        SkillOp::Uninstall { scope, name } => {
            let status = crate::skill_manager::uninstall(*scope, name, &root).unwrap_or_else(|e| e);
            let _ = in_tx.send(skills_snapshot(&root, Some(status)));
        }
        SkillOp::Edit { scope, name, body } => {
            let status =
                crate::skill_manager::save_edit(*scope, name, body, &root).unwrap_or_else(|e| e);
            let _ = in_tx.send(skills_snapshot(&root, Some(status)));
        }
        SkillOp::Pin {
            scope,
            name,
            version,
        } => {
            let status =
                crate::skill_manager::set_pin(*scope, name, *version, &root).unwrap_or_else(|e| e);
            let _ = in_tx.send(skills_snapshot(&root, Some(status)));
        }
        SkillOp::Search { query } => {
            let registry = registry.clone();
            let in_tx = in_tx.clone();
            let query = query.clone();
            tokio::spawn(async move {
                let argv = SkillRegistry::render(&registry.search_cmd, "{query}", &query);
                let (hits, status) = match registry.run(argv, 90).await {
                    Ok(out) => (parse_skill_hits(&out), None),
                    Err(e) => (Vec::new(), Some(format!("search failed: {e}"))),
                };
                let _ = in_tx.send(Inbound::SkillSearch {
                    query,
                    hits,
                    status,
                });
            });
        }
        SkillOp::Preview { id } => {
            let registry = registry.clone();
            let in_tx = in_tx.clone();
            let id = id.clone();
            tokio::spawn(async move {
                let (body, status) = fetch_skill_markdown(&registry, &id).await;
                let _ = in_tx.send(Inbound::SkillPreview { id, body, status });
            });
        }
        SkillOp::Install { scope, id } => {
            let registry = registry.clone();
            let in_tx = in_tx.clone();
            let id = id.clone();
            let scope = *scope;
            let root = root.clone();
            tokio::spawn(async move {
                let status = install_skill(&registry, scope, &id, &root).await;
                let _ = in_tx.send(skills_snapshot(&root, Some(status)));
            });
        }
        SkillOp::Create { scope, description } => {
            let registry = registry.clone();
            let in_tx = in_tx.clone();
            let cfg = cfg.clone();
            let scope = *scope;
            let description = description.clone();
            let root = root.clone();
            tokio::spawn(async move {
                let status = create_skill_llm(&cfg, &registry, scope, &description, &root).await;
                let _ = in_tx.send(skills_snapshot(&root, Some(status)));
            });
        }
    }
}
