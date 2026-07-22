"""Unit tests for the Stella Harbor adapter.

These import the adapter package, which subclasses Harbor's
``BaseInstalledAgent`` — so the whole module is skipped when Harbor is not
installed (e.g. a minimal CI job). Install dev deps with
``pip install -e '.[dev]'`` from ``bench/harbor_adapter`` to run them.

Instance methods are exercised on an ``__new__``-constructed agent with only
the attributes they read set explicitly, so the tests never depend on Harbor's
base ``__init__`` requirements.
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from types import SimpleNamespace

import pytest

pytest.importorskip("harbor", reason="Harbor is required to import the adapter")

from harbor.agents.installed.base import NonZeroAgentExitCodeError  # noqa: E402
from harbor.models.agent.context import AgentContext  # noqa: E402
from harbor.models.trajectories import Trajectory  # noqa: E402

import stella_harbor as adapter_module  # noqa: E402
from stella_harbor import (  # noqa: E402 - after importorskip by design
    _ADAPTER_VERSION,
    _ENGINE_CONFIG_ENV,
    _ENGINE_POSTURE_VERSION,
    _INSTALL_PATH,
    HOST_CREDENTIAL_BUNDLE_FD_ENV,
    StellaAgent,
    _benchmark_engine_posture,
    _cached_binary,
    _extract_metrics,
    _is_truthy,
    _load_json_object,
    _secure_exec_with_credential_fd,
    _sha256_file,
    _source_tree_sha256,
    _stream_to_envelope,
    _sum_step_usage,
    _validated_public_base_url,
    _verify_compose_containers_exclude_credential,
)
from stella_harbor.atif import envelope_accounting, envelope_to_trajectory  # noqa: E402
from stella_harbor.credential_bundle import (  # noqa: E402
    HOST_CREDENTIAL_SOURCE,
    create_anonymous_credential_bundle,
)


def _bare_agent() -> StellaAgent:
    """A StellaAgent instance bypassing the Harbor base ``__init__``.

    Only the attributes a given method reads are set by the test.
    """
    return StellaAgent.__new__(StellaAgent)


def _trajectory_envelope() -> dict[str, object]:
    """A realistic two-call Stella event envelope for ATIF tests."""
    return {
        "status": "completed",
        "text": "Done.",
        "cost_usd": 0.31,
        "model": "openrouter/anthropic/claude-sonnet-5",
        "task_class": "CodeChange",
        "events": [
            {"type": "stage", "name": "execute"},
            {"type": "reasoning", "delta": "Inspect "},
            {"type": "reasoning", "delta": "the file."},
            {
                "type": "step_usage",
                "step": 0,
                "model": "openrouter/anthropic/claude-sonnet-5",
                "input_tokens": 1000,
                "output_tokens": 200,
                "cached_input_tokens": 400,
                "cache_write_tokens": 25,
                "estimated_input_tokens": 950,
                "cost_usd": 0.2,
                "duration_ms": 1250,
                "retries": 1,
                "tool_calls": 1,
            },
            # Preview must not be concatenated with authoritative text.
            {"type": "text_delta", "text": "I will inspect"},
            {"type": "text", "delta": "Inspecting now."},
            {
                "type": "tool_start",
                "call": {
                    "call_id": "call-1",
                    "name": "read_file",
                    "input": {"path": "src/main.rs"},
                },
            },
            {
                "type": "tool_result",
                "call_id": "call-1",
                "output": {"ok": {"content": "fn main() {}"}},
                "duration_ms": 8,
                "speculated": True,
            },
            {"type": "reasoning", "delta": "Apply the fix."},
            {
                "type": "step_usage",
                "step": 1,
                "model": "openrouter/anthropic/claude-sonnet-5",
                "input_tokens": 500,
                "output_tokens": 80,
                "cached_input_tokens": 100,
                "cache_write_tokens": 10,
                "estimated_input_tokens": 525,
                "cost_usd": 0.11,
                "duration_ms": 750,
                "retries": 0,
                "tool_calls": 0,
            },
            {"type": "text", "delta": "Done."},
            {
                "type": "complete",
                "model": "openrouter/anthropic/claude-sonnet-5",
                "cost_usd": 0.31,
            },
        ],
    }


def _stream_for(envelope: dict[str, object], *, diagnostics: bool = False) -> str:
    """Serialize an envelope's events as Stella stream-json output."""
    lines = [json.dumps(event) for event in envelope["events"]]
    if diagnostics:
        lines.insert(0, "models: discovering models...")
        lines.insert(2, 'diagnostic payload: {"handle":"proc-5"}')
    return "\n".join(lines) + "\n"


class TestUtilityFunctions:
    def test_is_truthy(self) -> None:
        assert _is_truthy("1")
        assert _is_truthy("true")
        assert _is_truthy("TRUE")
        assert _is_truthy("yes")
        assert _is_truthy("on")
        assert not _is_truthy("0")
        assert not _is_truthy("false")
        assert not _is_truthy("")
        assert not _is_truthy(None)

    def test_cached_binary(self) -> None:
        # `ls` exists on any POSIX CI host.
        found = _cached_binary("ls")
        if found is not None:
            assert found.is_file()
            assert os.access(found, os.X_OK)
        assert _cached_binary("nonexistent-binary-xyz123") is None

    def test_sha256_file(self, tmp_path: Path) -> None:
        binary = tmp_path / "stella"
        binary.write_bytes(b"portable-stella-build")
        assert (
            _sha256_file(binary)
            == "18286b6d4e01ec03f401971f1eacecf2c5fc9f24fac92d2e76c4e7cfb6debc96"
        )

    def test_base_url_rejects_secret_bearing_components(self) -> None:
        assert _validated_public_base_url("https://openrouter.ai/api/v1") == (
            "https://openrouter.ai/api/v1"
        )
        for value in (
            "https://user:secret@openrouter.ai/api/v1",
            "https://openrouter.ai/api/v1?api_key=secret",
            "https://openrouter.ai/api/v1#secret",
        ):
            with pytest.raises(ValueError, match="must not contain"):
                _validated_public_base_url(value)

    def test_load_json_object_ignores_trailing_braced_diagnostic(self) -> None:
        envelope = _trajectory_envelope()
        mixed = (
            "models: discovering models...\n"
            + json.dumps(envelope, indent=2)
            + '\nstella: repeated input: {"handle":"proc-5"}\n'
        )
        assert _load_json_object(mixed) == envelope

    def test_load_json_object_prefers_envelope_over_leading_json_fragment(self) -> None:
        envelope = _trajectory_envelope()
        mixed = '{"startup":true}\n' + json.dumps(envelope) + "\ndone"
        assert _load_json_object(mixed) == envelope

    def test_stream_parser_synthesizes_complete_envelope_amid_diagnostics(
        self,
    ) -> None:
        source = _trajectory_envelope()
        envelope = _stream_to_envelope(
            _stream_for(source, diagnostics=True),
            process_returned=True,
        )
        assert envelope is not None
        assert envelope["status"] == "completed"
        assert envelope["text"] == "Done."
        assert envelope["model"] == "openrouter/anthropic/claude-sonnet-5"
        assert envelope["cost_usd"] == 0.31
        assert envelope["events"] == source["events"]
        assert envelope["_stella_stream"]["diagnostic_lines"] == 2
        assert envelope["_stella_stream"]["ignored_json_objects"] == 1
        assert envelope["_stella_stream"]["stream_complete"] is True

    def test_stream_parser_marks_cancelled_partial_stream_incomplete(self) -> None:
        events = _trajectory_envelope()["events"][:-1]
        partial = "\n".join(json.dumps(event) for event in events)
        partial += '\n{"type":"tool_start","call":'
        envelope = _stream_to_envelope(partial)
        assert envelope is not None
        assert envelope["status"] == "interrupted"
        assert envelope["cost_usd"] == pytest.approx(0.31)
        assert envelope["_stella_stream"]["stream_complete"] is False
        assert envelope["_stella_stream"]["diagnostic_lines"] == 1

    def test_stream_parser_preserves_normal_error_terminal(self) -> None:
        events = [
            {
                "type": "step_usage",
                "model": "provider/model",
                "input_tokens": 12,
                "output_tokens": 3,
                "cached_input_tokens": 0,
                "cost_usd": 0.02,
            },
            {"type": "error", "message": "budget exceeded", "retryable": False},
        ]
        envelope = _stream_to_envelope(
            "\n".join(json.dumps(event) for event in events),
            process_returned=True,
        )
        assert envelope is not None
        assert envelope["status"] == "aborted"
        assert envelope["reason"] == "budget exceeded"
        assert envelope["model"] == "provider/model"
        assert envelope["cost_usd"] == 0.02
        assert envelope["events"] == events
        assert envelope["_stella_stream"]["terminal_event"] == "error"
        assert envelope["_stella_stream"]["stream_complete"] is True


