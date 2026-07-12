"""Harbor agent adapter for the Stella coding CLI.

Implements Harbor's `BaseInstalledAgent` so the `stella` agent can be
benchmarked on Terminal-Bench and SWE-bench head-to-head with Claude Code,
Codex CLI, Oxagen, and other Harbor-supported agents.

How it works
------------
Stella is a Rust binary distributed as a standalone executable. This adapter:
1. Uploads the compiled binary to the container
2. Installs it as `/usr/local/bin/stella`
3. Configures any required environment variables (API keys, model selection)
4. Runs Stella one-shot in the task working directory
5. Captures logs and metadata for Harbor's results

Model selection
---------------
Harbor passes `-m <provider>/<model>` which maps to Stella's `--model` flag:
- `anthropic/claude-fable-5` → `stella --model anthropic/claude-fable-5 run ...`
- `zai/glm-5.2` → `stella --model zai/glm-5.2 run ...`
- etc.

Environment
-----------
- `STELLA_MODEL` — forwarded from Harbor's `-m` flag
- `STELLA_API_KEY` — provider API key (e.g., `ANTHROPIC_API_KEY`, `ZAI_API_KEY`)
- `STELLA_BUDGET` — per-task USD spend limit
- `STELLA_TIMEOUT` — per-task timeout in seconds (default 1800)
- `STELLA_BASE_URL` — API base URL override (required for Z.ai coding plans)
- Any `STELLA_*` var on the host is forwarded to the container

**Z.ai (GLM) users**: Set `STELLA_BASE_URL=https://api.z.ai/api/coding/paas/v4` for
coding plans. The endpoint must include `/coding/` — `https://api.z.ai/api/paas/v4`
will return HTTP 429 "insufficient balance".
"""

from __future__ import annotations

import os
import re
import shlex
import sys
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

# Installation paths
_BINARY_NAME = "stella"
_WRAPPER_PATH = "/usr/local/bin/stella"
_REMOTE_TMP = "/tmp/stella"
_AGENT_LOG_PATH = "/tmp/stella-run.txt"

# Environment variable prefixes
_ENV_PREFIX = "STELLA_"

