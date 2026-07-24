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

    /// One step down the ceremony ladder, saturating at the cheapest class.
    /// The bound on how far a triage classification may lower the
    /// deterministic floor — see [`resolve_task_class`].
    pub fn one_level_cheaper(self) -> Self {
        match self {
            TaskClass::MultiStep => TaskClass::SingleTask,
            TaskClass::SingleTask | TaskClass::SimpleLookup => TaskClass::SimpleLookup,
        }
    }
}

/// What triage concluded about a goal: how much orchestration it needs, and
/// what assurance the result actually warrants.
///
/// Splitting the two is the point. "How big is this" and "what proof does it
/// need" are different axes that [`TaskClass`] alone conflates: a sweeping
/// mechanical rename needs no independent test, while a one-line change to an
/// auth check might. Triage read the goal, so triage decides — a `None` flag
/// simply means it expressed no opinion and the class-derived default stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskAssessment {
    pub class: TaskClass,
    /// Whether the input is conversational — a greeting, small talk, or a
    /// question about the agent rather than a software task at all. When set,
    /// the pipeline answers in one plain completion and skips plan → witness →
    /// execute → verify entirely (the escape hatch a bare `hi` needs so it
    /// never gets forced into authoring a witness test). Orthogonal to
    /// [`TaskClass`]: "is this even a task" is a different axis from "how big
    /// is the task", so it rides its own field instead of a bottom variant of
    /// the ordered ceremony ladder. See [`resolve_conversational`].
    pub conversational: bool,
    /// Whether an independently authored witness test is warranted.
    pub require_witness: Option<bool>,
    /// Whether a separate model reviewing the result is warranted.
    pub require_judge: Option<bool>,
}

impl TaskAssessment {
    /// An assessment carrying only a class — what a legacy bare-token triage
    /// response, or a caller with no model answer, produces.
    pub fn from_class(class: TaskClass) -> Self {
        Self {
            class,
            conversational: false,
            require_witness: None,
            require_judge: None,
        }
    }

    /// Whether an authored witness is warranted, honoring triage's explicit
    /// call and otherwise falling back to what the class implies.
    pub fn wants_witness(&self) -> bool {
        self.require_witness
            .unwrap_or_else(|| self.class.verifies_unconditionally())
    }

    /// Whether a model judge is warranted.
    ///
    /// Defaults to `true`, unlike [`Self::wants_witness`]: this is only ever
    /// consulted once the verification ladder has already run and come back
    /// *inconclusive*, so the class has had its say. Skipping the judge there
    /// has to be an explicit call from triage — a class-derived default would
    /// silently drop the judge on exactly the path the zero-diff guard exists
    /// to catch (a "lookup" that turned out to touch files).
    pub fn wants_judge(&self) -> bool {
        self.require_judge.unwrap_or(true)
    }
}

/// Parse one `KEY: yes|no` line out of a triage response, `None` when the key
/// is absent or its value is not a recognized boolean.
fn parse_flag(lower: &str, key: &str) -> Option<bool> {
    for line in lower.lines() {
        let line = line.trim().trim_start_matches(['-', '*', ' ']);
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start().trim_start_matches(':').trim();
        let word = rest
            .split(|c: char| !c.is_ascii_alphanumeric())
            .find(|w| !w.is_empty())?;
        return match word {
            "yes" | "true" | "required" | "y" => Some(true),
            "no" | "false" | "skip" | "none" | "n" => Some(false),
            _ => None,
        };
    }
    None
}

/// Parse a triage response into a full [`TaskAssessment`].
///
/// Tolerant by construction: the class comes from the same keyword scan the
/// bare-token contract always used, so a model that ignores the structured
/// format still classifies correctly and simply expresses no assurance
/// opinion. A `chat` answer sets [`TaskAssessment::conversational`] (see
/// [`classify_conversational`]). Returns `None` only when neither a class
/// keyword nor a chat signal appears at all.
pub fn parse_triage_response(text: &str) -> Option<TaskAssessment> {
    let conversational = classify_conversational(text);
    let class = match classify_triage_response(text) {
        Some(class) => class,
        // A pure `chat` answer carries no orchestration keyword. Park the class
        // at the cheapest tier — it is never consulted, since the
        // conversational path skips plan/witness/execute/verify. Neither a
        // class nor a chat signal means the response is unparseable: fall
        // through to the deterministic floor exactly as before.
        None if conversational => TaskClass::SimpleLookup,
        None => return None,
    };
    let lower = text.to_ascii_lowercase();
    Some(TaskAssessment {
        class,
        conversational,
        require_witness: parse_flag(&lower, "witness"),
        require_judge: parse_flag(&lower, "judge"),
    })
}

