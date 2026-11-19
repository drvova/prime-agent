from __future__ import annotations

import os
import unittest
from pathlib import Path
from unittest.mock import patch

from rlm.harness import _agent_dir as harness_agent_dir
from rlm.mcp_base import agent_dir as mcp_agent_dir
from rlm.agent_dir import agent_dir
from rlm.factory import user_machines_dir


class AgentDirTest(unittest.TestCase):
    """Every runtime reader of the agent dir honors the host's export, and without it falls back
    to the build's layout: ``~/.prime/agent-<variant>`` for a distribution build."""

    def test_the_host_export_wins_everywhere(self) -> None:
        with patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": "/srv/agent-x", "PRIME_AGENT_DIST_VARIANT": "fork"}):
            self.assertEqual(
                [agent_dir(), harness_agent_dir(), mcp_agent_dir(), user_machines_dir()],
                [Path("/srv/agent-x")] * 3 + [Path("/srv/agent-x/machines")],
            )

    def test_the_fallback_follows_the_distribution_layout(self) -> None:
        env = {key: value for key, value in os.environ.items() if key not in ("PRIME_AGENT_CODING_AGENT_DIR", "PI_CODING_AGENT_DIR")}
        home = Path.home()
        with patch.dict(os.environ, {**env, "PRIME_AGENT_DIST_VARIANT": "fork"}, clear=True):
            self.assertEqual(
                [harness_agent_dir(), user_machines_dir()],
                [(home / ".prime" / "agent-fork").resolve(), (home / ".prime" / "agent-fork").resolve() / "machines"],
            )
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(agent_dir(), (home / ".prime" / "agent").resolve())


if __name__ == "__main__":
    unittest.main()
