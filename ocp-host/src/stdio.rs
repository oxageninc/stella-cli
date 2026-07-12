//! Stdio transport: a child-process OCP provider spoken to over its
//! stdin/stdout (`06-context-protocol.md` §3.2 "local providers: child
//! processes over stdio").
//!
//! Two layers:
//!
//! - [`RawStdioConnection`] — the low-level framed pipe. Public because
//!   conformance tooling needs byte-level control (e.g. injecting a
//!   malformed line to probe provider robustness, §3.6). It owns the child,
//!   spawns it under the OCP isolation contract, and guarantees the process
//!   group dies on drop/shutdown.
//! - [`StdioProvider`] — a [`ContextProvider`] built on the connection: it
//!   handshakes once, caches the provider's identity + capabilities, and
//!   serves queries as one request/response round-trip apiece.
//!
//! ## Isolation (`06-context-protocol.md` §3.5, `02-architecture.md` §7)
//!
//! The child is spawned with a **scrubbed environment** — `env_clear()` then
//! an allowlist of only `PATH` (so the program resolves) and `HOME`. No
//! inherited credentials, no ambient secrets: a provider sees exactly the
//! query payload and whatever it indexed through its own declared inputs,
//! nothing the host holds. On Unix the child leads its own process group so
//! the whole subtree is signalled at once and can never outlive the host.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use ocp_types::{Capabilities, ContextQuery, ContextQueryResult, PROTOCOL_VERSION, ProviderInfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as TokioMutex;

use crate::error::HostError;
use crate::provider::ContextProvider;
use crate::wire::{Envelope, decode_line, encode_line, envelope_kind, versions_compatible};

/// How long the handshake waits for a provider's ack before giving up —
/// bounds the "version mismatch = never a hang" guarantee even against a
/// provider that never answers (task deliverable 1).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a graceful `shutdown` waits for the child to exit before the
/// process-group kill backstop fires (task deliverable 2).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Maximum bytes accepted for a single framed line before the child is treated
/// as malformed. Guards the host against a provider that streams without ever
/// emitting a newline (the timeouts bound time, not memory). 16 MiB is far
/// above any legitimate framed message.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// A raw, framed connection to a child-process OCP provider. The low-level
/// primitive [`StdioProvider`] is built on; public so conformance tools can
/// drive the wire directly.
pub struct RawStdioConnection {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    child: Child,
    /// Process-group id (== the child pid, which `setsid` made a group
    /// leader) for the backstop kill. `None` off Unix, where `kill_on_drop`
    /// reaps the direct child only.
    #[cfg_attr(not(unix), allow(dead_code))]
    pgid: Option<i32>,
    /// A stable label for error messages before the handshake names the
    /// provider.
    label: String,
}

