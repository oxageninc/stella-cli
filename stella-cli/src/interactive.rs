//! The `ask_user` tool — the agent asking the human a multiple-choice
//! question mid-turn (user-mandated product rule, see
//! `stella_protocol::AgentEvent::AskUser`).
//!
//! BINDING contract: every question presents the model's structured options
//! PLUS always exactly one additional free-text option — the user can
//! always answer in their own words, on every question. The free-text
//! affordance is appended by the runtime (here), never by the model; the
//! tool's schema description forbids the model from listing an "Other"
//! option itself so it can't double up.
//!
//! Architecture: `InteractiveToolSet` wraps any inner
//! `stella_core::ports::ToolExecutor` (the native `ToolRegistry`, or the
//! MCP-merged set once that lands) and adds the `ask_user` schema. Actual
//! I/O goes through the [`AskUserIo`] port so tests never touch a real
//! terminal, and headless runs (`--output-format json|stream-json`, or a
//! non-TTY stdin) get a named error instead of a hang on input that will
//! never arrive.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use colored::Colorize;
use serde_json::Value;
use stella_core::ports::ToolExecutor;
use stella_protocol::{AgentEvent, ToolOutput, ToolSchema};
use tokio::sync::mpsc::UnboundedSender;

/// The label the runtime appends as the always-present free-text option.
pub const FREE_TEXT_LABEL: &str = "Type your own answer";

/// Monotonic source of per-invocation `ask_user` ids. The model's real tool
/// call_id never reaches `ToolExecutor::execute` (its signature is only
/// name+input), so the `AskUser` event can't reuse it — a process-unique id
/// per ask is minted here instead of a constant, keeping successive/concurrent
/// asks individually addressable for any consumer that keys on the id.
static NEXT_ASK_ID: AtomicU64 = AtomicU64::new(0);

/// How the `ask_user` tool actually reaches the human. Injectable so the
/// tool's selection/parsing logic is unit-testable without a TTY.
#[async_trait]
pub trait AskUserIo: Send + Sync {
    /// Present `question` + `options` (already including the free-text
    /// affordance as the final entry) and return the user's raw line.
    async fn prompt(&self, question: &str, options: &[String]) -> Result<String, String>;
}

/// Production io: prints the card to stdout and reads one line from stdin.
/// Safe to use while a turn is in flight — the REPL's own read loop is
/// suspended awaiting the turn, so stdin has exactly one reader.
pub struct TtyAskUserIo;

#[async_trait]
impl AskUserIo for TtyAskUserIo {
    async fn prompt(&self, question: &str, options: &[String]) -> Result<String, String> {
        println!("\n  {} {}", "?".bright_cyan().bold(), question.bold());
        for (i, option) in options.iter().enumerate() {
            println!("    {} {}", format!("{})", i + 1).bright_cyan(), option);
        }
        print!("  {} ", "answer (number or text):".dimmed());
        std::io::stdout().flush().map_err(|e| e.to_string())?;

        // Blocking stdin read off the async runtime's worker threads.
        tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map(|_| line)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// Headless io: always a named error. Chosen when stdin isn't a TTY or the
/// output format is machine-oriented — per the AskUser event's documented
/// contract, headless runs fail the tool loudly rather than hanging.
pub struct HeadlessAskUserIo;

#[async_trait]
impl AskUserIo for HeadlessAskUserIo {
    async fn prompt(&self, _question: &str, _options: &[String]) -> Result<String, String> {
        Err(
            "interactive input is unavailable in this run (no TTY / machine output format) — \
             proceed with your best judgment instead of asking"
                .to_string(),
        )
    }
}

/// Pick the production io for the current process: TTY when stdin is one
/// and the caller wants interactive text output, headless otherwise.
pub fn default_ask_io(interactive_output: bool) -> Box<dyn AskUserIo> {
    if interactive_output && std::io::stdin().is_terminal() {
        Box::new(TtyAskUserIo)
    } else {
        Box::new(HeadlessAskUserIo)
    }
}

/// Registry commands for the skills ecosystem (user requirement: the agent
/// can find skills it doesn't have on the internet and install them into
/// the project behind a user-confirm workflow). Tokenized argv templates —
/// `{query}`/`{id}` substitute as a SINGLE argv token, spawned without a
/// shell, so model-supplied text can never inject. Defaults target the
/// `npx skills` registry CLI; overridable via STELLA_SKILLS_SEARCH_CMD /
/// STELLA_SKILLS_INSTALL_CMD since registry CLIs vary.
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    pub search_cmd: Vec<String>,
    pub install_cmd: Vec<String>,
    /// `npx skills use <id>` — prints a not-yet-installed skill's `SKILL.md`
    /// (wrapped in `<SKILL.md>…</SKILL.md>`); used for the ctrl+o preview.
    pub use_cmd: Vec<String>,
    pub workspace_root: std::path::PathBuf,
}

impl SkillRegistry {
    pub fn from_env(workspace_root: std::path::PathBuf) -> Self {
        let parse = |var: &str, default: &[&str]| -> Vec<String> {
            std::env::var(var)
                .ok()
                .map(|v| v.split_whitespace().map(str::to_string).collect())
                .unwrap_or_else(|| default.iter().map(|s| s.to_string()).collect())
        };
        Self {
            search_cmd: parse(
                "STELLA_SKILLS_SEARCH_CMD",
                &["npx", "skills", "find", "{query}"],
            ),
            install_cmd: parse(
                "STELLA_SKILLS_INSTALL_CMD",
                // `--yes` skips the interactive "which agents do you want to install
                // to?" multi-select that the registry CLI otherwise shows — it would
                // hang forever when stella runs it with stdin null. `-y` is the short
                // form the CLI prints in its own hint ("use --yes (-y) … to install
                // without prompts").
                &["npx", "skills", "add", "--yes", "{id}"],
            ),
            use_cmd: parse("STELLA_SKILLS_USE_CMD", &["npx", "skills", "use", "{id}"]),
            workspace_root,
        }
    }

