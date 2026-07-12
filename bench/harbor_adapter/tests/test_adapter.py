"""Tests for the Stella Harbor adapter."""

import os
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest

from stella_harbor import StellaAgent, _cached_binary, _is_truthy, _locate_binary


class TestUtilityFunctions:
    """Test utility functions."""

    def test_is_truthy(self):
        """Test truthy value checking."""
        assert _is_truthy("1")
        assert _is_truthy("true")
        assert _is_truthy("TRUE")
        assert _is_truthy("yes")
        assert _is_truthy("on")
        assert not _is_truthy("0")
        assert not _is_truthy("false")
        assert not _is_truthy("")
        assert not _is_truthy(None)

    def test_cached_binary(self):
        """Test finding binaries on PATH."""
        # 'ls' should exist on most systems
        result = _cached_binary("ls")
        if result:
            assert result.is_file()
            assert os.access(result, os.X_OK)

        # Nonsensical binary should return None
        assert _cached_binary("nonexistent-binary-xyz123") is None


class TestStellaAgent:
    """Test StellaAgent implementation."""

    def test_name(self):
        """Test agent name."""
        assert StellaAgent.name() == "stella"

    def test_get_version_command(self):
        """Test version command."""
        agent = StellaAgent()
        assert agent.get_version_command() == "stella --version"

    def test_parse_version(self):
        """Test version parsing."""
        agent = StellaAgent()
        # Single line
        assert agent.parse_version("stella 0.1.0") == "stella 0.1.0"
        # Multi-line
        output = "stella 0.1.0\nSome other info"
        assert agent.parse_version(output) == "stella 0.1.0"
        # With whitespace
        assert agent.parse_version("  stella 0.1.0  ") == "stella 0.1.0"

    def test_build_command(self):
        """Test command building."""
        agent = StellaAgent()

        # Default model and budget
        os.environ.pop("STELLA_MODEL", None)
        os.environ.pop("STELLA_BUDGET", None)
        cmd = agent._build_command("Fix the bug")
        assert "stella" in cmd
        assert "--model" in cmd
        assert "anthropic/claude-fable-5" in cmd
        assert "--budget" in cmd
        assert "5.0" in cmd
        assert "run" in cmd
        assert "Fix the bug" in cmd

        # Custom model and budget
        os.environ["STELLA_MODEL"] = "zai/glm-5.2"
        os.environ["STELLA_BUDGET"] = "10.0"
        cmd = agent._build_command("Add a feature")
        assert "zai/glm-5.2" in cmd
        assert "10.0" in cmd

    def test_forwarded_env(self):
        """Test environment variable forwarding."""
        agent = StellaAgent()

        # Clear relevant env vars
        for key in list(os.environ.keys()):
            if key.startswith("STELLA_") or key.endswith("_API_KEY"):
                del os.environ[key]

        # No Stella vars set
        env = agent._forwarded_env()
        assert "STELLA" not in str(env)

        # Set some vars. Deliberately set the provider key FIRST, then set
        # another env var afterwards, so the provider key's real value differs
        # from "whatever os.environ iterated last". This is the regression
        # guard for the leftover-`value` bug: forwarding `value` (the last
        # loop binding) instead of `os.environ[key]` would fail here.
        os.environ["ANTHROPIC_API_KEY"] = "sk-anthropic-real"
        os.environ["OPENAI_API_KEY"] = "sk-openai-real"
        os.environ["STELLA_MODEL"] = "anthropic/claude-fable-5"
        os.environ["STELLA_BUDGET"] = "5.0"
        os.environ["_ADAPTER_TEST_SENTINEL"] = "not-a-key"  # iterated after the keys
        env = agent._forwarded_env()
        assert env["STELLA_MODEL"] == "anthropic/claude-fable-5"
        assert env["STELLA_BUDGET"] == "5.0"
        # Each provider key must carry its OWN value, not a leftover binding.
        assert env["ANTHROPIC_API_KEY"] == "sk-anthropic-real"
        assert env["OPENAI_API_KEY"] == "sk-openai-real"
        # A non-provider var must never be forwarded as a credential.
        assert "_ADAPTER_TEST_SENTINEL" not in env

        # Clean up
        del os.environ["STELLA_MODEL"]
        del os.environ["STELLA_BUDGET"]
        del os.environ["ANTHROPIC_API_KEY"]
        del os.environ["OPENAI_API_KEY"]
        del os.environ["_ADAPTER_TEST_SENTINEL"]


@pytest.mark.asyncio
class TestStellaAgentAsync:
    """Test async methods."""

    async def test_install(self):
        """Test install method (requires mock environment)."""
        agent = StellaAgent()
        env = AsyncMock()
        env.exec = AsyncMock(return_value=MagicMock(return_code=0))

        # This is a basic test - full integration requires actual Harbor setup
        # The method should upload the binary and install it
        # We'll just verify the method exists and is callable
        assert hasattr(agent, "install")
        assert callable(agent.install)

    async def test_run(self):
        """Test run method (requires mock environment)."""
        agent = StellaAgent()
        env = AsyncMock()
        env.exec = AsyncMock(return_value=MagicMock(return_code=0))

        # Verify method exists and is callable
        assert hasattr(agent, "run")
        assert callable(agent.run)

    def test_populate_context_post_run(self):
        """Test metadata extraction."""
        agent = StellaAgent()
        agent.logs_dir = "/tmp/test-logs"

        # Create a mock log file
        log_dir = Path(agent.logs_dir)
        log_dir.mkdir(parents=True, exist_ok=True)
        log_file = log_dir / "stella-run.txt"
        log_file.write_text("815.53s total · 83086 tok · $0.2714 · 37 steps")

        context = MagicMock()
        context.metadata = {}
        agent.populate_context_post_run(context)

        assert context.cost_usd == 0.2714
        assert context.metadata["stella_total_tokens"] == 83086
        assert context.metadata["stella_wall_sec"] == 815.53
        assert context.metadata["stella_steps"] == 37

        # Clean up
        log_file.unlink()
        log_dir.rmdir()


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
