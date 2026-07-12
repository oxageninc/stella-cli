//! Triage: classify the prompt so the pipeline can skip work it doesn't need
//! (L-E2). The *decision* logic here is pure and synchronous — the actual
//! Role::Triage model call (under `RetryPolicy::deterministic()` and a
//! latency ceiling, L-M4) lives in [`crate::pipeline`]; this module owns only
//! the classification vocabulary, the response parser, and the deterministic
//! single-task/multi-step pattern floor.
//!
//! # Fast paths and correct downgrade (L-E2)
//!
//! - [`TaskClass::SimpleLookup`] skips both the planner and the judge — but
//!   that judge-skip *self-revokes* if execution unexpectedly touches files
//!   (the zero-diff guard, enforced in [`crate::pipeline`]).
//! - [`TaskClass::SingleTask`] skips DAG planning (one execute turn) but is
//!   still verified.
//! - [`TaskClass::MultiStep`] takes the full plan → scope-review → execute →
//!   verify → judge path.
//!
//! Every fast path must **downgrade correctly**: a complex task *misclassified*
//! as simple must still complete (just without the planning/verification
//! scaffolding, more slowly) — never fail outright. Two mechanisms guarantee
//! that: the deterministic floor ([`deterministic_floor`]) can only *raise*
//! the class toward more planning (it errs toward planning, never away), and
//! the execute engine's own tool loop can still drive a complex task to
//! completion inside a single turn.

/// How much orchestration a prompt needs. Ordered least → most work; the
/// derived `Ord` is load-bearing (`deterministic_floor` takes the `max` of
/// the model's class and the pattern floor, so the floor can only ever add
/// planning, never remove it — L-E2 "errs toward planning").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskClass {
    /// A lookup/read/explain prompt: no plan, no judge. Self-revoking on
    /// unexpected file mutations (zero-diff guard).
    SimpleLookup,
    /// One concrete change: skip DAG planning, one execute turn, still
    /// verified.
    SingleTask,
    /// Genuinely multi-step: full plan → scope review → execute → verify →
    /// judge.
    MultiStep,
}

impl TaskClass {
    /// Whether DAG planning runs for this class. Only [`MultiStep`] plans;
    /// simple and single-task goals skip it (L-E2).
    ///
    /// [`MultiStep`]: TaskClass::MultiStep
    pub fn plans(self) -> bool {
        matches!(self, TaskClass::MultiStep)
    }

    /// Whether the verification ladder runs *unconditionally* for this class.
    /// [`SimpleLookup`] skips it (subject to the zero-diff guard, applied
    /// separately in the pipeline); the other two always verify.
    ///
    /// [`SimpleLookup`]: TaskClass::SimpleLookup
    pub fn verifies_unconditionally(self) -> bool {
        !matches!(self, TaskClass::SimpleLookup)
    }
}

/// Parse a triage model's classification response into a [`TaskClass`].
///
/// The triage prompt (see [`triage_prompt`]) asks the model to answer with a
/// single bare token; real models add whitespace, punctuation, or a short
/// justification, so this scans case-insensitively for the first recognized
/// keyword. Returns `None` when nothing matches — the caller treats an
/// unparseable triage response the same as a failed call: fall through to the
/// full path (never guess a fast path off ambiguous output).
pub fn classify_triage_response(text: &str) -> Option<TaskClass> {
    let lower = text.to_ascii_lowercase();
    // Scan token by token for the first recognized class keyword. Checking
    // whole tokens (not substrings) avoids matching e.g. "multi_step" inside
    // a longer word, and lets the model wrap the token in punctuation.
    for raw in lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        match raw {
            "lookup" | "simple" | "simple_lookup" | "read" | "explain" => {
                return Some(TaskClass::SimpleLookup);
            }
            "single" | "single_task" | "task" | "fix" => {
                return Some(TaskClass::SingleTask);
            }
            "multi" | "multi_step" | "multistep" | "complex" | "plan" => {
                return Some(TaskClass::MultiStep);
            }
            _ => {}
        }
    }
    None
}

