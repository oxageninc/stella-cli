"""Harbor installed-agent adapter for the Stella coding CLI.

Implements Harbor's ``BaseInstalledAgent`` so the ``stella`` binary can be
benchmarked on Terminal-Bench and SWE-bench head-to-head with Claude Code,
Terminus, Codex CLI, and any other Harbor-supported agent, under the same
verifier and container.

Harbor is the runner behind Terminal-Bench 2.x. This adapter is a *third-party*
agent: Harbor loads it by import path, not from its built-in agent registry.

    harbor run \\
      --dataset terminal-bench/terminal-bench-2-1 \\
      --agent-import-path stella_harbor:StellaAgent \\
      --model anthropic/claude-fable-5 \\
      --n-concurrent 4

How it works
------------
Stella is a Rust binary distributed as a standalone executable. This adapter:

1. Locates the compiled ``stella`` binary on the host (``STELLA_BINARY`` /
   ``PATH`` / ``./target/release/stella``).
2. Uploads it into the task container and installs it as
   ``/usr/local/bin/stella``.
3. Best-effort provisions the fast-search tools (``rg``, ``fd``) the agent
   likes to use, when present on the host.
4. Runs Stella one-shot in the task working directory, headless, emitting the
   ``--output-format json`` envelope — a stable machine interface.
5. Parses that envelope for Harbor's result context (cost, tokens, model).

Model selection
---------------
Harbor's ``--model provider/model`` reaches the agent as ``self.model_name``
(e.g. ``anthropic/claude-fable-5``) and is forwarded verbatim to Stella's
``--model`` flag. ``STELLA_MODEL`` in the environment overrides it; if neither
is set the adapter falls back to a documented default.

Environment
-----------
Stella is BYOK (bring-your-own-key). The adapter forwards, from the host into
the container:

- every ``STELLA_*`` variable (``STELLA_MODEL``, ``STELLA_BUDGET``,
  ``STELLA_BASE_URL``, ``STELLA_BINARY``, ...);
- the provider credential and addressing variables Stella's provider registry
  reads directly (one family per supported provider — see
  ``_PROVIDER_ENV_VARS``);
- any ``extra_env`` / declared ``ENV_VARS`` Harbor resolved for this agent.

**Z.ai (GLM) users**: set ``STELLA_BASE_URL=https://api.z.ai/api/coding/paas/v4``
for coding plans. The endpoint must include ``/coding/`` — the non-coding
endpoint returns HTTP 429 "insufficient balance".
"""

from __future__ import annotations

import json
import os
import re
import shlex
import sys
from pathlib import Path
from typing import Any

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

try:
    # Harbor renders prompt templates onto the ``instruction`` argument of a
    # ``run`` decorated with this helper. It is optional sugar: with no
    # template configured it is a pass-through, so a version of Harbor that
    # relocates or renames it must not break the adapter.
    from harbor.agents.installed.base import with_prompt_template
except ImportError:  # pragma: no cover - depends on the installed Harbor version

    def with_prompt_template(fn: Any) -> Any:
        """Fallback no-op used when Harbor does not export the real helper."""
        return fn


# Installation paths (inside the task container).
_BINARY_NAME = "stella"
_INSTALL_PATH = "/usr/local/bin/stella"
_REMOTE_TMP = "/tmp/stella-upload"

# Filenames written under ``self.logs_dir`` (host side) for durability/debug.
_RUN_JSON_NAME = "stella-run.json"
_RUN_STDERR_NAME = "stella-run.stderr.txt"

# Defaults when neither Harbor nor the environment specify a value.
_DEFAULT_MODEL = "anthropic/claude-fable-5"
_DEFAULT_BUDGET = "5.0"

_ENV_PREFIX = "STELLA_"

# Provider credentials and provider-specific addressing variables that Stella's
# provider registry reads directly (stella-cli/src/config.rs providers +
# agent.rs provider construction). One family per supported provider, plus the
# documented aliases, so every BYOK provider can authenticate in the container.
_PROVIDER_ENV_VARS: tuple[str, ...] = (
    # Anthropic · OpenAI · xAI · DeepSeek · Z.ai · OpenRouter · Gemini
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "ZAI_API_KEY",
    "ZAI_GLM_CODING_PLAN",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",  # documented Gemini alias
    # Google Vertex AI
    "VERTEX_ACCESS_TOKEN",
    "VERTEX_PROJECT_ID",
    "VERTEX_LOCATION",
    "GOOGLE_CLOUD_PROJECT",
    # Amazon Bedrock
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    # Local / any OpenAI-compatible gateway
    "LOCAL_API_KEY",
)


