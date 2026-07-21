//! Planning: turn a goal into an ordered step plan (
//! "plan"). The Role::Plan model call itself lives in [`crate::pipeline`];
//! this module owns the two pure pieces — assembling the planner's **split
//! context** (L-E6) and parsing the plan out of the model's response.
//!
//! # Split planner context (L-E6)
//!
//! Feeding the planner the whole running transcript made planning slow and
//! noisy. [`build_planner_prompt`] instead assembles a purpose-built context:
//! the goal, the recalled context frames, and a repo-structure summary —
//! **never** the accumulated message list. That assembly is explicit and
//! testable precisely because it is a pure function over owned data.
//!
//! # Parsing with a fallback (not a fake)
//!
//! Models return plans as either a JSON array or a plain numbered list.
//! [`parse_plan`] tries strict JSON first, then falls back to a numbered/
//! bulleted-list parser. A caller that gets `None` from JSON does one bounded
//! *repair* retry (re-prompt asking for valid JSON) before accepting the
//! list fallback — the repair loop is orchestration and lives in
//! [`crate::pipeline`]; this module just exposes both parsers so the pipeline
//! can compose them.

use serde::Deserialize;

use crate::ports::RecalledFrame;

/// One ordered step in an execution plan. Deliberately minimal — a step is a
/// natural-language instruction the execute stage turns into one engine turn.
/// Richer per-step metadata (target files, tool hints) is a documented
/// follow-up, not faked here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub description: String,
}

impl PlanStep {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
        }
    }
}

/// The JSON shape [`parse_plan`] accepts first: either a bare array of step
/// strings, or an object with a `steps` array (of strings or `{description}`
/// objects). Kept permissive on purpose — a planner that wraps its list in an
/// object shouldn't force a repair round-trip.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PlanJson {
    Bare(Vec<StepJson>),
    Wrapped { steps: Vec<StepJson> },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StepJson {
    Text(String),
    Object { description: String },
}

impl StepJson {
    fn into_description(self) -> String {
        match self {
            StepJson::Text(s) => s,
            StepJson::Object { description } => description,
        }
    }
}

/// Parse a plan from a model response. Tries strict JSON (a bare array or a
/// `{ "steps": [...] }` object) anywhere in the text, then falls back to a
/// numbered/bulleted plain-text list. Returns `None` only when neither yields
/// a non-empty ordered list — the signal the caller uses to decide whether to
/// spend its one bounded JSON-repair retry.
pub fn parse_plan(text: &str) -> Option<Vec<PlanStep>> {
    if let Some(steps) = parse_plan_json(text) {
        return Some(steps);
    }
    parse_plan_list(text)
}

/// Strict-JSON plan parse: scans for the first `[`/`{` … balanced span and
/// tries to deserialize it as a [`PlanJson`]. Whitespace and prose around the
/// JSON are tolerated (models often wrap JSON in ```` ```json ```` fences or a
/// sentence). Returns `None` if no embedded JSON parses into a non-empty
/// step list.
pub fn parse_plan_json(text: &str) -> Option<Vec<PlanStep>> {
    for (open, close) in [('[', ']'), ('{', '}')] {
        if let Some(span) = balanced_span(text, open, close)
            && let Ok(parsed) = serde_json::from_str::<PlanJson>(span)
        {
            let steps = match parsed {
                PlanJson::Bare(v) | PlanJson::Wrapped { steps: v } => v,
            };
            let steps: Vec<PlanStep> = steps
                .into_iter()
                .map(StepJson::into_description)
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty())
                .map(PlanStep::new)
                .collect();
            if !steps.is_empty() {
                return Some(steps);
            }
        }
    }
    None
}

/// Fallback plain-text plan parse: pulls numbered (`1.`, `2)`) or bulleted
/// (`-`, `*`) list items, in order. Returns `None` if fewer than one item is
/// found so the caller can distinguish "the model gave prose, not a plan".
pub fn parse_plan_list(text: &str) -> Option<Vec<PlanStep>> {
    let mut steps = Vec::new();
    for line in text.lines() {
        if let Some(desc) = strip_list_marker(line) {
            let desc = desc.trim();
            if !desc.is_empty() {
                steps.push(PlanStep::new(desc));
            }
        }
    }
    (!steps.is_empty()).then_some(steps)
}

/// Strip a leading list marker (`1.`, `2)`, `-`, `*`, `•`) from a line,
/// returning the remaining text, or `None` if the line isn't a list item.
fn strip_list_marker(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
        return Some(rest);
    }
    if let Some(rest) = t.strip_prefix("• ") {
        return Some(rest);
    }
    // Numbered: one-or-more digits then '.' or ')' then a space.
    let mut chars = t.char_indices();
    let mut saw_digit = false;
    for (i, c) in chars.by_ref() {
        if c.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if saw_digit && (c == '.' || c == ')') {
            // consume the delimiter, then require a following space/tab
            let rest = &t[i + c.len_utf8()..];
            return rest
                .strip_prefix(' ')
                .or_else(|| rest.strip_prefix('\t'))
                .map(str::trim_start);
        }
        break;
    }
    None
}