/// The deterministic pattern floor on task class (L-E2: "single-task goals
/// (deterministic pattern check that errs toward planning)"). Independent of
/// any model call, this inspects the goal text for signals that the work is
/// *at least* single-task or multi-step, and returns the minimum class those
/// signals justify. It can only ever *raise* the class (the caller takes the
/// `max` with the model's classification), so a flaky or over-eager triage
/// model can't talk the pipeline out of planning genuinely multi-step work.
///
/// Deliberately conservative: only strong, unambiguous multi-step signals
/// (explicit enumeration, conjoined imperatives, cross-cutting scope words)
/// raise to [`TaskClass::MultiStep`]. Everything else floors at
/// [`TaskClass::SimpleLookup`] and lets the model's own classification stand.
pub fn deterministic_floor(goal: &str) -> TaskClass {
    let lower = goal.to_ascii_lowercase();

    // Strong multi-step signals: cross-cutting scope, explicit sequencing, or
    // an enumerated list of asks.
    const MULTI_STEP_MARKERS: &[&str] = &[
        " and then ",
        " after that ",
        "step 1",
        "step 2",
        "first,",
        "firstly",
        "refactor across",
        "across the codebase",
        "every file",
        "all files",
        "all the files",
        "each of the",
        "migrate all",
        "rename all",
    ];
    if MULTI_STEP_MARKERS.iter().any(|m| lower.contains(m)) {
        return TaskClass::MultiStep;
    }

    // A numbered/bulleted list of three or more items reads as multi-step.
    if enumerated_item_count(&lower) >= 3 {
        return TaskClass::MultiStep;
    }

    // Two or more conjoined imperative clauses ("do X and do Y", "add … and
    // update …") read as at least single-task work with a real change, and
    // often multi-step — floor conservatively at single-task and let the
    // model raise it further if it saw more.
    if conjoined_imperative(&lower) {
        return TaskClass::SingleTask;
    }

    TaskClass::SimpleLookup
}

/// Combine the model's triage classification with the deterministic floor,
/// taking the more-planning-heavy of the two (L-E2 "errs toward planning").
/// When the model call failed or its response didn't parse (`model_class`
/// is `None`), the floor stands alone.
pub fn resolve_task_class(model_class: Option<TaskClass>, goal: &str) -> TaskClass {
    let floor = deterministic_floor(goal);
    match model_class {
        Some(model) => model.max(floor),
        None => floor,
    }
}

/// Count enumerated list items ("1.", "2)", "- ", "* ") in the goal — a
/// crude but deterministic proxy for "the user listed several asks".
fn enumerated_item_count(lower: &str) -> usize {
    let mut count = 0usize;
    for line in lower.lines() {
        let t = line.trim_start();
        let is_bullet = t.starts_with("- ") || t.starts_with("* ");
        let is_numbered = {
            let mut chars = t.chars();
            let first = chars.next();
            match first {
                Some(d) if d.is_ascii_digit() => {
                    matches!(chars.next(), Some('.') | Some(')'))
                }
                _ => false,
            }
        };
        if is_bullet || is_numbered {
            count += 1;
        }
    }
    count
}

/// Detect two or more imperative clauses joined by "and"/"then" — "add a
/// field and update the migration". Heuristic and intentionally narrow: it
/// only fires when an "and"/"then" is followed (soon) by a known change verb,
/// so a descriptive "the parser and the lexer" doesn't trip it.
fn conjoined_imperative(lower: &str) -> bool {
    const CHANGE_VERBS: &[&str] = &[
        "add",
        "update",
        "remove",
        "delete",
        "rename",
        "create",
        "write",
        "fix",
        "refactor",
        "implement",
        "wire",
        "move",
        "replace",
        "migrate",
        "extract",
    ];
    // A leading change verb establishes the first clause. `split_whitespace`
    // already skips leading whitespace, so no separate trim is needed.
    let has_leading_verb = lower
        .split_whitespace()
        .next()
        .map(|w| CHANGE_VERBS.contains(&w))
        .unwrap_or(false);
    if !has_leading_verb {
        return false;
    }
    // A second change verb after an " and "/" then " establishes the second.
    for sep in [" and ", " then "] {
        if let Some(idx) = lower.find(sep) {
            let rest = &lower[idx + sep.len()..];
            if let Some(word) = rest.split_whitespace().next()
                && CHANGE_VERBS.contains(&word)
            {
                return true;
            }
        }
    }
    false
}