class TestVersion:
    def test_name(self) -> None:
        assert StellaAgent.name() == "stella"

    def test_get_version_command(self) -> None:
        assert _bare_agent().get_version_command() == f"{_INSTALL_PATH} --version"

    def test_parse_version(self) -> None:
        agent = _bare_agent()
        assert agent.parse_version("stella 0.3.0") == "stella 0.3.0"
        assert agent.parse_version("stella 0.3.0\nextra") == "stella 0.3.0"
        assert agent.parse_version("  stella 0.3.0  ") == "stella 0.3.0"

    def test_reported_version_includes_full_uploaded_binary_hash(self) -> None:
        agent = _bare_agent()
        agent._version = "stella 0.4.47"
        agent._binary_sha256 = "a" * 64
        assert agent.version() == f"stella 0.4.47 [binary-sha256:{'a' * 64}]"


class TestBuildCommand:
    def test_uses_harbor_model_name(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("STELLA_MODEL", raising=False)
        monkeypatch.delenv("STELLA_BUDGET", raising=False)
        monkeypatch.delenv("STELLA_BASE_URL", raising=False)
        monkeypatch.delenv("STELLA_DISABLE_REFLECTION", raising=False)
        agent = _bare_agent()
        agent.model_name = "anthropic/claude-fable-5"  # Harbor's -m flows here

        cmd = agent._build_command("Fix the bug")

        # Global flags must precede the `run` subcommand.
        assert cmd.index("--model") < cmd.index("run")
        assert cmd.index("--budget") < cmd.index("run")
        assert cmd.index("--output-format") < cmd.index("run")
        assert cmd[:3] == [
            _INSTALL_PATH,
            "--model",
            "anthropic/claude-fable-5",
        ]
        assert cmd[cmd.index("--output-format") + 1] == "stream-json"
        assert cmd[cmd.index("--budget") + 1] == "5.0"  # default budget
        assert cmd[-2:] == ["run", "Fix the bug"]
        assert "--base-url" not in cmd  # not set

    def test_env_overrides_and_base_url(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("STELLA_MODEL", "zai/glm-5.2")
        monkeypatch.setenv("STELLA_BUDGET", "10.0")
        monkeypatch.setenv("STELLA_BASE_URL", "https://api.z.ai/api/coding/paas/v4")
        agent = _bare_agent()
        agent.model_name = None  # env should win when Harbor didn't set one

        cmd = agent._build_command("Add a feature")

        assert cmd[cmd.index("--model") + 1] == "zai/glm-5.2"
        assert cmd[cmd.index("--budget") + 1] == "10.0"
        assert cmd[cmd.index("--base-url") + 1] == (
            "https://api.z.ai/api/coding/paas/v4"
        )

    def test_openrouter_default_base_url_is_pinned_in_argv(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("STELLA_BASE_URL", raising=False)
        agent = _bare_agent()
        agent.model_name = "openrouter/anthropic/claude-sonnet-5"

        cmd = agent._build_command("Fix it")

        assert cmd[cmd.index("--base-url") + 1] == ("https://openrouter.ai/api/v1")

    @pytest.mark.parametrize("from_extra", [False, True])
    def test_openrouter_rejects_noncanonical_ambient_or_extra_base_url(
        self, monkeypatch: pytest.MonkeyPatch, from_extra: bool
    ) -> None:
        monkeypatch.delenv("STELLA_BASE_URL", raising=False)
        agent = _bare_agent()
        agent.model_name = "openrouter/deepseek/deepseek-v4-pro"
        if from_extra:
            agent._extra_env = {"STELLA_BASE_URL": "https://attacker.invalid/v1"}
        else:
            monkeypatch.setenv("STELLA_BASE_URL", "https://attacker.invalid/v1")

        with pytest.raises(RuntimeError, match="canonical provider endpoint"):
            agent._build_command("Fix it")

    def test_explicit_reflection_override_is_disclosed_in_environment_only(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("STELLA_DISABLE_REFLECTION", "false")
        agent = _bare_agent()
        agent.model_name = "provider/model"
        assert agent._build_command("Fix it")[0] == _INSTALL_PATH
        assert agent._forwarded_env()["STELLA_DISABLE_REFLECTION"] == "false"

    def test_instruction_is_one_literal_argv_element(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("STELLA_MODEL", raising=False)
        agent = _bare_agent()
        agent.model_name = "anthropic/claude-fable-5"
        instruction = "rm -rf / ; echo $HOME && $(touch /tmp/owned)"
        cmd = agent._build_command(instruction)
        assert cmd[-2:] == ["run", instruction]

    def test_secret_bearing_base_url_never_enters_command(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(
            "STELLA_BASE_URL",
            "https://openrouter.ai/api/v1?api_key=do-not-expose",
        )
        agent = _bare_agent()
        agent.model_name = "openrouter/provider/model"
        with pytest.raises(ValueError, match="must not contain"):
            agent._build_command("Fix it")


class TestForwardedEnv:
    def test_canonical_engine_posture_has_one_inherited_model_and_fixed_effort(
        self,
    ) -> None:
        model = "openrouter/deepseek/deepseek-v4-pro"
        posture, normalized, digest = _benchmark_engine_posture(model)

        assert posture["default_model"] == model
        assert posture["allowed_models"] == [model]
        assert posture["auto_mode"] == "off"
        assert posture["effort_auto"] == "off"
        assert posture["reasoning_auto"] == "off"
        assert all(
            "model" not in role and "provider" not in role
            for role in posture["agents"].values()
        )
        for role in ("default", "worker", "judge"):
            assert posture["agents"][role] == {
                "effort": "high",
                "reasoning": "on",
            }
        assert posture["agents"]["triage"] == {
            "effort": "low",
            "reasoning": "off",
        }
        assert json.loads(normalized) == posture
        assert digest == (
            "fb18233aadf78077bc70fe52cdb1dcacc1f840600473a92226a88e932a138fd6"
        )

    def test_excludes_all_provider_keys_and_selects_only_effective_provider(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for key in list(os.environ):
            if key.startswith("STELLA_") or key.endswith("_API_KEY"):
                monkeypatch.delenv(key, raising=False)

        # Set provider keys FIRST, then an unrelated var, guarding against a
        # "forward the last loop binding" bug.
        monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-anthropic-real")
        monkeypatch.setenv("OPENAI_API_KEY", "sk-openai-real")
        monkeypatch.setenv("STELLA_MODEL", "anthropic/claude-fable-5")
        monkeypatch.setenv("STELLA_BUDGET", "5.0")
        monkeypatch.setenv("_ADAPTER_TEST_SENTINEL", "not-a-key")

        env = _bare_agent()._forwarded_env()

        assert "STELLA_MODEL" not in env
        assert env["STELLA_BUDGET"] == "5.0"
        assert "ANTHROPIC_API_KEY" not in env
        assert "OPENAI_API_KEY" not in env
        assert "_ADAPTER_TEST_SENTINEL" not in env

        agent = _bare_agent()
        agent.model_name = "anthropic/claude-fable-5"
        assert agent._selected_provider_credential() == (
            "ANTHROPIC_API_KEY",
            "sk-anthropic-real",
        )

    def test_selects_one_key_from_host_bundle_without_ambient_copy(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        secret = "test-openrouter-secret"
        monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)
        handle = create_anonymous_credential_bundle({"OPENROUTER_API_KEY": secret})
        try:
            monkeypatch.setenv(
                HOST_CREDENTIAL_BUNDLE_FD_ENV,
                str(handle.fileno()),
            )
            agent = _bare_agent()
            agent.model_name = "openrouter/deepseek/deepseek-v4-pro"

            assert agent._selected_provider_credential() == (
                "OPENROUTER_API_KEY",
                secret,
            )
            assert agent._host_credential_source == HOST_CREDENTIAL_SOURCE
            assert agent._host_credential_name == "OPENROUTER_API_KEY"
        finally:
            handle.close()

    def test_rejects_unregistered_harbor_extra_env(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for key in list(os.environ):
            if key.startswith("STELLA_"):
                monkeypatch.delenv(key, raising=False)
        agent = _bare_agent()
        agent._resolved_env_vars = {"MAX_THINKING_TOKENS": "2048"}
        with pytest.raises(RuntimeError, match="unregistered Harbor agent extras"):
            agent._forwarded_env()

    def test_merges_harbor_private_extra_env(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for key in list(os.environ):
            if key.startswith("STELLA_"):
                monkeypatch.delenv(key, raising=False)
        agent = _bare_agent()
        agent._extra_env = {
            "GITHUB_TOKEN": "must-not-cross",
            "OPENROUTER_API_KEY": "must-use-fd",
            _ENGINE_CONFIG_ENV: json.dumps(
                {"default_model": "anthropic/task-controlled"}
            ),
            HOST_CREDENTIAL_BUNDLE_FD_ENV: "123",
        }
        env = agent._forwarded_env()
        assert "GITHUB_TOKEN" not in env
        assert "OPENROUTER_API_KEY" not in env
        assert _ENGINE_CONFIG_ENV not in env
        assert HOST_CREDENTIAL_BUNDLE_FD_ENV not in env

    def test_rejects_unregistered_ambient_stella_knob_and_plain_extra(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for key in list(os.environ):
            if key.startswith("STELLA_"):
                monkeypatch.delenv(key, raising=False)
        monkeypatch.setenv("STELLA_LEAN_TOOLS", "1")
        with pytest.raises(RuntimeError, match=r"unregistered STELLA_\* knobs"):
            _bare_agent()._forwarded_env()

        monkeypatch.delenv("STELLA_LEAN_TOOLS")
        agent = _bare_agent()
        agent._extra_env = {"HOST_METADATA": "must-not-cross"}
        with pytest.raises(RuntimeError, match="unregistered Harbor agent extras"):
            agent._forwarded_env()

    def test_explicit_reflection_override_wins(self) -> None:
        agent = _bare_agent()
        agent._extra_env = {"STELLA_DISABLE_REFLECTION": "0"}
        assert agent._forwarded_env()["STELLA_DISABLE_REFLECTION"] == "0"

    def test_task_extras_cannot_enable_dotenv_project_trust_hooks_catalog_or_proxies(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Model a hostile task/Harbor extra attempting to make Stella load a
        # repository .env/settings/hooks or route the paid request via a proxy.
        for name in (
            "STELLA_NO_ENV_FILE",
            "STELLA_NO_SETTINGS",
            "STELLA_TRUST_PROJECT",
            "STELLA_PROJECT_HOOKS",
            "STELLA_CATALOG_AUTO_REFRESH",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ):
            monkeypatch.setenv(name, "ambient-attacker-value")
        agent = _bare_agent()
        agent._extra_env = {
            "STELLA_NO_ENV_FILE": "0",
            "STELLA_NO_SETTINGS": "0",
            "STELLA_TRUST_PROJECT": "1",
            "STELLA_PROJECT_HOOKS": "true",
            "STELLA_CATALOG_AUTO_REFRESH": "1",
            "HTTP_PROXY": "http://task-proxy.invalid:8080",
            "HTTPS_PROXY": "http://task-proxy.invalid:8080",
            "ALL_PROXY": "socks5://task-proxy.invalid:1080",
            "http_proxy": "http://task-proxy.invalid:8080",
            "https_proxy": "http://task-proxy.invalid:8080",
            "all_proxy": "socks5://task-proxy.invalid:1080",
            "NO_PROXY": "openrouter.ai",
            "no_proxy": "openrouter.ai",
        }

        env = agent._forwarded_env()

        assert env["STELLA_NO_ENV_FILE"] == "1"
        assert env["STELLA_NO_SETTINGS"] == "1"
        assert env["STELLA_TRUST_PROJECT"] == "0"
        assert env["STELLA_PROJECT_HOOKS"] == "0"
        # Stella's parser disables implicit catalog fetching only for the
        # exact string "0" (`model_catalog::maybe_auto_refresh`).
        assert env["STELLA_CATALOG_AUTO_REFRESH"] == "0"
        for name in (
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ):
            assert env[name] == ""
        assert env["NO_PROXY"] == "*"
        assert env["no_proxy"] == "*"


class TestSecureCredentialExec:
    def test_secret_is_stdin_only_not_docker_argv_or_environment(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        secret = "openrouter-super-secret-witness"
        observed: dict[str, object] = {}

        class _ComposeEnv:
            def to_env_dict(self, *, include_os_env: bool) -> dict[str, str]:
                assert include_os_env is True
                return {
                    "DOCKER_HOST": "unix:///safe/docker.sock",
                    "OPENROUTER_API_KEY": secret,
                }

        class _Process:
            returncode = 0

            async def communicate(self, input: bytes):
                observed["stdin"] = bytes(input)
                return b"one event\n", None

            def terminate(self) -> None:  # pragma: no cover - success path
                raise AssertionError("unexpected terminate")

        async def _spawn(*args: str, **kwargs: object) -> _Process:
            observed["argv"] = args
            observed["host_env"] = kwargs["env"]
            return _Process()

        monkeypatch.setattr(asyncio, "create_subprocess_exec", _spawn)
        environment = SimpleNamespace(
            session_id="trial/security",
            environment_dir=tmp_path,
            _docker_compose_paths=[],
            _env_vars=_ComposeEnv(),
            _compose_task_env={"GITHUB_TOKEN": "repo-secret"},
            _persistent_env={},
            task_env_config=SimpleNamespace(workdir="/workspace"),
            _resolve_user=lambda _user: "agent",
        )

        result = asyncio.run(
            _secure_exec_with_credential_fd(
                environment,
                command=[
                    _INSTALL_PATH,
                    "--model",
                    "openrouter/model",
                    "run",
                    "task ; echo $HOME",
                ],
                env={
                    "STELLA_CREDENTIAL_HANDOFF_FD": "0",
                    "STELLA_CREDENTIAL_HANDOFF_TARGET": "OPENROUTER_API_KEY",
                    "STELLA_TRUST_PROJECT": "1",
                    "HTTPS_PROXY": "http://task-proxy.invalid:8080",
                },
                credential=secret,
            )
        )

        argv = "\0".join(observed["argv"])
        assert secret not in argv
        assert "OPENROUTER_API_KEY=" not in argv
        assert secret not in observed["host_env"].values()
        assert "GITHUB_TOKEN" not in observed["host_env"]
        assert observed["stdin"] == f"{secret}\n".encode()
        assert result.return_code == 0
        argv_items = observed["argv"]
        main_index = argv_items.index("main")
        assert argv_items[main_index:] == (
            "main",
            _INSTALL_PATH,
            "--model",
            "openrouter/model",
            "run",
            "task ; echo $HOME",
        )
        assert "bash" not in argv_items
        assert "-c" not in argv_items
        assert "STELLA_TRUST_PROJECT=0" in argv_items
        assert "STELLA_NO_ENV_FILE=1" in argv_items
        assert "STELLA_NO_SETTINGS=1" in argv_items
        assert "STELLA_PROJECT_HOOKS=0" in argv_items
        assert "STELLA_CATALOG_AUTO_REFRESH=0" in argv_items
        assert "HTTPS_PROXY=" in argv_items
        assert "NO_PROXY=*" in argv_items
        assert observed["host_env"]["HTTPS_PROXY"] == ""
        assert observed["host_env"]["NO_PROXY"] == "*"

    def test_rejects_non_stella_launcher_argv(self) -> None:
        with pytest.raises(RuntimeError, match="direct stella argv"):
            asyncio.run(
                _secure_exec_with_credential_fd(
                    SimpleNamespace(),
                    command=["bash", "-c", "stella run task"],
                    env={},
                    credential="test-secret",
                )
            )


class TestContainerCredentialPreflight:
    @staticmethod
    def _environment(tmp_path: Path) -> SimpleNamespace:
        class _ComposeEnv:
            def to_env_dict(self, *, include_os_env: bool) -> dict[str, str]:
                assert include_os_env is True
                return {"DOCKER_HOST": "unix:///safe/docker.sock"}

        return SimpleNamespace(
            session_id="trial/security",
            environment_dir=tmp_path,
            _docker_compose_paths=[tmp_path / "compose.yaml"],
            _env_vars=_ComposeEnv(),
            _compose_task_env={},
            _persistent_env={},
            task_env_config=SimpleNamespace(workdir="/workspace"),
        )

    def test_enumerates_every_compose_container_and_scans_complete_config(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        credential = "test-openrouter-secret"
        container_ids = ["a" * 12, "b" * 64]
        calls: list[list[str]] = []

        async def fake_captured(
            argv: list[str], env: dict[str, str]
        ) -> tuple[int, bytes]:
            calls.append(argv)
            assert credential not in env.values()
            if argv[-2:] == ["ps", "-aq"]:
                return 0, ("\n".join(container_ids) + "\n").encode()
            assert argv == ["docker", "inspect", *container_ids]
            return 0, json.dumps(
                [
                    {
                        "Config": {
                            "Env": ["SAFE=value"],
                            "Cmd": ["sleep", "infinity"],
                            "Entrypoint": ["/bin/sh"],
                            "Labels": {"com.docker.compose.service": "main"},
                        }
                    },
                    {
                        "Config": {
                            "Env": [],
                            "Cmd": ["sidecar"],
                            "Entrypoint": None,
                            "Labels": {"com.docker.compose.service": "database"},
                        }
                    },
                ]
            ).encode()

        monkeypatch.setattr(adapter_module, "_captured_process", fake_captured)

        asyncio.run(
            _verify_compose_containers_exclude_credential(
                self._environment(tmp_path), credential
            )
        )

        assert calls[0][-2:] == ["ps", "-aq"]
        assert calls[1] == ["docker", "inspect", *container_ids]

    def test_rejects_exact_key_anywhere_in_config_without_rendering_it(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        credential = "test-openrouter-secret"
        container_id = "c" * 12

        async def fake_captured(
            argv: list[str], _env: dict[str, str]
        ) -> tuple[int, bytes]:
            if argv[-2:] == ["ps", "-aq"]:
                return 0, f"{container_id}\n".encode()
            return 0, json.dumps(
                [
                    {
                        "Config": {
                            "Env": [],
                            "Labels": {
                                "com.docker.compose.service": "main",
                                "copied": f"prefix:{credential}",
                            },
                        }
                    }
                ]
            ).encode()

        monkeypatch.setattr(adapter_module, "_captured_process", fake_captured)

        with pytest.raises(RuntimeError) as caught:
            asyncio.run(
                _verify_compose_containers_exclude_credential(
                    self._environment(tmp_path), credential
                )
            )
        assert credential not in str(caught.value)

    @pytest.mark.parametrize(
        "inherited",
        [
            "STELLA_BASH_SANDBOX=0",
            "OPENROUTER_API_KEY=decoy",
            "OPENAI_BASE_URL=https://attacker.invalid/v1",
            "BASH_ENV=/workspace/hostile.sh",
            "LD_PRELOAD=/workspace/hostile.so",
            "LD_LIBRARY_PATH=/workspace/hostile-libs",
            "LD_AUDIT=/workspace/hostile-audit.so",
        ],
    )
    def test_rejects_unregistered_main_container_control_environment(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
        inherited: str,
    ) -> None:
        credential = "test-openrouter-secret"
        container_id = "d" * 12

        async def fake_captured(
            argv: list[str], _env: dict[str, str]
        ) -> tuple[int, bytes]:
            if argv[-2:] == ["ps", "-aq"]:
                return 0, f"{container_id}\n".encode()
            return 0, json.dumps(
                [
                    {
                        "Config": {
                            "Env": [inherited],
                            "Labels": {"com.docker.compose.service": "main"},
                        }
                    }
                ]
            ).encode()

        monkeypatch.setattr(adapter_module, "_captured_process", fake_captured)

        with pytest.raises(RuntimeError, match="forbidden inherited environment"):
            asyncio.run(
                _verify_compose_containers_exclude_credential(
                    self._environment(tmp_path), credential
                )
            )

    def test_allows_task_application_secrets_and_sidecar_environment(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        credential = "test-openrouter-secret"
        container_ids = ["e" * 12, "f" * 12]

        async def fake_captured(
            argv: list[str], _env: dict[str, str]
        ) -> tuple[int, bytes]:
            if argv[-2:] == ["ps", "-aq"]:
                return 0, ("\n".join(container_ids) + "\n").encode()
            return 0, json.dumps(
                [
                    {
                        "Config": {
                            "Env": [
                                "DATABASE_PASSWORD=fixture",
                                "TEST_API_KEY=fixture",
                            ],
                            "Labels": {"com.docker.compose.service": "main"},
                        }
                    },
                    {
                        "Config": {
                            "Env": [
                                "STELLA_SIDECAR_FIXTURE=allowed",
                                "POSTGRES_PASSWORD=fixture",
                            ],
                            "Labels": {"com.docker.compose.service": "database"},
                        }
                    },
                ]
            ).encode()

        monkeypatch.setattr(adapter_module, "_captured_process", fake_captured)
        asyncio.run(
            _verify_compose_containers_exclude_credential(
                self._environment(tmp_path), credential
            )
        )

    def test_requires_exactly_one_main_compose_service(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        credential = "test-openrouter-secret"
        container_id = "1" * 12

        async def fake_captured(
            argv: list[str], _env: dict[str, str]
        ) -> tuple[int, bytes]:
            if argv[-2:] == ["ps", "-aq"]:
                return 0, f"{container_id}\n".encode()
            return 0, json.dumps(
                [
                    {
                        "Config": {
                            "Env": [],
                            "Labels": {"com.docker.compose.service": "database"},
                        }
                    }
                ]
            ).encode()

        monkeypatch.setattr(adapter_module, "_captured_process", fake_captured)
        with pytest.raises(RuntimeError, match="exactly one main"):
            asyncio.run(
                _verify_compose_containers_exclude_credential(
                    self._environment(tmp_path), credential
                )
            )


class TestInstall:
    def test_hashes_exact_uploaded_binary_and_records_source_commit(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        binary = tmp_path / "stella"
        binary.write_bytes(b"exact-linux-build")
        binary.chmod(0o755)
        monkeypatch.setenv("STELLA_BINARY", str(binary))
        source_commit = "d" * 40
        monkeypatch.setenv("STELLA_SOURCE_COMMIT", source_commit)

        agent = _bare_agent()
        agent._extra_env = {}
        uploads: list[tuple[str, str]] = []
        root_commands: list[str] = []

        class _Environment:
            async def upload_file(self, source: str, destination: str) -> None:
                uploads.append((source, destination))

        async def _exec_as_root(
            environment: object, *, command: str, timeout_sec: int
        ) -> SimpleNamespace:
            root_commands.append(command)
            return SimpleNamespace(
                return_code=0,
                stdout=(
                    f"{_sha256_file(binary)}  /tmp/stella-upload\n"
                    f"stella 0.4.47-dev.{source_commit}"
                ),
            )

        agent.exec_as_root = _exec_as_root
        asyncio.run(agent.install(_Environment()))

        assert uploads == [(str(binary), "/tmp/stella-upload")]
        assert root_commands and "/usr/local/bin/stella --version" in root_commands[0]
        assert agent._binary_sha256 == _sha256_file(binary)
        assert agent._binary_sha256_verified is True
        assert agent._source_commit == source_commit
        assert agent._source_commit_verified is True
        assert len(agent._adapter_sha256) == 64
        assert agent._harbor_version_value == "0.6.1"
        assert len(agent._harbor_sha256) == 64
        assert agent._binary_sha256 in agent.version()

    def test_source_tree_hash_changes_without_version_change(
        self, tmp_path: Path
    ) -> None:
        package = tmp_path / "adapter"
        package.mkdir()
        source = package / "agent.py"
        source.write_text('VERSION = "1.0"\nVALUE = 1\n')
        first = _source_tree_sha256(package, domain="test-adapter-v1")
        source.write_text('VERSION = "1.0"\nVALUE = 2\n')
        second = _source_tree_sha256(package, domain="test-adapter-v1")
        assert first != second

    def test_rejects_source_commit_env_that_differs_from_embedded_binary(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        binary = tmp_path / "stella"
        binary.write_bytes(b"source-mismatch-build")
        binary.chmod(0o755)
        monkeypatch.setenv("STELLA_BINARY", str(binary))
        monkeypatch.setenv("STELLA_SOURCE_COMMIT", "a" * 40)
        agent = _bare_agent()
        agent._extra_env = {}

        class _Environment:
            async def upload_file(self, source: str, destination: str) -> None:
                return None

        async def _exec_as_root(
            environment: object, *, command: str, timeout_sec: int
        ) -> SimpleNamespace:
            return SimpleNamespace(
                return_code=0,
                stdout=(
                    f"{_sha256_file(binary)}  /tmp/stella-upload\n"
                    f"stella 0.4.47-dev.{'b' * 40}"
                ),
            )

        agent.exec_as_root = _exec_as_root
        with pytest.raises(RuntimeError, match="does not match"):
            asyncio.run(agent.install(_Environment()))


class TestRun:
    def test_mounted_internal_stream_is_source_of_truth_and_is_not_overwritten(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("STELLA_BUDGET", "0.17")
        monkeypatch.setenv("OPENROUTER_API_KEY", "openrouter-test-secret")
        source_envelope = _trajectory_envelope()
        durable_stream = _stream_for(source_envelope)
        (tmp_path / "stella-events.jsonl").write_text(durable_stream)

        agent = _bare_agent()
        agent.logs_dir = tmp_path
        agent.model_name = "openrouter/provider/model"
        agent._extra_env = {
            _ENGINE_CONFIG_ENV: json.dumps(
                {
                    "default_model": "anthropic/task-controlled",
                    "agents": {
                        "worker": {
                            "provider": "anthropic",
                            "model": "task-controlled",
                            "effort": "low",
                            "reasoning": "off",
                        }
                    },
                }
            )
        }
        agent._version = "stella 0.4.47"

        class _Environment:
            async def _stella_secure_exec_with_stdin(
                self, *, command: list[str], env: dict[str, str], stdin: bytes
            ):
                assert command[0] == _INSTALL_PATH
                assert command[-2:] == ["run", "Fix the task."]
                assert command[command.index("--base-url") + 1] == (
                    "https://openrouter.ai/api/v1"
                )
                assert stdin == b"openrouter-test-secret\n"
                assert "OPENROUTER_API_KEY" not in env
                assert env["STELLA_CREDENTIAL_HANDOFF_FD"] == "0"
                assert env["STELLA_NO_ENV_FILE"] == "1"
                assert env["STELLA_NO_SETTINGS"] == "1"
                assert env["STELLA_TRUST_PROJECT"] == "0"
                assert env["STELLA_PROJECT_HOOKS"] == "0"
                assert env["STELLA_CATALOG_AUTO_REFRESH"] == "0"
                assert env["HTTPS_PROXY"] == ""
                assert env["NO_PROXY"] == "*"
                assert env["STELLA_DURABLE_STREAM_JSON_PATH"] == (
                    "/logs/agent/stella-events.jsonl"
                )
                posture, normalized, digest = _benchmark_engine_posture(
                    "openrouter/provider/model"
                )
                assert env[_ENGINE_CONFIG_ENV] == normalized
                assert json.loads(env[_ENGINE_CONFIG_ENV]) == posture
                assert digest == agent._engine_posture_sha256
                return SimpleNamespace(
                    stdout="captured output was shorter",
                    stderr=None,
                    return_code=0,
                )

        context = AgentContext()
        asyncio.run(
            StellaAgent.run.__wrapped__(
                agent,
                "Fix the task.",
                _Environment(),
                context,
            )
        )

        assert (tmp_path / "stella-events.jsonl").read_text() == durable_stream
        assert context.cost_usd == 0.31
        assert context.n_input_tokens == 1500

    def test_nonzero_exit_persists_stream_context_and_atif_before_raising(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("STELLA_BUDGET", "0.17")
        monkeypatch.setenv("OPENROUTER_API_KEY", "openrouter-test-secret")
        agent = _bare_agent()
        agent.logs_dir = tmp_path
        agent.model_name = "openrouter/deepseek/deepseek-v4-pro"
        agent._extra_env = {}
        agent._version = "stella 0.4.47"
        agent._binary_sha256 = "b" * 64
        agent._binary_sha256_verified = True
        agent._source_commit = "d9caba12359a"
        source_envelope = _trajectory_envelope()

        class _Environment:
            async def _stella_secure_exec_with_stdin(
                self, *, command: list[str], env: dict[str, str], stdin: bytes
            ):
                assert command[0] == _INSTALL_PATH
                assert "bash" not in command
                assert "tee" not in command
                assert command[command.index("--base-url") + 1] == (
                    "https://openrouter.ai/api/v1"
                )
                assert env["STELLA_BUDGET"] == "0.17"
                assert env["STELLA_DISABLE_REFLECTION"] == "1"
                assert env["STELLA_NO_ENV_FILE"] == "1"
                assert env["STELLA_NO_SETTINGS"] == "1"
                assert env["STELLA_TRUST_PROJECT"] == "0"
                assert env["STELLA_PROJECT_HOOKS"] == "0"
                assert env["STELLA_CATALOG_AUTO_REFRESH"] == "0"
                assert env["STELLA_CREDENTIAL_HANDOFF_FD"] == "0"
                assert env["STELLA_CREDENTIAL_HANDOFF_TARGET"] == ("OPENROUTER_API_KEY")
                assert "OPENROUTER_API_KEY" not in env
                assert b"openrouter-test-secret" in stdin
                return SimpleNamespace(
                    stdout=_stream_for(source_envelope, diagnostics=True),
                    stderr="recorded stderr",
                    return_code=7,
                )

        # Bypass Harbor's prompt-template decorator; this test targets the
        # adapter's process/result contract directly.
        context = AgentContext()
        with pytest.raises(NonZeroAgentExitCodeError, match="exited with code 7"):
            asyncio.run(
                StellaAgent.run.__wrapped__(
                    agent,
                    "Fix the task.",
                    _Environment(),
                    context,
                )
            )

        assert agent._return_code == 7
        assert agent._metrics["cost_usd"] == 0.31
        assert agent._envelope["events"] == source_envelope["events"]
        assert (tmp_path / "stella-events.jsonl").read_text().startswith("models:")
        assert (tmp_path / "stella-run.stdout.txt").read_text().startswith("models:")
        persisted = json.loads((tmp_path / "stella-run.json").read_text())
        assert persisted["status"] == "completed"
        assert persisted["events"] == source_envelope["events"]
        assert (tmp_path / "stella-run.stderr.txt").read_text() == "recorded stderr"
        trajectory = Trajectory.model_validate_json(
            (tmp_path / "trajectory.json").read_text()
        )
        assert "b" * 64 in trajectory.agent.version
        assert trajectory.agent.extra["binary_sha256"] == "b" * 64
        assert trajectory.agent.extra["binary_sha256_verified_in_container"] is True
        assert trajectory.agent.extra["source_commit"] == "d9caba12359a"
        assert trajectory.agent.extra["adapter_version"] == _ADAPTER_VERSION
        assert trajectory.agent.extra["budget_usd"] == "0.17"
        assert trajectory.agent.extra["credential_handoff"] == "anonymous-fd"
        assert trajectory.agent.extra["launcher_controls"] == {
            "process_invocation": "docker-compose-direct-argv",
            "repository_env_file": "disabled",
            "project_env_files": "disabled",
            "filesystem_settings": "disabled",
            "filesystem_credentials": "disabled",
            "subprocess_credential_scrub": "enabled",
            "project_trust": "disabled",
            "project_hooks": "disabled",
            "catalog_auto_refresh": "disabled",
            "provider_proxy": "disabled",
            "base_url_authority": "validated-cli-argument",
            "engine_config_authority": "trusted-launcher-json",
        }

        assert context.cost_usd == 0.31
        assert context.n_input_tokens == 1500
        assert context.n_output_tokens == 280
        assert context.metadata["stella_return_code"] == 7
        assert context.metadata["stella_binary_sha256"] == "b" * 64
        assert context.metadata["stella_binary_sha256_verified_in_container"] is True
        assert context.metadata["stella_source_commit"] == "d9caba12359a"
        assert context.metadata["stella_adapter_version"] == _ADAPTER_VERSION
        assert context.metadata["stella_budget_usd"] == "0.17"
        assert context.metadata["stella_disable_reflection"] == "1"
        assert context.metadata["stella_reflection_policy"] == (
            "disabled_for_ephemeral_benchmark"
        )
        assert context.metadata["stella_credential_handoff"] == "anonymous-fd"
        assert (
            context.metadata["stella_launcher_controls"]
            == (trajectory.agent.extra["launcher_controls"])
        )
        assert context.metadata["stella_base_url"] == "https://openrouter.ai/api/v1"
        assert context.metadata["stella_provider_route_policy"] == "openrouter-auto"
        posture, normalized, digest = _benchmark_engine_posture(
            "openrouter/deepseek/deepseek-v4-pro"
        )
        assert context.metadata["stella_engine_posture_version"] == (
            _ENGINE_POSTURE_VERSION
        )
        assert context.metadata["stella_engine_posture"] == posture
        assert context.metadata["stella_engine_posture_json"] == normalized
        assert context.metadata["stella_engine_posture_sha256"] == digest
        assert trajectory.agent.extra["engine_posture_version"] == (
            _ENGINE_POSTURE_VERSION
        )
        assert trajectory.agent.extra["engine_posture"] == posture
        assert trajectory.agent.extra["engine_posture_json"] == normalized
        assert trajectory.agent.extra["engine_posture_sha256"] == digest
        assert context.metadata["stella_accounting"]["cost_consistency"] == (
            "consistent"
        )

    def test_cancellation_fallback_recovers_partial_stream_without_invention(
        self, tmp_path: Path
    ) -> None:
        agent = _bare_agent()
        agent.logs_dir = tmp_path
        agent.model_name = "provider/model"
        agent._version = "stella 0.4.47"
        agent._binary_sha256 = "c" * 64
        agent._binary_sha256_verified = True
        agent._source_commit = None
        agent._instruction = "Finish the interrupted task."
        agent._session_id = "cancelled-session"
        agent._disable_reflection = "1"
        agent._budget_usd = "0.17"

        events = _trajectory_envelope()["events"][:-1]
        stream = "noise before events\n"
        stream += "\n".join(json.dumps(event) for event in events)
        stream += '\n{"type":"step_usage","input_tokens":999'
        (tmp_path / "stella-events.jsonl").write_text(stream)

        context = AgentContext()
        agent.populate_context_post_run(context)

        assert context.cost_usd == pytest.approx(0.31)
        assert context.n_input_tokens == 1500
        assert context.n_output_tokens == 280
        assert context.metadata["stella_status"] == "interrupted"
        assert context.metadata["stella_return_code_state"] == "unknown"
        accounting = context.metadata["stella_accounting"]
        assert accounting["state"] == "incomplete"
        assert accounting["step_usage_records"] == 2
        assert accounting["cost_consistency"] == "derived_from_step_usage"
        assert context.metadata["stella_stream"]["stream_complete"] is False

        strict = json.loads((tmp_path / "stella-run.json").read_text())
        assert len(strict["events"]) == len(events)
        assert strict["status"] == "interrupted"
        trajectory = Trajectory.model_validate_json(
            (tmp_path / "trajectory.json").read_text()
        )
        assert trajectory.extra["status"] == "interrupted"
        assert trajectory.final_metrics.total_prompt_tokens == 1500
        assert trajectory.final_metrics.extra["stella_accounting"]["state"] == (
            "incomplete"
        )


class TestMetricsExtraction:
    def _envelope(self) -> str:
        return json.dumps(
            {
                "status": "completed",
                "text": "done",
                "cost_usd": 0.2714,
                "reason": None,
                "model": "anthropic/claude-fable-5",
                "events": [
                    {
                        "type": "step_usage",
                        "step": 0,
                        "model": "anthropic/claude-fable-5",
                        "input_tokens": 1000,
                        "output_tokens": 200,
                        "cached_input_tokens": 400,
                        "cost_usd": 0.1,
                    },
                    {"type": "assistant_message", "text": "hi"},
                    {
                        "type": "step_usage",
                        "step": 1,
                        "model": "anthropic/claude-fable-5",
                        "input_tokens": 500,
                        "output_tokens": 80,
                        "cached_input_tokens": 100,
                        "cost_usd": 0.05,
                    },
                ],
            },
            indent=2,
        )

    def test_full_envelope(self) -> None:
        m = _extract_metrics(self._envelope())
        assert m["cost_usd"] == 0.2714
        assert m["status"] == "completed"
        assert m["model"] == "anthropic/claude-fable-5"
        assert m["n_input_tokens"] == 1500
        assert m["n_output_tokens"] == 280
        assert m["n_cache_tokens"] == 500
        assert m["steps"] == 2

    def test_empty_and_blank(self) -> None:
        for value in (None, "", "   "):
            m = _extract_metrics(value)
            assert m["cost_usd"] is None
            assert m["n_input_tokens"] is None
            assert m["steps"] is None

    def test_regex_fallback_on_truncated_json(self) -> None:
        # Truncated so json.loads fails, but the cost key is still greppable.
        truncated = '{"status": "completed", "cost_usd": 1.23, "events": [{"typ'
        m = _extract_metrics(truncated)
        assert m["cost_usd"] == 1.23
        assert m["n_input_tokens"] is None  # tokens unrecoverable from garbage

    def test_no_usage_events_leaves_tokens_none(self) -> None:
        env = json.dumps(
            {"status": "completed", "cost_usd": 0.0, "events": [{"type": "x"}]}
        )
        m = _extract_metrics(env)
        assert m["cost_usd"] == 0.0
        assert m["n_input_tokens"] is None  # not misreported as 0

    def test_missing_usage_field_keeps_aggregate_unknown(self) -> None:
        env = json.dumps(
            {
                "status": "interrupted",
                "events": [
                    {
                        "type": "step_usage",
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "cached_input_tokens": 0,
                    },
                    {
                        "type": "step_usage",
                        "output_tokens": 3,
                        "cached_input_tokens": 0,
                    },
                ],
            }
        )
        metrics = _extract_metrics(env)
        assert metrics["n_input_tokens"] is None
        assert metrics["n_output_tokens"] == 5
        assert metrics["n_cache_tokens"] == 0

    def test_accounting_detects_mismatch_and_missing_values(self) -> None:
        envelope = {
            "status": "completed",
            "cost_usd": 0.5,
            "events": [
                {
                    "type": "step_usage",
                    "model": "provider/frozen-model",
                    "input_tokens": 10,
                    "output_tokens": 2,
                    "cached_input_tokens": 0,
                    "cost_usd": 0.2,
                },
                {
                    "type": "step_usage",
                    "model": "provider/frozen-model",
                    "input_tokens": 20,
                    "output_tokens": 3,
                    "cached_input_tokens": 0,
                    "cost_usd": 0.1,
                },
            ],
        }
        accounting = envelope_accounting(envelope)
        assert accounting["state"] == "complete"
        assert accounting["step_usage_total_cost_usd"] == pytest.approx(0.3)
        assert accounting["cost_consistency"] == "mismatch"
        assert accounting["cost_difference_usd"] == pytest.approx(0.2)
        assert accounting["model_state"] == "complete"
        assert accounting["model_records"] == 2
        assert accounting["models"] == ["provider/frozen-model"]

        del envelope["events"][1]["input_tokens"]
        accounting = envelope_accounting(envelope)
        assert accounting["state"] == "incomplete"
        assert accounting["fields"]["input_tokens"] == {
            "state": "incomplete",
            "reported_records": 1,
            "total": 10,
        }

        del envelope["events"][1]["model"]
        accounting = envelope_accounting(envelope)
        assert accounting["model_state"] == "incomplete"
        assert accounting["model_records"] == 1

    def test_sum_step_usage_ignores_bad_shapes(self) -> None:
        totals = _sum_step_usage(
            [
                {"type": "step_usage", "input_tokens": 10, "output_tokens": 2},
                "not-a-dict",
                {"type": "other", "input_tokens": 999},
                {"type": "step_usage", "input_tokens": "bad", "output_tokens": 3},
            ]
        )
        assert totals == {"input": 10, "output": 5, "cache": 0}

    def test_load_json_object_slice(self) -> None:
        assert _load_json_object('noise\n{"a": 1}\ntrailer') == {"a": 1}
        assert _load_json_object("not json") is None
        assert _load_json_object("[1, 2, 3]") is None  # arrays are not objects


class TestPopulateContext:
    def test_populates_from_stashed_metrics(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("STELLA_BUDGET", raising=False)
        monkeypatch.delenv("STELLA_DISABLE_REFLECTION", raising=False)
        agent = _bare_agent()
        agent._metrics = {
            "cost_usd": 0.5,
            "n_input_tokens": 1500,
            "n_output_tokens": 280,
            "n_cache_tokens": 500,
            "status": "completed",
            "model": "anthropic/claude-fable-5",
            "steps": 2,
        }
        agent._return_code = 0

        class _Ctx:
            cost_usd = None
            n_input_tokens = None
            n_output_tokens = None
            n_cache_tokens = None
            metadata = None

        ctx = _Ctx()
        agent.populate_context_post_run(ctx)

        assert ctx.cost_usd == 0.5
        assert ctx.n_input_tokens == 1500
        assert ctx.n_output_tokens == 280
        assert ctx.n_cache_tokens == 500
        assert ctx.metadata["stella_status"] == "completed"
        assert ctx.metadata["stella_steps"] == 2
        assert ctx.metadata["stella_return_code"] == 0
        assert ctx.metadata["stella_adapter_version"] == "0.6.0"
        assert ctx.metadata["stella_budget_usd"] == "5.0"
        assert ctx.metadata["stella_disable_reflection"] == "1"
        assert ctx.metadata["stella_host_credential_source"] == ("environment-fallback")
        assert ctx.metadata["stella_host_credential_bundle_count"] == 0
        assert ctx.metadata["stella_container_credential_absence_verified"] is False


class TestAtifTrajectory:
    def test_management_outputs_and_purposes_are_public_audit_steps(self) -> None:
        trajectory = envelope_to_trajectory(
            {
                "status": "completed",
                "cost_usd": 0.06,
                "model": "provider/model",
                "events": [
                    {
                        "type": "step_usage",
                        "step": 0,
                        "purpose": "triage",
                        "output_text": "multi",
                        "model": "provider/model",
                        "input_tokens": 10,
                        "output_tokens": 1,
                        "cost_usd": 0.01,
                    },
                    {
                        "type": "step_usage",
                        "step": 0,
                        "purpose": "plan",
                        "output_text": '["inspect", "patch"]',
                        "model": "provider/model",
                        "input_tokens": 20,
                        "output_tokens": 2,
                        "cost_usd": 0.02,
                    },
                    {
                        "type": "step_usage",
                        "step": 0,
                        "purpose": "execute",
                        "model": "provider/model",
                        "input_tokens": 30,
                        "output_tokens": 3,
                        "cost_usd": 0.03,
                    },
                    {"type": "text", "delta": "Done."},
                ],
            },
            instruction="Fix it.",
            session_id="session-management",
            agent_version="stella 0.4.47",
            default_model=None,
            return_code=0,
        )

        validated = Trajectory.model_validate(trajectory.to_json_dict())
        assert [step.message for step in validated.steps[1:]] == [
            "multi",
            '["inspect", "patch"]',
            "Done.",
        ]
        assert [step.extra["stella_purpose"] for step in validated.steps[1:]] == [
            "triage",
            "plan",
            "execute",
        ]
        assert validated.final_metrics.total_prompt_tokens == 60
        assert validated.final_metrics.total_completion_tokens == 6
        assert validated.final_metrics.total_cost_usd == 0.06

    def test_full_envelope_is_valid_atif_v17(self) -> None:
        trajectory = envelope_to_trajectory(
            _trajectory_envelope(),
            instruction="Fix the bug without changing the public API.",
            session_id="session-123",
            agent_version="stella 0.4.43",
            default_model="fallback/model",
            return_code=0,
            host_credential_source=HOST_CREDENTIAL_SOURCE,
            host_credential_name="OPENROUTER_API_KEY",
            host_credential_bundle_count=1,
            container_credential_absence_verified=True,
        )

        # Round-trip through Harbor's validator, not just our constructors.
        exported = trajectory.to_json_dict()
        validated = Trajectory.model_validate(exported)
        assert validated.schema_version == "ATIF-v1.7"
        assert validated.session_id == "session-123"
        assert validated.agent.name == "stella"
        assert validated.agent.version == "stella 0.4.43"
        assert validated.agent.model_name == "openrouter/anthropic/claude-sonnet-5"
        assert validated.agent.extra["output_format"] == "stream-json"
        assert validated.agent.extra["host_credential_source"] == (
            HOST_CREDENTIAL_SOURCE
        )
        assert validated.agent.extra["host_credential_name"] == ("OPENROUTER_API_KEY")
        assert validated.agent.extra["host_credential_bundle_count"] == 1
        assert validated.agent.extra["container_credential_absence_verified"] is True
        assert [step.step_id for step in validated.steps] == [1, 2, 3]

        instruction = validated.steps[0]
        assert instruction.source == "user"
        assert instruction.message == "Fix the bug without changing the public API."

        first_call = validated.steps[1]
        assert first_call.source == "agent"
        assert first_call.llm_call_count == 1
        assert first_call.reasoning_content == "Inspect the file."
        assert first_call.message == "Inspecting now."
        assert "I will inspect" not in first_call.message
        assert first_call.metrics.prompt_tokens == 1000
        assert first_call.metrics.completion_tokens == 200
        assert first_call.metrics.cached_tokens == 400
        assert first_call.metrics.cost_usd == 0.2
        assert first_call.metrics.extra == {
            "cache_write_tokens": 25,
            "estimated_input_tokens": 950,
            "duration_ms": 1250,
            "retries": 1,
            "tool_calls": 1,
        }
        assert first_call.tool_calls[0].tool_call_id == "call-1"
        assert first_call.tool_calls[0].function_name == "read_file"
        assert first_call.tool_calls[0].arguments == {"path": "src/main.rs"}
        result = first_call.observation.results[0]
        assert result.source_call_id == "call-1"
        assert result.content == "fn main() {}"
        assert result.extra == {
            "status": "ok",
            "duration_ms": 8,
            "speculated": True,
        }

        second_call = validated.steps[2]
        assert second_call.reasoning_content == "Apply the fix."
        assert second_call.message == "Done."
        assert second_call.metrics.prompt_tokens == 500

        totals = validated.final_metrics
        assert totals.total_prompt_tokens == 1500
        assert totals.total_completion_tokens == 280
        assert totals.total_cached_tokens == 500
        assert totals.total_cost_usd == 0.31
        assert totals.total_steps == 3
        assert totals.extra["total_model_duration_ms"] == 2000
        assert totals.extra["total_tool_duration_ms"] == 8
        assert totals.extra["total_cache_write_tokens"] == 35
        assert totals.extra["stella_accounting"]["state"] == "complete"
        assert totals.extra["stella_accounting"]["cost_consistency"] == ("consistent")
        assert validated.extra["status"] == "completed"
        assert validated.extra["stella_return_code"] == 0

    def test_orphan_error_result_and_scalar_input_are_preserved(self) -> None:
        envelope = {
            "status": "completed",
            "model": "provider/model",
            "events": [
                {"type": "step_usage", "step": 0},
                {
                    "type": "tool_start",
                    "call": {
                        "call_id": "scalar",
                        "name": "shell",
                        "input": "pwd",
                    },
                },
                {
                    "type": "tool_result",
                    "call_id": "scalar",
                    "output": {"error": {"message": "denied"}},
                    "duration_ms": 2,
                    "speculated": False,
                },
                {
                    "type": "tool_result",
                    "call_id": "orphan",
                    "output": {"ok": {"content": "recovered"}},
                    "duration_ms": 3,
                },
            ],
        }

        trajectory = envelope_to_trajectory(
            envelope,
            instruction="Run the command.",
            session_id="session-orphan",
            agent_version="unknown",
            default_model=None,
            return_code=0,
        )
        step = trajectory.steps[1]
        assert [call.tool_call_id for call in step.tool_calls] == [
            "scalar",
            "orphan",
        ]
        assert step.tool_calls[0].arguments == {"value": "pwd"}
        assert step.tool_calls[0].extra == {"stella_raw_input_type": "str"}
        assert step.tool_calls[1].extra == {"synthetic_from_orphan_result": True}
        assert [result.content for result in step.observation.results] == [
            "denied",
            "recovered",
        ]
        assert [result.extra["status"] for result in step.observation.results] == [
            "error",
            "ok",
        ]
        Trajectory.model_validate(trajectory.to_json_dict())

    def test_missing_usage_keeps_authoritative_output(self) -> None:
        trajectory = envelope_to_trajectory(
            {
                "status": "completed",
                "model": "provider/model",
                "events": [
                    {"type": "text_delta", "text": "preview"},
                    {"type": "text", "delta": "authoritative"},
                ],
            },
            instruction="Answer.",
            session_id="session-unmetered",
            agent_version="unknown",
            default_model=None,
            return_code=None,
        )
        assert len(trajectory.steps) == 2
        assert trajectory.steps[1].message == "authoritative"
        assert trajectory.steps[1].extra["usage_missing"] is True
        assert trajectory.final_metrics.total_prompt_tokens is None
        Trajectory.model_validate(trajectory.to_json_dict())

    def test_populate_context_writes_public_trajectory(self, tmp_path: Path) -> None:
        agent = _bare_agent()
        agent.logs_dir = tmp_path
        agent.model_name = "openrouter/anthropic/claude-sonnet-5"
        agent._version = "stella 0.4.43"
        agent._instruction = "Fix the benchmark task."
        agent._session_id = "session-file"
        agent._return_code = 0
        agent._envelope = _trajectory_envelope()
        agent._metrics = _extract_metrics(json.dumps(agent._envelope))
        agent._host_credential_source = HOST_CREDENTIAL_SOURCE
        agent._host_credential_name = "OPENROUTER_API_KEY"
        agent._container_credential_absence_verified = True

        class _Ctx:
            cost_usd = None
            n_input_tokens = None
            n_output_tokens = None
            n_cache_tokens = None
            metadata = None

        ctx = _Ctx()
        agent.populate_context_post_run(ctx)

        path = tmp_path / "trajectory.json"
        assert path.is_file()
        validated = Trajectory.model_validate_json(path.read_text(encoding="utf-8"))
        assert validated.schema_version == "ATIF-v1.7"
        assert validated.steps[0].message == "Fix the benchmark task."
        assert ctx.cost_usd == 0.31
        assert ctx.n_input_tokens == 1500
        assert ctx.n_output_tokens == 280
        assert ctx.n_cache_tokens == 500
        assert ctx.metadata["stella_host_credential_source"] == (HOST_CREDENTIAL_SOURCE)
        assert ctx.metadata["stella_host_credential_name"] == ("OPENROUTER_API_KEY")
        assert ctx.metadata["stella_host_credential_bundle_count"] == 1
        assert ctx.metadata["stella_container_credential_absence_verified"] is True
        assert validated.agent.extra["host_credential_source"] == (
            HOST_CREDENTIAL_SOURCE
        )
        assert validated.agent.extra["host_credential_name"] == ("OPENROUTER_API_KEY")
        assert validated.agent.extra["host_credential_bundle_count"] == 1
        assert validated.agent.extra["container_credential_absence_verified"] is True


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))