def _is_truthy(value: str | None) -> bool:
    """Return whether a string environment variable represents truth."""
    if not value:
        return False
    return value.strip().lower() in ("1", "true", "yes", "on")


def _cached_binary(name: str) -> Path | None:
    """Return the first executable named ``name`` found on ``PATH``, or None."""
    for path in os.environ.get("PATH", "").split(os.pathsep):
        if not path:
            continue
        candidate = Path(path) / name
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def _locate_binary() -> Path:
    """Find the Stella binary on the host.

    Resolution order: explicit ``STELLA_BINARY`` env var, then ``stella`` on
    ``PATH``, then ``./target/release/stella`` walking up from the current
    working directory. Never imported at module load — only ``install`` calls
    this, so importing the adapter never requires Stella to be present.
    """
    explicit = os.environ.get("STELLA_BINARY")
    if explicit:
        binary = Path(explicit)
        if not binary.is_file():
            raise FileNotFoundError(
                f"STELLA_BINARY={explicit!r} does not point at a file"
            )
        return binary

    on_path = _cached_binary(_BINARY_NAME)
    if on_path is not None:
        return on_path

    cwd = Path.cwd()
    while True:
        candidate = cwd / "target" / "release" / _BINARY_NAME
        if candidate.is_file():
            return candidate
        if cwd == cwd.parent:  # reached filesystem root
            break
        cwd = cwd.parent

    raise FileNotFoundError(
        f"cannot find the {_BINARY_NAME!r} binary. Build it with "
        "`cargo build --release -p stella-cli` (produces ./target/release/"
        "stella), put it on PATH, or set STELLA_BINARY=/path/to/stella."
    )


def _sum_step_usage(events: list[Any]) -> dict[str, int]:
    """Aggregate token usage across a turn's ``step_usage`` events.

    Each committed model call emits one ``{"type": "step_usage", ...}`` event
    carrying the normalized usage envelope (stella-protocol ``AgentEvent``).
    Summing them yields the turn totals. Fully defensive: an unexpected shape
    contributes nothing rather than raising.
    """
    totals = {"input": 0, "output": 0, "cache": 0}
    for event in events:
        if not isinstance(event, dict) or event.get("type") != "step_usage":
            continue
        for src, dst in (
            ("input_tokens", "input"),
            ("output_tokens", "output"),
            ("cached_input_tokens", "cache"),
        ):
            value = event.get(src)
            if isinstance(value, (int, float)):
                totals[dst] += int(value)
    return totals


def _extract_metrics(stdout: str | None) -> dict[str, Any]:
    """Parse Stella's ``--output-format json`` envelope into a metrics dict.

    Returns keys ``cost_usd`` (float | None), ``n_input_tokens`` /
    ``n_output_tokens`` / ``n_cache_tokens`` (int | None), ``status`` /
    ``model`` (str | None), and ``steps`` (int | None). Never raises: a
    missing or malformed envelope yields all-None so a benchmark run is never
    aborted by a metadata-parsing edge case.
    """
    empty: dict[str, Any] = {
        "cost_usd": None,
        "n_input_tokens": None,
        "n_output_tokens": None,
        "n_cache_tokens": None,
        "status": None,
        "model": None,
        "steps": None,
    }
    if not stdout or not stdout.strip():
        return empty

    envelope = _load_json_object(stdout)
    if envelope is None:
        # Last resort: the envelope's total cost is a stable, greppable key
        # even if the surrounding JSON did not parse (e.g. truncated output).
        match = re.search(r'"cost_usd"\s*:\s*([0-9]+(?:\.[0-9]+)?)', stdout)
        if match:
            return {**empty, "cost_usd": float(match.group(1))}
        return empty

    metrics = dict(empty)
    cost = envelope.get("cost_usd")
    if isinstance(cost, (int, float)):
        metrics["cost_usd"] = float(cost)

    status = envelope.get("status")
    if isinstance(status, str):
        metrics["status"] = status
    model = envelope.get("model")
    if isinstance(model, str):
        metrics["model"] = model

    events = envelope.get("events")
    if isinstance(events, list):
        totals = _sum_step_usage(events)
        # Only surface token totals when at least one usage event was present;
        # all-zero would misrepresent "no data" as "zero tokens".
        if any(totals.values()):
            metrics["n_input_tokens"] = totals["input"]
            metrics["n_output_tokens"] = totals["output"]
            metrics["n_cache_tokens"] = totals["cache"]
        step_events = [
            e
            for e in events
            if isinstance(e, dict) and e.get("type") == "step_usage"
        ]
        if step_events:
            metrics["steps"] = len(step_events)

    return metrics