/// Whether a triage response classifies the input as conversational — a `chat`
/// CLASS answer (a greeting, small talk, a question about the agent), not a
/// software task.
///
/// Scans in the same first-recognized-class-keyword-wins order as
/// [`classify_triage_response`], but resolves to a boolean: `true` only when
/// the first class token is a chat token. That ordering is load-bearing — a
/// justification that mentions "chat" *after* a real class token
/// (`CLASS: lookup — just answering a chat about the code`) must not misfire,
/// because routing to chat is terminal no-work and a false positive silently
/// drops a real task.
pub fn classify_conversational(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    for raw in lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        match raw {
            "chat" | "greeting" | "smalltalk" | "small_talk" => return true,
            // Any real class token appearing first overrules a later "chat".
            "lookup" | "simple" | "simple_lookup" | "read" | "explain" | "single"
            | "single_task" | "task" | "fix" | "multi" | "multi_step" | "multistep" | "complex"
            | "plan" => return false,
            _ => {}
        }
    }
    false
}

/// Whether the input should take the conversational (no-work) path.
///
/// Two independent routes, because betting the whole fix on a cheap triage
/// model choosing `chat` is fragile — the model that shipped this bug
/// *recognized* `hi` was odd and forced a task class anyway:
///
/// 1. **Deterministic** ([`is_bare_greeting`]): an unmistakable bare greeting
///    (`hi`, `hello`, `thanks`, an empty submit) routes to chat with no model
///    opinion needed. EXACT whole-message match only — never prefix/substring
///    — so `hey, fix the login bug` can never be swallowed.
/// 2. **Model**: triage said `chat`, and the deterministic floor found no
///    positive task signal to overrule it. Same asymmetry
///    [`resolve_task_class`] relies on: the deterministic layer can only ever
///    *add* work, so it may **veto** "this is just chat" (raising a real task
///    back onto the work path) but never manufacture it. An over-eager `chat`
///    on a goal carrying an enumeration, conjoined imperatives, or
///    cross-cutting scope is overruled.
pub fn resolve_conversational(model_says_chat: bool, goal: &str) -> bool {
    is_bare_greeting(goal)
        || (model_says_chat && deterministic_floor(goal) == TaskClass::SimpleLookup)
}

/// Whether the whole message is nothing but a greeting / acknowledgement — the
/// deterministic half of [`resolve_conversational`]. Normalizes (trim,
/// lowercase, strip surrounding punctuation and quotes) then requires an EXACT
/// match against a small fixed set. Whole-message only by construction: a real
/// task that merely *opens* with a greeting (`hey, fix the login bug`) keeps
/// its trailing words and so never matches. An empty/whitespace-only submit
/// counts too — it is not a task.
fn is_bare_greeting(goal: &str) -> bool {
    let normalized = goal
        .trim()
        .trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, '.' | '!' | '?' | ',' | '"' | '\'' | '`' | ':')
        })
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return true;
    }
    const GREETINGS: &[&str] = &[
        "hi",
        "hii",
        "hiya",
        "hello",
        "hey",
        "heya",
        "yo",
        "sup",
        "hey there",
        "hi there",
        "hello there",
        "howdy",
        "greetings",
        "hi stella",
        "hey stella",
        "hello stella",
        "gm",
        "good morning",
        "good afternoon",
        "good evening",
        "thanks",
        "thank you",
        "thankyou",
        "thx",
        "ty",
        "cheers",
        "ok thanks",
        "ok thank you",
        "nice",
        "cool",
    ];
    GREETINGS.contains(&normalized.as_str())
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
    let Some(model) = model_class else {
        // No usable classification: the floor stands alone, erring toward
        // planning exactly as it always has.
        return floor;
    };
    // Triage read the goal; the floor only pattern-matched it, so triage may
    // route work onto a cheaper path than the keywords guessed — otherwise a
    // one-line fix whose description contains "and then" buys a full
    // plan/witness/judge pipeline forever.
    //
    // But only one level cheaper. Triage is meant to be the weakest, fastest
    // tier, and the floor's keyword evidence is weak rather than worthless;
    // letting a cheap model strip planning, witness, and judge in a single
    // step is a bigger bet than the speed is worth. Raising stays unbounded —
    // erring toward more work was never the risk.
    model.max(floor.one_level_cheaper())
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