    /// Substitute `placeholder` with `value` across the template's tokens.
    pub fn render(template: &[String], placeholder: &str, value: &str) -> Vec<String> {
        template
            .iter()
            .map(|t| t.replace(placeholder, value))
            .collect()
    }

    pub(crate) async fn run(&self, argv: Vec<String>, timeout_secs: u64) -> Result<String, String> {
        // Search/install output is a terse list — 6 KB is ample and guards the
        // debug log / UI from a runaway subprocess.
        self.run_capped(argv, timeout_secs, 6000).await
    }

    /// Like [`Self::run`] but with an explicit output cap. The ctrl+o preview
    /// passes a larger cap so a full `SKILL.md` body is not truncated.
    pub(crate) async fn run_capped(
        &self,
        argv: Vec<String>,
        timeout_secs: u64,
        max_bytes: usize,
    ) -> Result<String, String> {
        let Some((program, args)) = argv.split_first() else {
            return Err("empty registry command".into());
        };
        // kill_on_drop: when the timeout below fires, wait_with_output is
        // dropped and the child must die with it — otherwise a wedged npx
        // install keeps running (and downloading) long after the tool
        // reported failure.
        let mut command = tokio::process::Command::new(program);
        stella_tools::exec::scrub_sensitive_env(&mut command);
        let child = command
            .args(args)
            .current_dir(&self.workspace_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to run `{program}`: {e} (is Node/npx installed?)"))?;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| format!("`{program}` timed out after {timeout_secs}s"))?
        .map_err(|e| e.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut text = format!("{}\n{}", stdout.trim(), stderr.trim());
        if text.len() > max_bytes {
            // Truncate on a char boundary — `String::truncate` panics if the
            // cap lands inside a multi-byte character (subprocess output can
            // contain accents/emoji/box-drawing), which would unwind the whole
            // CLI session.
            let end = (0..=max_bytes)
                .rev()
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(0);
            text.truncate(end);
        }
        if output.status.success() {
            Ok(text.trim().to_string())
        } else {
            Err(format!("exit {}: {}", output.status, text.trim()))
        }
    }
}

/// A `ToolExecutor` that adds `ask_user` (plus the skills-registry tools
/// when configured) on top of an inner executor.
pub struct InteractiveToolSet<'a> {
    inner: &'a dyn ToolExecutor,
    events: UnboundedSender<AgentEvent>,
    io: Box<dyn AskUserIo>,
    skills: Option<SkillRegistry>,
}

impl<'a> InteractiveToolSet<'a> {
    pub fn new(
        inner: &'a dyn ToolExecutor,
        events: UnboundedSender<AgentEvent>,
        io: Box<dyn AskUserIo>,
    ) -> Self {
        Self {
            inner,
            events,
            io,
            skills: None,
        }
    }

