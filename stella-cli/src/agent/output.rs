//! Headless output, durable benchmark telemetry, and reflection controls.

use std::io::Write;
use std::sync::Arc;

use colored::Colorize;
use stella_core::{EventSendError, EventSender};
use stella_protocol::AgentEvent;
use tokio::sync::mpsc;

use crate::OutputFormat;
use crate::memory::ReflectionReport;

/// Surface a post-turn [`ReflectionReport`] for a headless / line-based
/// format — the reflection outcome must never vanish (the silent-reflection
/// blind spot this closes). `stream-json` gets one machine event line so a
/// metering/CI consumer sees that reflection ran and whether it errored;
/// `text` and `json` get a one-line stderr warning ONLY when the reflection
/// model call actually failed — a clean empty reflection is the common,
/// correct case and stays quiet. Never writes stdout in `json` mode, so that
/// format's single-object contract is untouched. A configured durable sink is
/// fail-closed: persistence failure terminates before another paid call can be
/// made. A `None` model error in `text`/`json` prints nothing.
pub(super) fn surface_reflection(report: &ReflectionReport, format: OutputFormat) {
    if format == OutputFormat::StreamJson {
        let line = serde_json::json!({
            "type": "reflect",
            "recorded": report.recorded,
            "error": report.model_error,
        });
        emit_stream_json_line_or_terminate(&line.to_string());
        return;
    }
    if let Some(err) = &report.model_error {
        eprintln!(
            "  {} post-turn reflection skipped — model call failed: {err}",
            "!".yellow()
        );
    }
}

/// Trusted launcher-only sink for timeout-survivable stream-json telemetry.
///
/// Harbor captures stdout only after its exec call returns; an outer timeout
/// can therefore lose all partial output. For benchmark runs the adapter sets
/// this exact mounted-log destination and Stella appends+flushes every complete
/// event itself. No shell/`tee` parent is needed, which is also what lets the
/// adapter `exec stella` with a credential arriving solely on stdin.
const DURABLE_STREAM_JSON_ENV: &str = "STELLA_DURABLE_STREAM_JSON_PATH";
const HARBOR_DURABLE_STREAM_PATH: &str = "/logs/agent/stella-events.jsonl";
const DURABLE_STREAM_FAILURE_EXIT: i32 = 74;

/// Resolve the launcher's fixed durable path without ever accepting a
/// repository-selected append destination. Invalid Unicode is invalid too,
/// and the rejected value is intentionally absent from the error text.
fn configured_durable_stream_path() -> Result<Option<&'static std::path::Path>, String> {
    let Some(path) = std::env::var_os(DURABLE_STREAM_JSON_ENV) else {
        return Ok(None);
    };
    // This is deliberately not a general arbitrary-file output feature.
    // Accepting only Harbor's fixed mounted path prevents a repository .env
    // file from turning stream rendering into an append primitive elsewhere.
    if path != std::ffi::OsStr::new(HARBOR_DURABLE_STREAM_PATH) {
        return Err("invalid durable stream-json path".to_string());
    }
    Ok(Some(std::path::Path::new(HARBOR_DURABLE_STREAM_PATH)))
}

fn open_durable_stream(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "durable stream-json sink is not a regular file",
        ));
    }
    Ok(file)
}

fn append_durable_stream_json_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    let mut file = open_durable_stream(path)?;
    writeln!(file, "{line}")?;
    file.flush()
}

/// Establish the exact mounted sink before a benchmark can spend. A durable
/// path on a non-streaming invocation is a launcher error, not permission to
/// create a file that will never receive events.
pub(super) fn preflight_durable_stream(format: OutputFormat) -> Result<(), String> {
    let Some(path) = configured_durable_stream_path()? else {
        return Ok(());
    };
    if format != OutputFormat::StreamJson {
        return Err(format!(
            "{DURABLE_STREAM_JSON_ENV} requires --output-format stream-json"
        ));
    }
    preflight_durable_stream_path(path)
        .map_err(|error| format!("durable stream-json preflight failed: {error}"))
}