/// The system+user prompt handed to the Role::Triage model to classify a
/// goal. A tiny, fixed instruction (the triage model is a cheap/fast tier,
/// `07-model-matrix.md`): it must answer with exactly one bare token.
pub fn triage_prompt(goal: &str) -> String {
    format!(
        "Classify the following software task by how much orchestration it needs. \
         Answer with EXACTLY ONE of these bare tokens and nothing else:\n\
         - `lookup`  — a read/explain/search question that changes no files\n\
         - `single`  — one concrete code change\n\
         - `multi`   — genuinely multi-step work spanning several changes\n\n\
         Task:\n{goal}\n\n\
         Classification:"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_class_ordering_is_least_to_most_work() {
        assert!(TaskClass::SimpleLookup < TaskClass::SingleTask);
        assert!(TaskClass::SingleTask < TaskClass::MultiStep);
    }

    #[test]
    fn plans_and_verifies_flags_match_the_fast_path_design() {
        assert!(!TaskClass::SimpleLookup.plans());
        assert!(!TaskClass::SingleTask.plans());
        assert!(TaskClass::MultiStep.plans());

        assert!(!TaskClass::SimpleLookup.verifies_unconditionally());
        assert!(TaskClass::SingleTask.verifies_unconditionally());
        assert!(TaskClass::MultiStep.verifies_unconditionally());
    }

    #[test]
    fn parses_each_class_from_bare_and_decorated_responses() {
        assert_eq!(
            classify_triage_response("lookup"),
            Some(TaskClass::SimpleLookup)
        );
        assert_eq!(
            classify_triage_response("Classification: single."),
            Some(TaskClass::SingleTask)
        );
        assert_eq!(
            classify_triage_response("`multi` — this spans files"),
            Some(TaskClass::MultiStep)
        );
        assert_eq!(
            classify_triage_response("I think this is a SIMPLE read"),
            Some(TaskClass::SimpleLookup)
        );
    }

    #[test]
    fn unparseable_triage_response_is_none_not_a_guess() {
        assert_eq!(classify_triage_response(""), None);
        assert_eq!(classify_triage_response("hmm, not sure"), None);
        assert_eq!(classify_triage_response("42"), None);
    }

    #[test]
    fn deterministic_floor_raises_on_strong_multi_step_markers() {
        assert_eq!(
            deterministic_floor("Add the field and then update the migration"),
            TaskClass::MultiStep
        );
        assert_eq!(
            deterministic_floor("Rename all callers of foo across the codebase"),
            TaskClass::MultiStep
        );
        assert_eq!(
            deterministic_floor("First, add a route. Secondly wire the handler."),
            TaskClass::MultiStep
        );
    }

    #[test]
    fn deterministic_floor_detects_enumerated_lists() {
        let goal = "Please:\n1. add a field\n2. update the schema\n3. add a test";
        assert_eq!(deterministic_floor(goal), TaskClass::MultiStep);

        let bullets = "Do these:\n- add a field\n- update the schema\n- add a test";
        assert_eq!(deterministic_floor(bullets), TaskClass::MultiStep);
    }

    #[test]
    fn deterministic_floor_conjoined_imperatives_reach_single_task() {
        assert_eq!(
            deterministic_floor("Add a field and update the migration"),
            // "and then" isn't present, but two change verbs joined by "and"
            // floor at single-task (the model may raise it to multi).
            TaskClass::SingleTask
        );
    }

    #[test]
    fn deterministic_floor_leaves_plain_prompts_at_lookup() {
        assert_eq!(
            deterministic_floor("What does the retry policy do?"),
            TaskClass::SimpleLookup
        );
        assert_eq!(
            deterministic_floor("Explain the flip oracle"),
            TaskClass::SimpleLookup
        );
        // A descriptive "and" between nouns must NOT trip the imperative
        // detector (no leading change verb).
        assert_eq!(
            deterministic_floor("Where are the parser and the lexer defined?"),
            TaskClass::SimpleLookup
        );
    }

    #[test]
    fn resolve_takes_the_more_planning_heavy_class() {
        // Model says simple, floor says multi (markers present) → multi wins.
        assert_eq!(
            resolve_task_class(
                Some(TaskClass::SimpleLookup),
                "refactor across the codebase"
            ),
            TaskClass::MultiStep
        );
        // Model says multi, floor says simple → model wins (floor never
        // lowers).
        assert_eq!(
            resolve_task_class(Some(TaskClass::MultiStep), "explain X"),
            TaskClass::MultiStep
        );
        // Failed/unparseable triage → floor stands alone.
        assert_eq!(
            resolve_task_class(None, "explain X"),
            TaskClass::SimpleLookup
        );
        assert_eq!(
            resolve_task_class(None, "add a field and then update the migration"),
            TaskClass::MultiStep
        );
    }

    #[test]
    fn triage_prompt_names_all_three_tokens_and_the_goal() {
        let p = triage_prompt("Fix the failing test");
        assert!(p.contains("lookup"));
        assert!(p.contains("single"));
        assert!(p.contains("multi"));
        assert!(p.contains("Fix the failing test"));
    }
}