impl RawStdioConnection {
    /// Spawn `program` with `args` as an OCP provider child, under the
    /// isolation contract (scrubbed env, own process group). Does **not**
    /// handshake — call [`RawStdioConnection::handshake`] next.
    pub async fn spawn(program: &str, args: &[String]) -> Result<Self, HostError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        // Provider diagnostics flow to the host's own stderr — never captured
        // as frames, never mistaken for protocol.
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);

        // Scrub the environment: no inherited credentials (§3.5). Allowlist
        // only PATH (so `program` resolves) and HOME.
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        if let Ok(home) = std::env::var("HOME") {
            cmd.env("HOME", home);
        }

        // New session/process group so the whole subtree can be signalled at
        // once on drop/shutdown.
        #[cfg(unix)]
        {
            // SAFETY: `setsid` is async-signal-safe and only reparents the
            // child's own process-group membership in the window between fork
            // and exec — the same narrowly-scoped OS-boundary use
            // `stella-tools`' bash tool makes (`02-architecture.md` §1.2).
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| HostError::Spawn(format!("{program}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HostError::Spawn(format!("{program}: child has no stdin pipe")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::Spawn(format!("{program}: child has no stdout pipe")))?;

        #[cfg(unix)]
        let pgid = child.id().map(|id| id as i32);
        #[cfg(not(unix))]
        let pgid = None;

        Ok(Self {
            stdin,
            stdout: BufReader::new(stdout),
            child,
            pgid,
            label: program.to_string(),
        })
    }

    /// Override the connection's error label (defaults to the program name).
    /// [`StdioProvider`] sets it to the provider's host-facing id.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Send one envelope as an NDJSON line.
    pub async fn send(&mut self, env: &Envelope) -> Result<(), HostError> {
        let line = encode_line(env)?;
        self.send_raw_line(&line).await
    }

    /// Write a raw line to the provider's stdin verbatim — the escape hatch
    /// conformance uses to inject a malformed line (§3.6). A trailing `\n` is
    /// appended if missing so the provider's line reader unblocks.
    pub async fn send_raw_line(&mut self, line: &str) -> Result<(), HostError> {
        let label = self.label.clone();
        // A write into a closed stdin means the child is gone — the
        // write-side twin of the read-side EOF in `recv`, so both surface
        // as ProviderCrashed rather than racing between two error shapes.
        let transport = |e: std::io::Error| match e.kind() {
            std::io::ErrorKind::BrokenPipe => HostError::ProviderCrashed { id: label.clone() },
            _ => HostError::Transport {
                id: label.clone(),
                message: e.to_string(),
            },
        };
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(transport)?;
        if !line.ends_with('\n') {
            self.stdin.write_all(b"\n").await.map_err(transport)?;
        }
        self.stdin.flush().await.map_err(transport)?;
        Ok(())
    }

    /// Read the next raw line, or `None` at EOF (the child closed stdout).
    ///
    /// Bounded to [`MAX_LINE_BYTES`] via an incremental `fill_buf`/`consume`
    /// loop: a buggy or hostile child that streams bytes without ever emitting
    /// a newline would otherwise grow a single `String` without limit (the
    /// handshake/query timeouts bound *time*, not *memory*) and OOM the host.
    pub async fn read_raw_line(&mut self) -> Result<Option<String>, HostError> {
        let label = self.label.clone();
        let transport = |message: String| HostError::Transport {
            id: label.clone(),
            message,
        };
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let buf = self
                .stdout
                .fill_buf()
                .await
                .map_err(|e| transport(e.to_string()))?;
            if buf.is_empty() {
                break; // EOF — deliver any final unterminated line, else None.
            }
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                bytes.extend_from_slice(&buf[..=pos]);
                self.stdout.consume(pos + 1);
                break;
            }
            bytes.extend_from_slice(buf);
            let consumed = buf.len();
            self.stdout.consume(consumed);
            if bytes.len() > MAX_LINE_BYTES {
                return Err(transport(format!(
                    "provider emitted a line exceeding {MAX_LINE_BYTES} bytes without a newline"
                )));
            }
        }
        if bytes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
        }
    }

    /// Read the next envelope. A closed stream (the child died) is
    /// [`HostError::ProviderCrashed`] — never a hang or a panic
    /// (task deliverable 5).
    pub async fn recv(&mut self) -> Result<Envelope, HostError> {
        match self.read_raw_line().await? {
            Some(line) => decode_line(&line),
            None => Err(HostError::ProviderCrashed {
                id: self.label.clone(),
            }),
        }
    }

    /// Perform the OCP handshake (§3.2): send `handshake`, expect
    /// `handshake_ack`, and reject an incompatible protocol version with a
    /// named error. Bounded by [`HANDSHAKE_TIMEOUT`] so a silent provider
    /// fails cleanly rather than hanging (task deliverable 1).
    pub async fn handshake(&mut self) -> Result<(ProviderInfo, Capabilities), HostError> {
        self.send(&Envelope::Handshake {
            protocol_version: PROTOCOL_VERSION.to_string(),
        })
        .await?;

        let ack = match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.recv()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(HostError::Timeout {
                    id: self.label.clone(),
                    timeout_ms: HANDSHAKE_TIMEOUT.as_millis() as u64,
                });
            }
        };

        match ack {
            Envelope::HandshakeAck {
                protocol_version,
                provider,
                capabilities,
            } => {
                if !versions_compatible(PROTOCOL_VERSION, &protocol_version) {
                    return Err(HostError::VersionMismatch {
                        host: PROTOCOL_VERSION.to_string(),
                        provider: provider.name,
                        provider_version: protocol_version,
                    });
                }
                Ok((provider, capabilities))
            }
            other => Err(HostError::UnexpectedEnvelope {
                id: self.label.clone(),
                expected: "handshake_ack".into(),
                got: envelope_kind(&other).into(),
            }),
        }
    }

    /// Send `shutdown` and wait a bounded grace for the child to exit,
    /// killing the process group if it overstays (task deliverable 2). A
    /// provider that already died is not treated as a shutdown error.
    pub async fn shutdown(&mut self) -> Result<(), HostError> {
        let _ = self.send(&Envelope::Shutdown).await;
        let label = self.label.clone();
        match tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(HostError::Transport {
                id: label,
                message: e.to_string(),
            }),
            Err(_) => {
                self.kill_group();
                Ok(())
            }
        }
    }

    /// SIGKILL the whole process group (Unix) and the direct child. Idempotent
    /// — signalling an already-dead group is a harmless, ignored `ESRCH`.
    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: `-pgid` targets the process group this connection
            // created via `setsid`; a stale/dead group returns `ESRCH`,
            // which we ignore.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for RawStdioConnection {
    fn drop(&mut self) {
        // Backstop: even if a caller forgot `shutdown`, the child tree dies
        // with the host (`02-architecture.md` §8 — no orphaned children).
        self.kill_group();
    }
}