fn preflight_durable_stream_path(path: &std::path::Path) -> std::io::Result<()> {
    let mut file = open_durable_stream(path)?;
    file.flush()
}

/// Persist first, then publish the same complete event to stdout. This order
/// makes stdout a truthful acknowledgement: no observer can see an event that
/// the timeout-survivable sink did not already accept and flush.
fn write_stream_json_line(
    line: &str,
    durable_path: Option<&std::path::Path>,
    stdout: &mut impl Write,
) -> std::io::Result<()> {
    if let Some(path) = durable_path {
        append_durable_stream_json_line(path, line)?;
    }
    writeln!(stdout, "{line}")?;
    stdout.flush()
}

fn emit_stream_json_line(line: &str) -> Result<(), String> {
    let durable_path = configured_durable_stream_path()?;
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    write_stream_json_line(line, durable_path, &mut stdout)
        .map_err(|error| format!("stream-json persistence/output failed: {error}"))
}

/// Renderer tasks cannot safely return a sink failure to the provider loop:
/// the loop could issue another paid request before observing the join error.
/// Exit immediately instead, after a secret-free stderr diagnostic.
pub(super) fn emit_stream_json_line_or_terminate(line: &str) {
    if let Err(error) = emit_stream_json_line(line) {
        terminate_stream_json(&error);
    }
}

pub(super) fn terminate_stream_json(error: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({
            "type": "error",
            "message": error,
        })
    );
    std::process::exit(DURABLE_STREAM_FAILURE_EXIT)
}

/// Build the event boundary for one run. With Harbor's durable path enabled,
/// every producer clone shares one mutex: serialize, append+flush, then enqueue
/// the exact same event while holding that lock. Durable JSONL order therefore
/// equals renderer-channel order even with concurrent streaming callbacks, and
/// a paid StepUsage producer cannot advance until its metering row is durable.
pub(super) fn event_sender_for_run(
    sender: mpsc::UnboundedSender<AgentEvent>,
    format: OutputFormat,
) -> (EventSender, bool) {
    if format == OutputFormat::StreamJson {
        match configured_durable_stream_path() {
            Ok(Some(path)) => {
                return (
                    ordered_durable_event_sender(sender, path.to_path_buf()),
                    true,
                );
            }
            Ok(None) => {}
            Err(error) => terminate_stream_json(&error),
        }
    }
    (EventSender::new(sender), false)
}

fn ordered_durable_event_sender(
    sender: mpsc::UnboundedSender<AgentEvent>,
    path: std::path::PathBuf,
) -> EventSender {
    let ordering = Arc::new(std::sync::Mutex::new(()));
    EventSender::from_fn(move |event| {
        let line = match serde_json::to_string(&event) {
            Ok(line) => line,
            Err(error) => {
                terminate_stream_json(&format!("stream-json serialization failed: {error}"))
            }
        };
        let _guard = ordering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = append_durable_stream_json_line(&path, &line) {
            terminate_stream_json(&format!("durable stream-json write failed: {error}"));
        }
        if sender.send(event).is_err() {
            terminate_stream_json("stream-json renderer stopped before event admission");
        }
        Ok::<(), EventSendError>(())
    })
}

pub(super) fn emit_pre_persisted_stream_json_line_or_terminate(line: &str) {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if let Err(error) = write_stream_json_line(line, None, &mut stdout) {
        terminate_stream_json(&format!("stream-json stdout write failed: {error}"));
    }
}

pub(super) const DISABLE_REFLECTION_ENV: &str = "STELLA_DISABLE_REFLECTION";