/// Find the first balanced `open`…`close` span in `text` (depth-tracked, so
/// nested brackets don't truncate early). Returns the inclusive span as a
/// slice, or `None` if no balanced pair exists. Ignores brackets inside JSON
/// string literals so a `]` inside a step description can't close the array
/// early.
fn balanced_span(text: &str, open: char, close: char) -> Option<&str> {
    let start = text.find(open)?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            _ if c == open => depth += 1,
            _ if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + c.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Assemble the planner's split context (L-E6): goal + recalled frames +
/// repo-structure summary, and an instruction to emit a JSON array of steps.
/// **Never** includes the running transcript — the whole point of the split.
/// The recall frames are cited by label (L-C4), and their content is included
/// as grounding.
pub fn build_planner_prompt(goal: &str, recall: &[RecalledFrame], repo_structure: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are the planner for a coding agent. Produce a short ordered plan of \
         concrete steps to accomplish the goal. Respond with a JSON array of \
         step strings, e.g. [\"step one\", \"step two\"]. Keep it minimal — the \
         fewest steps that fully accomplish the goal.\n\n",
    );

    prompt.push_str("## Goal\n");
    prompt.push_str(goal.trim());
    prompt.push_str("\n\n");

    if !recall.is_empty() {
        prompt.push_str("## Recalled context\n");
        for frame in recall {
            // Cite by human label (L-C4); include the content as grounding.
            prompt.push_str("- [");
            prompt.push_str(&frame.citation_label);
            prompt.push_str("] (");
            prompt.push_str(&frame.source);
            prompt.push_str(")\n");
            if !frame.content.trim().is_empty() {
                prompt.push_str("  ");
                prompt.push_str(frame.content.trim());
                prompt.push('\n');
            }
        }
        prompt.push('\n');
    }

    let repo_structure = repo_structure.trim();
    if !repo_structure.is_empty() {
        prompt.push_str("## Repository structure\n");
        prompt.push_str(repo_structure);
        prompt.push_str("\n\n");
    }

    prompt.push_str("## Plan (JSON array of step strings)\n");
    prompt
}

/// A short re-prompt asking a model that returned unparseable plan output to
/// re-emit it as a strict JSON array — the pipeline's one bounded repair
/// retry (L-V2 "bounded repair loops"). Kept here beside the parsers it feeds.
pub fn plan_repair_prompt(previous_response: &str) -> String {
    format!(
        "Your previous response could not be parsed as a plan. Re-emit the plan as \
         a strict JSON array of step strings and NOTHING else — no prose, no code \
         fences. Previous response:\n{previous_response}\n\nJSON array:"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(label: &str, content: &str) -> RecalledFrame {
        RecalledFrame {
            citation_label: label.into(),
            provider: "code-graph".into(),
            source: "code-graph".into(),
            kind: "symbol".into(),
            uri: None,
            method: None,
            content: content.into(),
            token_cost: 10,
            id: None,
        }
    }

    #[test]
    fn parses_a_bare_json_array() {
        let steps = parse_plan(r#"["add field", "update migration"]"#).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], PlanStep::new("add field"));
        assert_eq!(steps[1], PlanStep::new("update migration"));
    }

    #[test]
    fn parses_json_wrapped_in_prose_and_fences() {
        let text = "Here is the plan:\n```json\n[\"one\", \"two\", \"three\"]\n```\nDone.";
        let steps = parse_plan(text).unwrap();
        assert_eq!(steps.len(), 3);
    }

    #[test]
    fn parses_a_steps_object_with_object_items() {
        let text = r#"{"steps": [{"description": "a"}, {"description": "b"}]}"#;
        let steps = parse_plan(text).unwrap();
        assert_eq!(steps, vec![PlanStep::new("a"), PlanStep::new("b")]);
    }

    #[test]
    fn bracket_inside_a_string_does_not_close_the_array_early() {
        let steps = parse_plan(r#"["fix arr[0] indexing", "add test"]"#).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], PlanStep::new("fix arr[0] indexing"));
    }

    #[test]
    fn falls_back_to_a_numbered_list_when_json_absent() {
        let text = "1. add the field\n2. update the migration\n3. add a test";
        let steps = parse_plan(text).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0], PlanStep::new("add the field"));
    }

    #[test]
    fn falls_back_to_a_bulleted_list() {
        let text = "- add the field\n- update the migration";
        let steps = parse_plan(text).unwrap();
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn numbered_list_with_paren_delimiter_and_indentation() {
        let text = "  1) first\n  2) second";
        let steps = parse_plan_list(text).unwrap();
        assert_eq!(steps, vec![PlanStep::new("first"), PlanStep::new("second")]);
    }

    #[test]
    fn prose_with_no_list_or_json_is_none() {
        assert_eq!(
            parse_plan("I will add a field and update the migration."),
            None
        );
        assert_eq!(parse_plan(""), None);
        assert_eq!(parse_plan("[]"), None, "empty array yields no steps");
    }

    #[test]
    fn a_line_that_is_only_a_marker_is_skipped() {
        // "1. " with no text must not produce an empty step.
        let text = "1. \n2. real step";
        let steps = parse_plan_list(text).unwrap();
        assert_eq!(steps, vec![PlanStep::new("real step")]);
    }

    #[test]
    fn planner_prompt_includes_goal_recall_and_structure_but_not_transcript() {
        let recall = vec![frame("engine driver (driver.rs)", "run_turn drives steps")];
        let prompt = build_planner_prompt(
            "Fix the failing budget test",
            &recall,
            "crates/\n  stella-core/src/budget.rs",
        );
        assert!(prompt.contains("Fix the failing budget test"));
        assert!(prompt.contains("engine driver (driver.rs)"));
        assert!(prompt.contains("run_turn drives steps"));
        assert!(prompt.contains("stella-core/src/budget.rs"));
        assert!(prompt.contains("JSON array"));
    }

    #[test]
    fn planner_prompt_omits_empty_recall_and_structure_sections() {
        let prompt = build_planner_prompt("goal", &[], "");
        assert!(!prompt.contains("Recalled context"));
        assert!(!prompt.contains("Repository structure"));
    }

    #[test]
    fn repair_prompt_asks_for_strict_json_and_echoes_the_bad_response() {
        let p = plan_repair_prompt("here's a plan in prose");
        assert!(p.contains("JSON array"));
        assert!(p.contains("here's a plan in prose"));
    }
}