/// A [`ContextProvider`] backed by a child process over stdio. Handshakes
/// once on construction and caches the negotiated identity + capabilities.
pub struct StdioProvider {
    id: String,
    info: ProviderInfo,
    capabilities: Capabilities,
    conn: TokioMutex<RawStdioConnection>,
}

impl StdioProvider {
    /// Spawn a child-process provider, complete the handshake, and cache its
    /// declared identity + capabilities. `id` is the host-facing routing and
    /// consent key. Fails cleanly (killing the child) on a bad or
    /// incompatible handshake.
    pub async fn spawn(
        id: impl Into<String>,
        program: &str,
        args: &[String],
    ) -> Result<Self, HostError> {
        let id = id.into();
        let mut conn = RawStdioConnection::spawn(program, args)
            .await?
            .with_label(id.clone());
        let (info, capabilities) = conn.handshake().await?;
        Ok(Self {
            id,
            info,
            capabilities,
            conn: TokioMutex::new(conn),
        })
    }
}

#[async_trait]
impl ContextProvider for StdioProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn query(&self, query: &ContextQuery) -> Result<ContextQueryResult, HostError> {
        let mut conn = self.conn.lock().await;
        conn.send(&Envelope::Query {
            query: query.clone(),
        })
        .await?;
        match conn.recv().await? {
            Envelope::Frames { result } => Ok(result),
            Envelope::Error { message } => Err(HostError::Provider {
                id: self.id.clone(),
                message,
            }),
            other => Err(HostError::UnexpectedEnvelope {
                id: self.id.clone(),
                expected: "frames".into(),
                got: envelope_kind(&other).into(),
            }),
        }
    }

    async fn shutdown(&self) -> Result<(), HostError> {
        let mut conn = self.conn.lock().await;
        conn.shutdown().await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ocp_types::{ContextFrame, FrameKind};

    /// Build a one-shot bash "provider" that emits `script` lines. Bash's
    /// `read`/`printf` are builtins, so it works under the scrubbed env
    /// (only PATH/HOME forwarded).
    fn bash_provider(script: &str) -> (String, Vec<String>) {
        (
            "bash".to_string(),
            vec!["-c".to_string(), script.to_string()],
        )
    }

    fn ack_line(version: &str) -> String {
        // A minimal, well-formed handshake_ack the bash provider can echo.
        let ack = Envelope::HandshakeAck {
            protocol_version: version.to_string(),
            provider: ProviderInfo {
                name: "bash-fixture".into(),
                version: "0.0.1".into(),
                data_flow: ocp_types::DataFlow {
                    reads: true,
                    writes: false,
                    egress: false,
                },
            },
            capabilities: Capabilities {
                query: ocp_types::capability::QueryCapability {
                    kinds: vec!["doc".into()],
                    filters: vec![],
                },
                ..Capabilities::default()
            },
        };
        serde_json::to_string(&ack).unwrap()
    }

    fn frames_line() -> String {
        let frame = ContextFrame {
            id: "frm_1".into(),
            kind: FrameKind::Doc,
            title: "README".into(),
            content: "hello from a stdio provider".into(),
            uri: Some("file:///README.md".into()),
            score: 0.7,
            token_cost: 12,
            valid_from: None,
            valid_to: None,
            recorded_at: None,
            provenance: vec![],
            citation_label: Some("README.md".into()),
            embedding: None,
            relations: vec![],
        };
        let env = Envelope::Frames {
            result: ContextQueryResult {
                frames: vec![frame],
                truncated: false,
                dropped_estimate: None,
            },
        };
        serde_json::to_string(&env).unwrap()
    }

    fn sample_query() -> ContextQuery {
        ContextQuery {
            goal: "g".into(),
            query_text: None,
            embedding: None,
            kinds: vec![],
            anchors: vec![],
            max_frames: 5,
            max_tokens: 4000,
            as_of: None,
        }
    }

    #[tokio::test]
    async fn full_handshake_and_query_round_trip_over_stdio() {
        // Reads the handshake, acks; reads the query, replies with frames.
        let script = format!(
            "read h; printf '%s\\n' '{}'; read q; printf '%s\\n' '{}'",
            ack_line(PROTOCOL_VERSION),
            frames_line()
        );
        let (program, args) = bash_provider(&script);
        let provider = StdioProvider::spawn("docs", &program, &args)
            .await
            .expect("handshake should succeed");
        assert_eq!(provider.id(), "docs");
        assert_eq!(provider.info().name, "bash-fixture");
        assert!(provider.capabilities().query.kinds.contains(&"doc".into()));

        let result = provider.query(&sample_query()).await.expect("query ok");
        assert_eq!(result.frames.len(), 1);
        assert_eq!(result.frames[0].title, "README");
    }

    #[tokio::test]
    async fn an_incompatible_protocol_version_is_a_named_error_not_a_hang() {
        let script = format!("read h; printf '%s\\n' '{}'", ack_line("ocp/2.0"));
        let (program, args) = bash_provider(&script);
        let err = match StdioProvider::spawn("docs", &program, &args).await {
            Ok(_) => panic!("a version mismatch must reject the provider"),
            Err(e) => e,
        };
        match err {
            HostError::VersionMismatch {
                provider_version, ..
            } => assert_eq!(provider_version, "ocp/2.0"),
            other => panic!("expected VersionMismatch, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_child_dying_after_handshake_surfaces_as_provider_crashed() {
        // Acks the handshake, then exits before the query — the crash path.
        let script = format!(
            "read h; printf '%s\\n' '{}'; exit 0",
            ack_line(PROTOCOL_VERSION)
        );
        let (program, args) = bash_provider(&script);
        let provider = StdioProvider::spawn("docs", &program, &args)
            .await
            .expect("handshake ok");
        let err = provider
            .query(&sample_query())
            .await
            .expect_err("a dead child must error, not hang");
        assert!(
            matches!(err, HostError::ProviderCrashed { .. }),
            "expected ProviderCrashed, got {err}"
        );
    }

    #[tokio::test]
    async fn the_child_is_spawned_with_a_scrubbed_environment() {
        // Pick a variable the parent test process has that scrubbing must
        // strip — anything but the PATH/HOME allowlist and bash's own
        // re-injected names. `cargo test` always sets CARGO_* vars, so one
        // exists.
        let injected = ["PWD", "SHLVL", "_", "HOME", "PATH", "OLDPWD"];
        let leaked = std::env::vars()
            .map(|(k, _)| k)
            .find(|k| !injected.contains(&k.as_str()) && !k.is_empty())
            .expect("the test process has at least one non-allowlisted env var");

        // A raw connection running `env`; read its environment dump.
        let mut conn = RawStdioConnection::spawn("bash", &["-c".into(), "env".into()])
            .await
            .expect("spawn env");
        let mut child_keys = Vec::new();
        while let Some(line) = conn.read_raw_line().await.expect("read env line") {
            if let Some((key, _)) = line.trim_end().split_once('=') {
                child_keys.push(key.to_string());
            }
        }
        assert!(
            !child_keys.contains(&leaked),
            "scrubbed child leaked parent env var `{leaked}` — credentials must not cross (§3.5)"
        );
    }
}