/// The prompt handed to the Role::Triage model to classify a user message. A
/// tiny, fixed instruction for a cheap/fast tier: it asks for three labeled
/// lines (`CLASS`/`WITNESS`/`JUDGE`), but the parser tolerates a bare class
/// token too ([`parse_triage_response`]). The first line offers `chat` so a
/// non-task (a greeting, small talk) has somewhere to land instead of being
/// forced onto the work path.
pub fn triage_prompt(goal: &str) -> String {
    format!(
        "Classify the following user message, and decide what assurance its \
         result actually warrants. Do NOT assume it is a software task — it may \
         just be conversation. Answer with EXACTLY these three lines and \
         nothing else:\n\
         CLASS: chat|lookup|single|multi\n\
         WITNESS: yes|no\n\
         JUDGE: yes|no\n\n\
         CLASS is what the message needs:\n\
         - `chat`    — NOT a software task: a greeting (`hi`), thanks, small \
         talk, or a question about you. Reply conversationally; touch no files, \
         write no plan, no test.\n\
         - `lookup`  — a read/explain/search question about the code that \
         changes no files\n\
         - `single`  — one concrete code change\n\
         - `multi`   — genuinely multi-step work spanning several changes\n\n\
         WITNESS is whether a failing test should be written first to pin the \
         intended behavior. Say no when the change is mechanical, when \
         correctness is already obvious from the diff, or when the project \
         has no way to run such a test.\n\
         JUDGE is whether a separate model should review the result. Say no \
         when success is self-evident or a test already proves it.\n\
         Prefer `no` for both on small, self-evident work — ceremony that \
         proves nothing costs the user time and money.\n\n\
         Task:\n{goal}\n\n\
         Answer:"
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
    fn parses_the_structured_assurance_answer() {
        let a = parse_triage_response("CLASS: single\nWITNESS: no\nJUDGE: yes")
            .expect("a well-formed answer parses");
        assert_eq!(a.class, TaskClass::SingleTask);
        assert_eq!(a.require_witness, Some(false));
        assert_eq!(a.require_judge, Some(true));
        assert!(!a.wants_witness(), "an explicit no wins over the class");
        assert!(a.wants_judge());
    }

    #[test]
    fn a_bare_token_answer_still_classifies_and_claims_no_assurance_opinion() {
        // The pre-existing contract: models that ignore the structured format
        // must keep working, falling back to what the class implies.
        let a = parse_triage_response("multi").expect("a bare token parses");
        assert_eq!(a.class, TaskClass::MultiStep);
        assert_eq!(a.require_witness, None);
        assert_eq!(a.require_judge, None);
        assert!(a.wants_witness(), "class-derived default stands");
    }

    #[test]
    fn an_answer_with_no_class_keyword_is_not_a_guess() {
        assert_eq!(parse_triage_response("WITNESS: no\nJUDGE: no"), None);
        assert_eq!(parse_triage_response("hmm, hard to say"), None);
    }

    #[test]
    fn assurance_flags_tolerate_bullets_punctuation_and_case() {
        let a =
            parse_triage_response("- CLASS: Multi\n- Witness: NO\n* judge : none").expect("parses");
        assert_eq!(a.class, TaskClass::MultiStep);
        assert_eq!(a.require_witness, Some(false));
        assert_eq!(a.require_judge, Some(false));
    }

    #[test]
    fn a_chat_answer_parses_as_conversational_and_parks_the_class() {
        // The whole point: a greeting must have somewhere to land. A `chat`
        // CLASS carries no orchestration keyword, so the class parks at the
        // cheapest tier and the conversational flag is what the pipeline reads.
        let a = parse_triage_response("CLASS: chat\nWITNESS: no\nJUDGE: no")
            .expect("a chat answer parses");
        assert!(a.conversational);
        assert_eq!(a.class, TaskClass::SimpleLookup);

        // A bare `chat` token works too (models that ignore the format).
        let bare = parse_triage_response("chat").expect("a bare chat token parses");
        assert!(bare.conversational);
    }

    #[test]
    fn classify_conversational_is_first_class_keyword_wins() {
        assert!(classify_conversational("chat"));
        assert!(classify_conversational("CLASS: chat — greeting, no task"));
        assert!(classify_conversational("greeting"));
        // A real class token appearing FIRST overrules a later "chat": routing
        // to no-work is terminal, so a justification must never flip it.
        assert!(!classify_conversational(
            "CLASS: lookup — just answering a chat about the code"
        ));
        assert!(!classify_conversational("multi"));
        assert!(!classify_conversational(""));
        assert!(!classify_conversational("hmm, not sure"));
    }

    #[test]
    fn a_non_chat_answer_is_not_conversational() {
        assert!(!parse_triage_response("single").unwrap().conversational);
        assert!(
            !parse_triage_response("CLASS: lookup\nWITNESS: no\nJUDGE: no")
                .unwrap()
                .conversational
        );
        // A class carried by `from_class` is never conversational.
        assert!(!TaskAssessment::from_class(TaskClass::SimpleLookup).conversational);
    }

    #[test]
    fn a_bare_greeting_is_conversational_without_any_model_opinion() {
        // The deterministic route: no model answer needed for an unmistakable
        // greeting — the fix must not hinge on a cheap model choosing `chat`.
        assert!(resolve_conversational(false, "hi"));
        assert!(resolve_conversational(false, "  Hi!  "));
        assert!(resolve_conversational(false, "hello"));
        assert!(resolve_conversational(false, "thanks"));
        assert!(resolve_conversational(false, ""));
    }

    #[test]
    fn is_bare_greeting_matches_whole_message_only() {
        for g in [
            "hi",
            "Hi",
            "  hi  ",
            "hi!",
            "hello.",
            "HELLO",
            "hey",
            "yo",
            "hey there",
            "thanks",
            "thank you",
            "thankyou",
            "ty",
            "",
            "   ",
            "hi stella",
            "\"hi\"",
            "gm",
            "good morning",
        ] {
            assert!(is_bare_greeting(g), "{g:?} should be a bare greeting");
        }
        // NOT greetings: a real task that merely opens with one, or anything
        // with trailing content. Whole-message match, never prefix/substring.
        for g in [
            "hey, fix the login bug",
            "hi, can you refactor the parser",
            "fix the parser",
            "hello world function",
            "hey there, add a test",
            "thanks for nothing, delete the file",
            "high", // superstring of "hi"
            "history",
        ] {
            assert!(!is_bare_greeting(g), "{g:?} must NOT be a bare greeting");
        }
    }

    #[test]
    fn the_floor_vetoes_an_over_eager_model_chat_when_there_is_task_signal() {
        // Model says chat, no task signal, not a bare greeting → chat stands.
        assert!(resolve_conversational(true, "what's your favorite color"));
        // Model silent AND not a greeting → not conversational.
        assert!(!resolve_conversational(
            false,
            "what does the retry policy do?"
        ));
        // Positive task signal overrules an over-eager `chat`: an enumeration,
        // conjoined imperatives, or cross-cutting scope is real work no matter
        // what the cheap triage model guessed.
        assert!(!resolve_conversational(
            true,
            "add the field and then update the migration"
        ));
        assert!(!resolve_conversational(
            true,
            "rename all callers of foo across the codebase"
        ));
        assert!(!resolve_conversational(true, "add a field and update it"));
    }

    #[test]
    fn a_judge_opinion_is_required_to_skip_the_judge() {
        // `wants_judge` is only consulted after the ladder came back
        // inconclusive, so silence must never mean "skip".
        for class in [
            TaskClass::SimpleLookup,
            TaskClass::SingleTask,
            TaskClass::MultiStep,
        ] {
            assert!(
                TaskAssessment::from_class(class).wants_judge(),
                "{class:?} with no triage opinion must still reach the judge"
            );
        }
    }

    #[test]
    fn triage_may_lower_the_floor_by_one_level_and_raise_it_without_bound() {
        // Floor says multi (markers present), triage says lookup → lands on
        // single: DAG planning is skipped, verification is kept. Triage read
        // the goal, but it is the weakest tier, so it does not get to strip
        // planning, witness, and judge in one move.
        assert_eq!(
            resolve_task_class(
                Some(TaskClass::SimpleLookup),
                "refactor across the codebase"
            ),
            TaskClass::SingleTask
        );
        // One level down is honored exactly.
        assert_eq!(
            resolve_task_class(Some(TaskClass::SingleTask), "refactor across the codebase"),
            TaskClass::SingleTask
        );
        // A floor of single may be lowered all the way to lookup.
        assert_eq!(
            resolve_task_class(Some(TaskClass::SimpleLookup), "add the field and update it"),
            TaskClass::SimpleLookup
        );
        // Raising stays unbounded: erring toward more work was never the risk.
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
    fn triage_prompt_names_all_four_class_tokens_and_the_goal() {
        let p = triage_prompt("Fix the failing test");
        assert!(p.contains("chat"));
        assert!(p.contains("lookup"));
        assert!(p.contains("single"));
        assert!(p.contains("multi"));
        assert!(p.contains("Fix the failing test"));
    }
}