    /// Enable the skills-registry tools (search_skills / install_skill).
    pub fn with_skill_registry(mut self, registry: SkillRegistry) -> Self {
        if !crate::enterprise_telemetry::process_free_authority_active() {
            self.skills = Some(registry);
        }
        self
    }

    fn skills_schemas() -> Vec<ToolSchema> {
        vec![
            ToolSchema {
                name: "search_skills".into(),
                description: "Search the public skills registry for reusable agent skills \
                              matching a topic. Use when the task would benefit from a skill \
                              you don't have locally. Returns the registry's search results."
                    .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }),
                // A registry search touches no workspace state.
                read_only: true,
            },
            ToolSchema {
                name: "install_skill".into(),
                description: "Install a skill from the registry into this project's \
                              .stella/skills/. ALWAYS asks the user for confirmation first — \
                              the install only proceeds if they approve. Pass the skill id \
                              exactly as shown by search_skills."
                    .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"]
                }),
                // Writes into .stella/skills/ — a workspace mutation.
                read_only: false,
            },
        ]
    }

    async fn execute_search_skills(&self, registry: &SkillRegistry, input: &Value) -> ToolOutput {
        let Some(query) = input.get("query").and_then(Value::as_str) else {
            return ToolOutput::Error {
                message: "search_skills: missing required string field `query`".into(),
            };
        };
        let argv = SkillRegistry::render(&registry.search_cmd, "{query}", query);
        match registry.run(argv, 90).await {
            Ok(out) if out.is_empty() => ToolOutput::Ok {
                content: "no results".into(),
            },
            Ok(out) => ToolOutput::Ok { content: out },
            Err(e) => ToolOutput::Error {
                message: format!("skills search failed: {e}"),
            },
        }
    }

    async fn execute_install_skill(&self, registry: &SkillRegistry, input: &Value) -> ToolOutput {
        let Some(id) = input.get("id").and_then(Value::as_str) else {
            return ToolOutput::Error {
                message: "install_skill: missing required string field `id`".into(),
            };
        };
        // THE user-confirm workflow: installation never proceeds without an
        // explicit yes through the same io ask_user uses (headless io ->
        // named error -> install impossible headlessly, by design).
        let options = vec![
            format!("Yes — install `{id}` into .stella/skills/"),
            "No — don't install".to_string(),
        ];
        let answer = match self
            .io
            .prompt(
                &format!("The agent wants to install skill `{id}` from the registry. Proceed?"),
                &options,
            )
            .await
        {
            Ok(a) => a,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("install_skill: confirmation unavailable: {e}"),
                };
            }
        };
        let approved = matches!(answer.trim(), "1")
            || answer.trim().eq_ignore_ascii_case("yes")
            || answer.trim().eq_ignore_ascii_case("y");
        if !approved {
            return ToolOutput::Ok {
                content: "user declined the installation — do not retry; proceed without it".into(),
            };
        }
        // Stage the registry CLI's write into a private tempdir, then adopt the
        // produced tree into THIS project's `.stella/skills/` — the exact path
        // the command deck uses. Running the install against the workspace
        // directly lets `npx skills add` write global symlinks
        // (`~/.config/stella/skills`) that never appear in the project scope, so
        // the tool reported success yet the skill was absent from the list.
        let tmp = match tempfile::Builder::new().prefix("stella-skill-").tempdir() {
            Ok(t) => t,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("install_skill: could not stage a tempdir: {e}"),
                };
            }
        };
        let mut staged = registry.clone();
        staged.workspace_root = tmp.path().to_path_buf();
        let argv = SkillRegistry::render(&staged.install_cmd, "{id}", id);
        if let Err(e) = staged.run(argv, 300).await {
            return ToolOutput::Error {
                message: format!("skills install failed: {e}"),
            };
        }
        match crate::skill_manager::adopt_tree(
            stella_tui::SkillScope::Project,
            &registry.workspace_root,
            tmp.path(),
            id,
        ) {
            Ok(name) => ToolOutput::Ok {
                content: format!(
                    "installed `{name}` into .stella/skills/. It will be considered for \
                     selection on the next turn."
                ),
            },
            Err(e) => ToolOutput::Error {
                message: format!("skills install produced nothing usable: {e}"),
            },
        }
    }

    fn ask_user_schema() -> ToolSchema {
        ToolSchema {
            name: "ask_user".into(),
            description: "Ask the user a multiple-choice question when a decision is genuinely \
                          theirs to make and guessing would be costlier than asking. Provide 2-6 \
                          short, distinct options. The UI ALWAYS adds one extra free-text option \
                          automatically, so never include an 'Other' / 'something else' option \
                          yourself. Returns the user's answer as text."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The complete question, ending with a question mark." },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "maxItems": 6,
                        "description": "Distinct, mutually exclusive choices. No 'Other' option — it is added automatically."
                    }
                },
                "required": ["question", "options"]
            }),
            // ask_user mutates no state, but it owns stdin for the duration of
            // the prompt — it must never be parallelized with a sibling call,
            // so it is marked mutating (the safe direction) to keep the engine
            // from batching it concurrently.
            read_only: false,
        }
    }

    async fn execute_ask_user(&self, call_id: &str, input: &Value) -> ToolOutput {
        let Some(question) = input.get("question").and_then(Value::as_str) else {
            return ToolOutput::Error {
                message: "ask_user: missing required string field `question`".into(),
            };
        };
        let model_options: Vec<String> = input
            .get("options")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if model_options.len() < 2 {
            return ToolOutput::Error {
                message: "ask_user: `options` must contain at least 2 choices".into(),
            };
        }

        // The event carries the model's structured options; the free-text
        // affordance is appended for the prompt itself (the binding
        // always-one-free-text-option rule lives HERE, in the runtime).
        let _ = self.events.send(AgentEvent::AskUser {
            id: call_id.to_string(),
            question: question.to_string(),
            options: model_options.clone(),
        });

        let mut presented = model_options.clone();
        presented.push(format!("{FREE_TEXT_LABEL}…"));

        let raw = match self.io.prompt(question, &presented).await {
            Ok(line) => line,
            Err(e) => {
                return ToolOutput::Error {
                    message: format!("ask_user failed: {e}"),
                };
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return ToolOutput::Error {
                message: "ask_user: the user gave an empty answer — ask again or proceed with \
                          your best judgment"
                    .into(),
            };
        }

        // A bare number selects that option; picking the free-text slot (or
        // typing anything else) is a free-text answer verbatim.
        let answer = match trimmed.parse::<usize>() {
            Ok(n) if (1..=model_options.len()).contains(&n) => model_options[n - 1].clone(),
            Ok(n) if n == model_options.len() + 1 => {
                // They selected the free-text slot by number; re-prompt once
                // for the actual text.
                match self.io.prompt("Your answer:", &[]).await {
                    Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
                    _ => {
                        return ToolOutput::Error {
                            message: "ask_user: no free-text answer provided".into(),
                        };
                    }
                }
            }
            _ => trimmed.to_string(),
        };

        ToolOutput::Ok { content: answer }
    }
}

