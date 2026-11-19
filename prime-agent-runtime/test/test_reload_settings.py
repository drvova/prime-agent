from __future__ import annotations

import asyncio
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, call, patch

import rlm
from rlm import model, resource_reload, settings

_SESSION = {
    "model": {"id": "claude-x", "provider": "anthropic", "contextWindow": 200000, "thinkingLevel": "high"},
    "cellTimeoutMs": 60000,
    "compaction": {"enabled": True, "thresholdPercent": 80.0, "keepRecentTokens": 20000, "agentCallable": True},
    "imageModel": None,
    "webSearchProvider": "auto",
    "cacheRetention": "short",
    "rlmDepth": 0,
    "daemonless": False,
}
_HOST = {
    "rlmMaxDepth": {"maxDepth": 2, "source": "default"},
    "autonomous": {
        "enabled": True,
        "continuationsUsed": 1,
        "turnsUsed": 4,
        "tokensUsed": 9000,
        "limits": {"maxContinuations": 5, "maxTurns": 0, "maxTokens": 100000, "timeoutMs": 0},
    },
}


class SettingsClientTest(unittest.TestCase):
    def test_get_reads_both_halves_and_parses_them(self) -> None:
        host = AsyncMock(side_effect=[_SESSION, _HOST])
        with patch.object(settings, "host_request", host):
            result = asyncio.run(settings.get())
        self.assertEqual(host.await_args_list, [call("settings.session", {}), call("settings.host", {})])
        self.assertEqual(
            result,
            settings.Settings(
                model="anthropic/claude-x",
                context_window=200000,
                thinking_level="high",
                cell_timeout_ms=60000,
                compaction=settings.Compaction(True, 80.0, 20000, True),
                rlm_depth=0,
                rlm_max_depth=2,
                rlm_max_depth_source="default",
                image_model=None,
                web_search_provider="auto",
                cache_retention="short",
                daemonless=False,
                autonomous=settings.Autonomous(
                    enabled=True,
                    continuations_used=1,
                    turns_used=4,
                    tokens_used=9000,
                    limits={"max_continuations": 5, "max_turns": 0, "max_tokens": 100000, "timeout_ms": 0},
                ),
            ),
        )
        self.assertIn("thinking high", str(result))

    def test_no_autonomous_runner_reads_as_none(self) -> None:
        host = AsyncMock(side_effect=[_SESSION, {"rlmMaxDepth": {"maxDepth": 2, "source": "global"}, "autonomous": None}])
        with patch.object(settings, "host_request", host):
            result = asyncio.run(settings.get())
        self.assertIsNone(result.autonomous)
        self.assertIn("autonomous: unavailable", str(result))


class ModelInfoClientTest(unittest.TestCase):
    def test_info_parses_the_live_facts(self) -> None:
        reply = {"id": "gpt-x", "provider": "openai", "input": ["text", "image"], "contextWindow": 400000, "thinkingLevel": "medium"}
        host = AsyncMock(return_value=reply)
        with patch.object(model, "host_request", host):
            result = asyncio.run(model.info())
        host.assert_awaited_once_with("model.info", {})
        self.assertEqual(result, model.ModelInfo("gpt-x", "openai", ["text", "image"], 400000, "medium"))
        self.assertEqual((result.selector, result.image_input), ("openai/gpt-x", True))


class ReloadClientTest(unittest.TestCase):
    def test_reload_binds_a_new_python_skill_from_its_src_layout(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            package = Path(root) / "zz-reload-probe"
            (package / "src" / "zz_reload_probe").mkdir(parents=True)
            (package / "src" / "zz_reload_probe" / "__init__.py").write_text("def run():\n    return 'ok'\n")
            reply = {
                "skillsAdded": ["zz-reload-probe"],
                "skillsRemoved": [],
                "skills": 13,
                "promptTemplates": 0,
                "contextFiles": [],
                "systemPromptChanged": True,
                "pythonSkills": [
                    {"name": "zz-reload-probe", "importName": "zz_reload_probe", "packagePath": str(package)},
                    {"name": "gone", "importName": "zz_missing_probe", "packagePath": str(Path(root) / "gone")},
                ],
                "diagnostics": [],
            }
            namespace = types.ModuleType("__main__")
            host = AsyncMock(return_value=reply)
            saved_path = list(sys.path)
            try:
                with patch.object(resource_reload, "host_request", host), patch.dict(sys.modules, {"__main__": namespace}):
                    result = asyncio.run(rlm.rlm.reload())
                self.assertEqual(namespace.zz_reload_probe.run(), "ok")
            finally:
                sys.path[:] = saved_path
                sys.modules.pop("zz_reload_probe", None)
        host.assert_awaited_once_with("rlm.reload", {})
        self.assertEqual(
            result,
            resource_reload.ReloadResult(
                skills_added=["zz-reload-probe"],
                skills_removed=[],
                skills=13,
                prompt_templates=0,
                context_files=[],
                system_prompt_changed=True,
                python_skills_bound=["zz_reload_probe"],
                unbound={"zz_missing_probe": "no module named 'zz_missing_probe'"},
            ),
        )


if __name__ == "__main__":
    unittest.main()