def _load_json_object(text: str) -> dict[str, Any] | None:
    """Best-effort parse of a JSON object from ``text``.

    Tries the whole string first (Stella prints exactly one pretty-printed
    object in JSON mode), then falls back to the outermost ``{...}`` slice in
    case an unrelated line leaked onto stdout. Returns None on failure.
    """
    text = text.strip()
    try:
        parsed = json.loads(text)
        return parsed if isinstance(parsed, dict) else None
    except json.JSONDecodeError:
        pass

    start = text.find("{")
    end = text.rfind("}")
    if start == -1 or end <= start:
        return None
    try:
        parsed = json.loads(text[start : end + 1])
        return parsed if isinstance(parsed, dict) else None
    except json.JSONDecodeError:
        return None


class StellaAgent(BaseInstalledAgent):
    """Run the Stella coding CLI as a Harbor installed agent."""

    # Metrics captured during run() and consumed by populate_context_post_run().
    _metrics: dict[str, Any]
    _return_code: int | None

    @staticmethod
    def name() -> str:
        return "stella"

    def get_version_command(self) -> str | None:
        # Overriding this (base default returns None) enables Harbor's
        # post-install version auto-detection.
        return "stella --version"

    def parse_version(self, stdout: str) -> str:
        """Parse ``stella --version`` output (e.g. ``stella 0.3.0``)."""
        for line in stdout.strip().splitlines():
            stripped = line.strip()
            if stripped:
                return stripped
        return stdout.strip()

    async def install(self, environment: BaseEnvironment) -> None:
        """Upload the Stella binary and install it as ``stella`` on PATH."""
        binary_path = _locate_binary()

        await environment.upload_file(str(binary_path), _REMOTE_TMP)
        await self.exec_as_root(
            environment,
            command=(
                f"cp {_REMOTE_TMP} {_INSTALL_PATH} && "
                f"chmod +x {_INSTALL_PATH} && "
                f"{_INSTALL_PATH} --version"
            ),
            timeout_sec=120,
        )

        # Best-effort: give the agent the fast-search tools it prefers.
        await self._provision_search_tool(environment, "rg")
        await self._provision_search_tool(environment, "fd")

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Run Stella one-shot on the task, headless, in JSON mode.

        Invoked as::

            stella --model <model> --budget <usd> --output-format json run "<task>"

        The command runs in the container's default working directory (the task
        repo). Stella's JSON envelope is captured from stdout for
        ``populate_context_post_run``; a non-zero Stella exit is recorded but
        never raised — the benchmark verifier, not the agent's exit code,
        decides task success.
        """
        command = self._build_command(instruction)
        env = self._forwarded_env()

        result = await self.exec_as_agent(environment, command=command, env=env)

        stdout = getattr(result, "stdout", None)
        stderr = getattr(result, "stderr", None)
        self._return_code = getattr(result, "return_code", None)
        self._metrics = _extract_metrics(stdout)

        # Persist the raw envelope (and any stderr) host-side for debugging.
        self._write_log(_RUN_JSON_NAME, stdout)
        if stderr:
            self._write_log(_RUN_STDERR_NAME, stderr)

    def _build_command(self, instruction: str) -> str:
        """Build the headless one-shot Stella command string.

        Global flags (``--model``, ``--budget``, ``--base-url``,
        ``--output-format``) precede the ``run`` subcommand — they are top-level
        CLI flags in Stella, not flags of ``run``.
        """
        model = (
            getattr(self, "model_name", None)
            or os.environ.get("STELLA_MODEL")
            or _DEFAULT_MODEL
        )
        budget = os.environ.get("STELLA_BUDGET", _DEFAULT_BUDGET)
        base_url = os.environ.get("STELLA_BASE_URL")

        parts = [
            "stella",
            "--model",
            shlex.quote(model),
            "--budget",
            shlex.quote(budget),
            "--output-format",
            "json",
        ]
        if base_url:
            parts += ["--base-url", shlex.quote(base_url)]
        parts += ["run", shlex.quote(instruction)]
        return " ".join(parts)

    def _forwarded_env(self) -> dict[str, str]:
        """Collect host env vars to forward into the container.

        Forwards every ``STELLA_*`` variable, the provider credential/addressing
        vars Stella reads directly, and any ``extra_env`` / resolved ``ENV_VARS``
        Harbor prepared for this agent.
        """
        forwarded: dict[str, str] = {}

        for key, value in os.environ.items():
            if key.startswith(_ENV_PREFIX):
                forwarded[key] = value

        for key in _PROVIDER_ENV_VARS:
            value = os.environ.get(key)
            if value is not None:
                forwarded[key] = value

        # Harbor-resolved declared env vars and per-run --env overrides, if the
        # installed Harbor version exposes them (attribute names vary by
        # version, so read defensively).
        for attr in ("_resolved_env_vars", "extra_env"):
            extra = getattr(self, attr, None)
            if isinstance(extra, dict):
                forwarded.update({str(k): str(v) for k, v in extra.items()})

        return forwarded

    async def _provision_search_tool(
        self, environment: BaseEnvironment, tool: str
    ) -> None:
        """Provision a host fast-search tool (``rg``/``fd``) in the container.

        Best-effort: skipped if the host lacks the binary or the container
        already has it. Never fails the install.
        """
        host_binary = _cached_binary(tool)
        if host_binary is None:
            return

        try:
            probe = await self.exec_as_root(
                environment, command=f"command -v {tool}", timeout_sec=15
            )
            if getattr(probe, "return_code", 1) == 0:
                return  # already present in the container
        except Exception:  # noqa: BLE001 - probe failure is non-fatal
            pass  # fall through and try to install it

        remote_tmp = f"/tmp/{tool}-upload"
        try:
            await environment.upload_file(str(host_binary), remote_tmp)
            await self.exec_as_root(
                environment,
                command=(
                    f"cp {remote_tmp} /usr/local/bin/{tool} && "
                    f"chmod +x /usr/local/bin/{tool}"
                ),
                timeout_sec=60,
            )
        except Exception as exc:  # noqa: BLE001 - optional convenience tool
            print(
                f"stella-adapter: could not provision {tool}: {exc}",
                file=sys.stderr,
            )

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Populate Harbor's result context from the captured JSON envelope."""
        metrics = getattr(self, "_metrics", None)
        if metrics is None:
            # run() never captured an envelope; fall back to the host log file.
            metrics = _extract_metrics(self._read_log(_RUN_JSON_NAME))

        if metrics.get("cost_usd") is not None:
            context.cost_usd = metrics["cost_usd"]
        if metrics.get("n_input_tokens") is not None:
            context.n_input_tokens = metrics["n_input_tokens"]
        if metrics.get("n_output_tokens") is not None:
            context.n_output_tokens = metrics["n_output_tokens"]
        if metrics.get("n_cache_tokens") is not None:
            context.n_cache_tokens = metrics["n_cache_tokens"]

        extra = {
            "stella_status": metrics.get("status"),
            "stella_model": metrics.get("model"),
            "stella_steps": metrics.get("steps"),
            "stella_return_code": getattr(self, "_return_code", None),
        }
        extra = {k: v for k, v in extra.items() if v is not None}
        if extra:
            context.metadata = {**(context.metadata or {}), **extra}

    # -- host-side log helpers ------------------------------------------------

    def _log_path(self, name: str) -> Path | None:
        logs_dir = getattr(self, "logs_dir", None)
        if logs_dir is None:
            return None
        return Path(logs_dir) / name

    def _write_log(self, name: str, content: str | None) -> None:
        if not content:
            return
        path = self._log_path(name)
        if path is None:
            return
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        except OSError as exc:
            print(f"stella-adapter: could not write {name}: {exc}", file=sys.stderr)

    def _read_log(self, name: str) -> str | None:
        path = self._log_path(name)
        if path is None or not path.is_file():
            return None
        try:
            return path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return None