#[async_trait]
impl ToolExecutor for InteractiveToolSet<'_> {
    fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas = self.inner.schemas();
        schemas.push(Self::ask_user_schema());
        if self.skills.is_some() {
            schemas.extend(Self::skills_schemas());
        }
        schemas
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutput {
        if name == "ask_user" {
            // `ToolExecutor::execute` doesn't carry the model's tool call_id,
            // so mint a process-unique id per invocation rather than emitting
            // a constant that would collide across every ask in a session
            // (the TUI keys its pending-ask card by this id).
            let id = format!("ask_user-{}", NEXT_ASK_ID.fetch_add(1, Ordering::Relaxed));
            return self.execute_ask_user(&id, input).await;
        }
        if let Some(registry) = &self.skills {
            if name == "search_skills" {
                return self.execute_search_skills(registry, input).await;
            }
            if name == "install_skill" {
                return self.execute_install_skill(registry, input).await;
            }
        }
        self.inner.execute(name, input).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::mpsc;

    use super::*;

    /// Scripted io: records every prompt it was shown, pops answers from a
    /// queue.
    struct ScriptedIo {
        answers: Mutex<Vec<&'static str>>,
        seen_options: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedIo {
        fn new(answers: Vec<&'static str>) -> Self {
            Self {
                answers: Mutex::new(answers),
                seen_options: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AskUserIo for ScriptedIo {
        async fn prompt(&self, _q: &str, options: &[String]) -> Result<String, String> {
            self.seen_options
                .lock()
                .expect("lock")
                .push(options.to_vec());
            let mut answers = self.answers.lock().expect("lock");
            if answers.is_empty() {
                return Err("script exhausted".into());
            }
            Ok(answers.remove(0).to_string())
        }
    }

    /// Minimal inner executor: one native tool, echoes.
    struct FakeInner;
    #[async_trait]
    impl ToolExecutor for FakeInner {
        fn schemas(&self) -> Vec<ToolSchema> {
            vec![ToolSchema {
                name: "bash".into(),
                description: "run".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: false,
            }]
        }
        async fn execute(&self, name: &str, _input: &Value) -> ToolOutput {
            ToolOutput::Ok {
                content: format!("inner ran {name}"),
            }
        }
    }

    fn ask_input() -> Value {
        serde_json::json!({
            "question": "Which migration target?",
            "options": ["local (5433)", "staging"]
        })
    }

    #[tokio::test]
    async fn schemas_include_native_tools_plus_ask_user() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(&inner, tx, Box::new(ScriptedIo::new(vec![])));
        let names: Vec<String> = set.schemas().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"ask_user".to_string()));
    }

    /// An [`AskUserIo`] that shares its scripted state, so tests can keep a
    /// handle and inspect what was presented after the tool ran.
    #[derive(Clone)]
    struct SharedIo(std::sync::Arc<ScriptedIo>);

    #[async_trait]
    impl AskUserIo for SharedIo {
        async fn prompt(&self, q: &str, options: &[String]) -> Result<String, String> {
            self.0.prompt(q, options).await
        }
    }

    #[tokio::test]
    async fn every_question_always_presents_one_extra_free_text_option() {
        // THE user-mandated rule: N model options are always presented as
        // N+1 choices, the last being free text.
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let io = SharedIo(std::sync::Arc::new(ScriptedIo::new(vec!["1"])));
        let handle = io.clone();
        let set = InteractiveToolSet::new(&inner, tx, Box::new(io));
        let _ = set.execute("ask_user", &ask_input()).await;

        let seen = handle.0.seen_options.lock().expect("lock");
        let presented = seen.first().expect("one prompt happened");
        assert_eq!(presented.len(), 3, "2 options + 1 free-text");
        assert!(presented[2].starts_with(FREE_TEXT_LABEL));
    }

    #[tokio::test]
    async fn numeric_answer_selects_that_option() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(&inner, tx, Box::new(ScriptedIo::new(vec!["2"])));
        match set.execute("ask_user", &ask_input()).await {
            ToolOutput::Ok { content } => assert_eq!(content, "staging"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn free_text_answer_returns_verbatim() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(
            &inner,
            tx,
            Box::new(ScriptedIo::new(vec!["actually use the docker instance"])),
        );
        match set.execute("ask_user", &ask_input()).await {
            ToolOutput::Ok { content } => {
                assert_eq!(content, "actually use the docker instance")
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn selecting_the_free_text_slot_by_number_reprompts_for_text() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        // "3" = the free-text slot (2 options + 1); then the actual text.
        let set = InteractiveToolSet::new(
            &inner,
            tx,
            Box::new(ScriptedIo::new(vec!["3", "my own words"])),
        );
        match set.execute("ask_user", &ask_input()).await {
            ToolOutput::Ok { content } => assert_eq!(content, "my own words"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_user_emits_the_ask_user_event_with_structured_options() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(&inner, tx, Box::new(ScriptedIo::new(vec!["1"])));
        let _ = set.execute("ask_user", &ask_input()).await;
        let event = rx.try_recv().expect("AskUser event emitted");
        match event {
            AgentEvent::AskUser {
                question, options, ..
            } => {
                assert!(question.contains("migration"));
                assert_eq!(options.len(), 2, "event carries the model's options only");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn each_ask_user_invocation_gets_a_distinct_event_id() {
        // Regression: the id was a hard-coded constant, so every ask in a
        // session collided. Two invocations must now carry different ids.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(&inner, tx, Box::new(ScriptedIo::new(vec!["1", "1"])));
        let _ = set.execute("ask_user", &ask_input()).await;
        let _ = set.execute("ask_user", &ask_input()).await;
        let id_of = |e: AgentEvent| match e {
            AgentEvent::AskUser { id, .. } => id,
            other => panic!("expected AskUser, got {other:?}"),
        };
        let first = id_of(rx.try_recv().expect("first AskUser event"));
        let second = id_of(rx.try_recv().expect("second AskUser event"));
        assert_ne!(first, second, "ask_user ids must be unique per invocation");
    }

    #[tokio::test]
    async fn headless_io_fails_with_a_named_error_never_hangs() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(&inner, tx, Box::new(HeadlessAskUserIo));
        match set.execute("ask_user", &ask_input()).await {
            ToolOutput::Error { message } => {
                assert!(message.contains("unavailable"), "{message}")
            }
            other => panic!("expected a named error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_input_is_a_named_error() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(&inner, tx, Box::new(ScriptedIo::new(vec![])));
        let out = set
            .execute("ask_user", &serde_json::json!({"question": "?"}))
            .await;
        assert!(out.is_error());
        let out = set
            .execute(
                "ask_user",
                &serde_json::json!({"question": "?", "options": ["only one"]}),
            )
            .await;
        assert!(out.is_error());
    }

    #[tokio::test]
    async fn non_ask_user_tools_fall_through_to_the_inner_executor() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        let set = InteractiveToolSet::new(&inner, tx, Box::new(ScriptedIo::new(vec![])));
        match set.execute("bash", &Value::Null).await {
            ToolOutput::Ok { content } => assert_eq!(content, "inner ran bash"),
            other => panic!("expected inner fallthrough, got {other:?}"),
        }
    }

    /// The default install command must carry `--yes`: `npx skills add` is
    /// otherwise an interactive multi-select that hangs forever when stella
    /// runs it with stdin null. Locking the default here guards against a
    /// regression that would silently re-break TUI installs.
    #[test]
    fn default_install_cmd_is_non_interactive() {
        // Clear any override so the default path is exercised.
        let _lock = crate::test_env::lock();
        unsafe {
            std::env::remove_var("STELLA_SKILLS_INSTALL_CMD");
        }
        let reg = SkillRegistry::from_env(std::path::PathBuf::from("/ws"));
        assert!(
            reg.install_cmd.iter().any(|t| t == "--yes" || t == "-y"),
            "default install cmd must be non-interactive (have --yes/-y): {:?}",
            reg.install_cmd
        );
        // The {id} placeholder is still present for substitution.
        assert!(reg.install_cmd.iter().any(|t| t.contains("{id}")));
    }

    #[tokio::test]
    async fn install_skill_lands_in_the_project_scope_not_the_workspace_root() {
        // THE cli-skills P1: the agent tool used to run the registry CLI
        // directly against the workspace, so the skill was written wherever the
        // CLI defaults (a global `~/.config/stella/skills` symlink) — the tool
        // reported success yet the skill never showed up in the project's list.
        // The fix mirrors the command deck: stage the install into a private
        // tempdir, then `adopt_tree` it into `<ws>/.stella/skills/`.
        let ws = tempfile::tempdir().expect("workspace tempdir");
        // A fake installer that writes a SKILL.md into WHATEVER cwd it is run
        // in. Under the fix that cwd is the private staging tempdir, so this
        // never touches the workspace root.
        let install_script = "mkdir -p demo-skill && printf '%s\\n' \
            '---' 'name: demo-skill' 'description: a demo skill' '---' '# Demo' 'body' \
            > demo-skill/SKILL.md";
        let registry = SkillRegistry {
            search_cmd: vec![],
            install_cmd: vec!["sh".into(), "-c".into(), install_script.into()],
            use_cmd: vec![],
            workspace_root: ws.path().to_path_buf(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let inner = FakeInner;
        // "1" = approve the install confirmation.
        let set = InteractiveToolSet::new(&inner, tx, Box::new(ScriptedIo::new(vec!["1"])))
            .with_skill_registry(registry);

        let out = set
            .execute(
                "install_skill",
                &serde_json::json!({ "id": "acme/demo-skill" }),
            )
            .await;
        assert!(!out.is_error(), "install should succeed, got {out:?}");

        // It landed in the PROJECT scope…
        let landed = ws.path().join(".stella/skills/demo-skill/SKILL.md");
        assert!(
            landed.exists(),
            "skill must land in <ws>/.stella/skills/, got {out:?}"
        );
        // …and the installer's raw write did NOT pollute the workspace root
        // (the pre-fix behavior, which left the project scope empty).
        assert!(
            !ws.path().join("demo-skill/SKILL.md").exists(),
            "install must stage into a tempdir, never write the workspace root directly"
        );
    }
}