/// Whether a one-shot run may make the post-turn reflection model call.
///
/// Reflection is part of Stella's default learning behavior for every output
/// format. Automation that needs to suppress the extra provider call must do
/// so explicitly with `STELLA_DISABLE_REFLECTION=1` (also accepts `true`,
/// `yes`, or `on`, case-insensitively and with surrounding whitespace).
pub(crate) fn one_shot_reflection_enabled(format: OutputFormat) -> bool {
    let supported_format = matches!(
        format,
        OutputFormat::Text | OutputFormat::Json | OutputFormat::StreamJson
    );
    supported_format && !reflection_explicitly_disabled()
}

fn reflection_explicitly_disabled() -> bool {
    std::env::var(DISABLE_REFLECTION_ENV).is_ok_and(|value| is_truthy_env_value(&value))
}

pub(super) fn is_truthy_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod durable_stream_tests {
    use super::*;

    struct SinkCheckingWriter {
        path: std::path::PathBuf,
        written: Vec<u8>,
    }

    impl Write for SinkCheckingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            assert_eq!(
                std::fs::read_to_string(&self.path).unwrap(),
                "{\"type\":\"step_usage\"}\n",
                "durable sink must be flushed before the first stdout write"
            );
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn each_complete_event_is_visible_without_a_terminal_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.jsonl");
        let event = r#"{"type":"step_usage","cost_usd":0.01}"#;

        append_durable_stream_json_line(&path, event).unwrap();

        // This read happens immediately, before any `complete` event or clean
        // process shutdown. It is the unit witness for Harbor killing Stella
        // after a paid call: the completed JSONL record has already crossed
        // the userspace buffer boundary into the mounted file.
        assert_eq!(std::fs::read_to_string(path).unwrap(), format!("{event}\n"));
    }

    #[test]
    fn durable_sink_is_flushed_before_stdout_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut stdout = SinkCheckingWriter {
            path: path.clone(),
            written: Vec::new(),
        };

        write_stream_json_line(r#"{"type":"step_usage"}"#, Some(&path), &mut stdout).unwrap();

        assert_eq!(stdout.written, b"{\"type\":\"step_usage\"}\n");
    }

    #[test]
    fn sink_failure_prevents_stdout_publication() {
        let dir = tempfile::tempdir().unwrap();
        let missing_parent = dir.path().join("missing").join("events.jsonl");
        let mut stdout = Vec::new();

        assert!(write_stream_json_line("{}", Some(&missing_parent), &mut stdout).is_err());
        assert!(stdout.is_empty());
    }

    #[test]
    fn preflight_rejects_non_regular_sink() {
        let dir = tempfile::tempdir().unwrap();
        assert!(preflight_durable_stream_path(dir.path()).is_err());
    }

    #[test]
    fn paid_usage_is_durable_with_renderer_paused_then_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (raw_tx, mut paused_renderer) = mpsc::unbounded_channel();
        let sender = ordered_durable_event_sender(raw_tx, path.clone());
        let stage = AgentEvent::Stage {
            name: stella_protocol::StageKind::Execute,
        };
        let usage = AgentEvent::StepUsage {
            step: 0,
            purpose: Some("execute".to_string()),
            output_text: None,
            model: "provider/model".to_string(),
            input_tokens: 11,
            output_tokens: 7,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            estimated_input_tokens: 10,
            cost_usd: 0.01,
            duration_ms: 5,
            retries: 0,
            tool_calls: 0,
        };

        sender.send(stage.clone()).unwrap();
        sender.send(usage.clone()).unwrap();

        // The renderer has not polled once, yet both preceding context and
        // paid-call metering are already ordered and flushed. Dropping it now
        // simulates timeout/cancellation after provider completion.
        let expected = format!(
            "{}\n{}\n",
            serde_json::to_string(&stage).unwrap(),
            serde_json::to_string(&usage).unwrap()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(
            serde_json::to_string(&paused_renderer.try_recv().unwrap()).unwrap(),
            serde_json::to_string(&stage).unwrap()
        );
        drop(paused_renderer);
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("step_usage")
        );
    }
}
