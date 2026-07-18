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

import json
import os

import pytest

pytest.importorskip("harbor", reason="Harbor is required to import the adapter")

from stella_harbor import (  # noqa: E402 - after importorskip by design
    StellaAgent,
    _cached_binary,
    _extract_metrics,
    _is_truthy,
    _load_json_object,
    _sum_step_usage,
)


def _bare_agent() -> StellaAgent:
    """A StellaAgent instance bypassing the Harbor base ``__init__``.

    Only the attributes a given method reads are set by the test.
    """
    return StellaAgent.__new__(StellaAgent)


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


class TestVersion:
    def test_name(self) -> None:
        assert StellaAgent.name() == "stella"

    def test_get_version_command(self) -> None:
        assert _bare_agent().get_version_command() == "stella --version"

    def test_parse_version(self) -> None:
        agent = _bare_agent()
        assert agent.parse_version("stella 0.3.0") == "stella 0.3.0"
        assert agent.parse_version("stella 0.3.0\nextra") == "stella 0.3.0"
        assert agent.parse_version("  stella 0.3.0  ") == "stella 0.3.0"


class TestBuildCommand:
    def test_uses_harbor_model_name(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("STELLA_MODEL", raising=False)
        monkeypatch.delenv("STELLA_BUDGET", raising=False)
        monkeypatch.delenv("STELLA_BASE_URL", raising=False)
        agent = _bare_agent()
        agent.model_name = "anthropic/claude-fable-5"  # Harbor's -m flows here

        cmd = agent._build_command("Fix the bug")

        # Global flags must precede the `run` subcommand.
        assert cmd.index("--model") < cmd.index(" run ")
        assert cmd.index("--budget") < cmd.index(" run ")
        assert cmd.index("--output-format") < cmd.index(" run ")
        assert "--model anthropic/claude-fable-5" in cmd
        assert "--output-format json" in cmd
        assert "--budget 5.0" in cmd  # default budget
        assert cmd.endswith("run 'Fix the bug'")
        assert "--base-url" not in cmd  # not set

    def test_env_overrides_and_base_url(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("STELLA_MODEL", "zai/glm-5.2")
        monkeypatch.setenv("STELLA_BUDGET", "10.0")
        monkeypatch.setenv("STELLA_BASE_URL", "https://api.z.ai/api/coding/paas/v4")
        agent = _bare_agent()
        agent.model_name = None  # env should win when Harbor didn't set one

        cmd = agent._build_command("Add a feature")

        assert "--model zai/glm-5.2" in cmd
        assert "--budget 10.0" in cmd
        assert "--base-url https://api.z.ai/api/coding/paas/v4" in cmd

    def test_instruction_is_shell_quoted(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("STELLA_MODEL", raising=False)
        agent = _bare_agent()
        agent.model_name = "anthropic/claude-fable-5"
        cmd = agent._build_command("rm -rf / ; echo $HOME")
        # The dangerous string must be a single quoted argument to `run`.
        assert "run 'rm -rf / ; echo $HOME'" in cmd


class TestForwardedEnv:
    def test_forwards_provider_keys_with_own_values(
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

        assert env["STELLA_MODEL"] == "anthropic/claude-fable-5"
        assert env["STELLA_BUDGET"] == "5.0"
        assert env["ANTHROPIC_API_KEY"] == "sk-anthropic-real"
        assert env["OPENAI_API_KEY"] == "sk-openai-real"
        assert "_ADAPTER_TEST_SENTINEL" not in env

    def test_merges_harbor_extra_env(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for key in list(os.environ):
            if key.startswith("STELLA_"):
                monkeypatch.delenv(key, raising=False)
        agent = _bare_agent()
        agent.extra_env = {"MAX_THINKING_TOKENS": "2048"}
        env = agent._forwarded_env()
        assert env["MAX_THINKING_TOKENS"] == "2048"


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
    def test_populates_from_stashed_metrics(self) -> None:
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


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))