# Provider credentials and provider-specific addressing vars that Stella's
# provider registry reads directly (stella-model adapters + the credential
# chain). One family per supported provider, plus the documented aliases —
# kept in sync with the README's provider table so every BYOK provider can
# authenticate inside the container.
_PROVIDER_ENV_VARS = (
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
    """Check if a string environment variable represents truth."""
    if not value:
        return False
    return value.strip().lower() in ("1", "true", "yes", "on")


def _locate_binary() -> Path:
    """Find the Stella binary.

    Order: explicit `STELLA_BINARY`, then `stella` on PATH, then
    `./target/release/stella` relative to the current working directory.
    """
    explicit = os.environ.get("STELLA_BINARY")
    if explicit:
        return Path(explicit)

    # Check PATH
    for path in os.environ.get("PATH", "").split(os.pathsep):
        bin_path = Path(path) / _BINARY_NAME
        if bin_path.is_file() and os.access(bin_path, os.X_OK):
            return bin_path

    # Check cargo build output
    cwd = Path.cwd()
    while cwd != cwd.parent:  # Stop at filesystem root
        cargo_release = cwd / "target" / "release" / _BINARY_NAME
        if cargo_release.is_file():
            return cargo_release
        cwd = cwd.parent

    raise FileNotFoundError(
        f"Cannot find {_BINARY_NAME} binary. "
        f"Build with `cargo build --release -p stella-cli` or set STELLA_BINARY."
    )


class StellaAgent(BaseInstalledAgent):
    """Run the Stella coding CLI as a Harbor installed agent."""

    @staticmethod
    def name() -> str:
        return "stella"

    def get_version_command(self) -> str | None:
        return "stella --version"

    def parse_version(self, stdout: str) -> str:
        """Parse Stella version from `stella --version` output."""
        # Expected format: "stella 0.1.0"
        for line in stdout.strip().splitlines():
            if line.strip():
                return line.strip()
        return stdout.strip()

    async def install(self, environment: BaseEnvironment) -> None:
        """Install Stella in the container.

        Uploads the compiled binary and makes it available as `stella` on PATH.
        """
        binary_path = _locate_binary()

        # Upload the binary
        await environment.upload_file(str(binary_path), _REMOTE_TMP)

        # Install as /usr/local/bin/stella
        await self.exec_as_root(
            environment,
            command=(
                f"cp {_REMOTE_TMP} {_WRAPPER_PATH} && "
                f"chmod +x {_WRAPPER_PATH} && "
                f"stella --version"
            ),
            timeout_sec=60,
        )

        # Install fast-search tools (rg, fd) if available on host
        await self._provision_search_tool(environment, "rg", _cached_binary("rg"))
        await self._provision_search_tool(environment, "fd", _cached_binary("fd"))

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Run Stella one-shot on the task.

        Stella is invoked as:
            stella --model <model> [--budget <usd>] run "<instruction>"

        The command runs in the container's default working directory (the task repo).
        """
        command = self._build_command(instruction)

        # Forward environment variables
        env = self._forwarded_env()

        # Run with output captured to a log file
        full_command = f"{command} 2>&1 | tee {_AGENT_LOG_PATH}"

        await self.exec_as_agent(
            environment,
            command=full_command,
            env=env,
        )

    def _build_command(self, instruction: str) -> str:
        """Build the Stella one-shot command."""
        model = os.environ.get("STELLA_MODEL", "anthropic/claude-fable-5")
        budget = os.environ.get("STELLA_BUDGET", "5.0")

        # Escape the instruction for shell
        quoted_instruction = shlex.quote(instruction)

        return (
            f"stella --model {shlex.quote(model)} "
            f"--budget {budget} "
            f"run {quoted_instruction}"
        )

    def _forwarded_env(self) -> dict[str, str]:
        """Collect host-side environment variables to forward into the container.

        Forwards all `STELLA_*` variables, plus the provider credential and
        addressing vars that Stella reads directly, so the agent can make LLM
        calls for any of the supported providers.
        """
        forwarded = {}

        # Forward all STELLA_* variables (STELLA_MODEL, STELLA_BUDGET,
        # STELLA_BASE_URL, STELLA_BINARY, STELLA_TIMEOUT, ...).
        for key in os.environ:
            if key.startswith(_ENV_PREFIX):
                forwarded[key] = os.environ[key]

        # Forward provider credentials and provider-specific addressing vars.
        # These mirror the env vars Stella's provider registry actually reads
        # (stella-cli/src/config.rs PROVIDERS + agent.rs build_provider): one
        # entry per supported provider, plus the documented aliases.
        for key in _PROVIDER_ENV_VARS:
            if key in os.environ:
                forwarded[key] = os.environ[key]

        # Forward model from Harbor's -m flag if not already set
        if "STELLA_MODEL" not in forwarded and "HARBOR_MODEL" in os.environ:
            forwarded["STELLA_MODEL"] = os.environ["HARBOR_MODEL"]

        return forwarded

    async def _provision_search_tool(
        self, environment: BaseEnvironment, name: str, host_binary: Path | None
    ) -> None:
        """Provision a fast-search tool (rg, fd, etc.) in the container.

        Best-effort: skipped if the host binary is missing or the container
        already has it.
        """
        if not host_binary or not host_binary.is_file():
            return

        # Check if container already has it
        try:
            probe = await environment.exec(
                command=f"which {name}",
                timeout_sec=10,
            )
            if probe.return_code == 0:
                return  # Already present
        except Exception:
            pass  # Probe failed, try uploading anyway

        # Upload to container
        remote_tmp = f"/tmp/{name}"
        await environment.upload_file(str(host_binary), remote_tmp)

        # Install as /usr/local/bin/{name}
        try:
            await self.exec_as_root(
                environment,
                command=f"cp {remote_tmp} /usr/local/bin/{name} && chmod +x /usr/local/bin/{name}",
                timeout_sec=30,
            )
        except Exception as exc:
            print(
                f"stella-adapter: failed to install {name}: {exc}",
                file=sys.stderr,
            )

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Parse Stella output for Harbor metadata.

        Extracts cost, tokens, steps, and timing information from the agent log.
        """
        log = self._find_agent_log()
        if log is None:
            return

        text = log.read_text(errors="replace")

        # Parse efficiency summary if present
        # Expected format: "815.53s total · 83086 tok · $0.2714"
        m = re.search(r"([\d.]+)s total\D+([\d,]+)\s*tok\D+\$([\d.]+)", text)
        if not m:
            return

        wall_sec = float(m.group(1))
        total_tokens = int(m.group(2).replace(",", ""))
        context.cost_usd = float(m.group(3))

        steps_m = re.search(r"(\d+)\s*steps", text)
        context.metadata = {
            **(context.metadata or {}),
            "stella_total_tokens": total_tokens,
            "stella_wall_sec": wall_sec,
            "stella_steps": int(steps_m.group(1)) if steps_m else None,
        }

    def _find_agent_log(self) -> Path | None:
        """Find the Stella agent log file."""
        candidate = Path(self.logs_dir) / "stella-run.txt"
        if candidate.is_file():
            return candidate

        matches = sorted(Path(self.logs_dir).rglob("stella*.txt"))
        return matches[-1] if matches else None


def _cached_binary(name: str) -> Path | None:
    """Find a binary on the host system."""
    for path in os.environ.get("PATH", "").split(os.pathsep):
        bin_path = Path(path) / name
        if bin_path.is_file() and os.access(bin_path, os.X_OK):
            return bin_path
    return None
