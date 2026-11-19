from __future__ import annotations

import asyncio
import importlib.util
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from rlm import web

SKILL = Path(__file__).parents[2] / "skills/websearch/src/websearch/websearch.py"

_REPLY = {
    "provider": "duckduckgo",
    "label": "DuckDuckGo",
    "answer": None,
    "sources": [{"title": "Tokio", "url": "https://tokio.rs/", "snippet": "runtime", "publishedDate": "2024-01-02"}],
    "citations": [],
    "relatedQuestions": [],
    "failures": [{"provider": "serper", "label": "Serper", "reason": "401", "status": 401}],
    "notes": ["answered by DuckDuckGo after Serper failed (401)"],
    "text": "Note: answered by DuckDuckGo after Serper failed (401)\n[1] Tokio (2024-01-02)\n    https://tokio.rs/\n    runtime",
}


class WebSearchClientTest(unittest.TestCase):
    def test_search_sends_the_host_request_and_parses_the_reply(self) -> None:
        host = AsyncMock(return_value=_REPLY)
        with patch.object(web, "host_request", host):
            result = asyncio.run(web.search("tokio", provider="auto", num_results=3, recency="week", timeout=5))
        host.assert_awaited_once_with(
            "web.search",
            {"query": "tokio", "provider": "auto", "numResults": 3, "recency": "week", "timeoutSeconds": 5},
        )
        self.assertEqual(
            result,
            web.WebSearchResult(
                provider="duckduckgo",
                label="DuckDuckGo",
                answer=None,
                sources=[web.WebSearchSource("Tokio", "https://tokio.rs/", "runtime", "2024-01-02", None)],
                failures=[web.WebSearchFailure("serper", "Serper", "401", 401)],
                notes=["answered by DuckDuckGo after Serper failed (401)"],
                text=_REPLY["text"],
            ),
        )
        self.assertEqual(str(result), _REPLY["text"])

    def test_invalid_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(web, "host_request", host):
            with self.assertRaisesRegex(ValueError, "recency"):
                asyncio.run(web.search("q", recency="hour"))
            with self.assertRaisesRegex(ValueError, "num_results"):
                asyncio.run(web.search("q", num_results=0))
            with self.assertRaises(TypeError):
                asyncio.run(web.search("  "))
        host.assert_not_awaited()


_PAGE = {
    "url": "https://tokio.rs/",
    "finalUrl": "https://tokio.rs/tokio/tutorial",
    "title": "Tutorial",
    "contentType": "text/html",
    "method": "html",
    "markdown": "## Setup {#setup}\n\nInstall it.",
    "outline": [{"level": 2, "text": "Setup", "anchor": "setup", "url": "https://tokio.rs/tokio/tutorial#setup"}],
    "links": [{"text": "Docs", "url": "https://docs.rs/tokio"}],
    "section": "setup",
    "truncated": False,
    "totalChars": 31,
    "notes": [],
    "text": "# Tutorial\nSource: https://tokio.rs/tokio/tutorial (html, section setup)\n\n## Setup {#setup}\n\nInstall it.",
}


class WebReadClientTest(unittest.TestCase):
    def test_a_search_source_reads_through_the_host_request(self) -> None:
        host = AsyncMock(return_value=_PAGE)
        source = web.WebSearchSource("Tokio", "https://tokio.rs/")
        with patch.object(web, "host_request", host):
            page = asyncio.run(web.read(source, max_chars=500, section="setup", timeout=5))
        host.assert_awaited_once_with(
            "web.read",
            {"url": "https://tokio.rs/", "maxChars": 500, "section": "setup", "timeoutSeconds": 5},
        )
        self.assertEqual(
            page,
            web.WebPage(
                url="https://tokio.rs/",
                final_url="https://tokio.rs/tokio/tutorial",
                title="Tutorial",
                content_type="text/html",
                method="html",
                markdown="## Setup {#setup}\n\nInstall it.",
                outline=[web.WebHeading(2, "Setup", "setup", "https://tokio.rs/tokio/tutorial#setup")],
                links=[web.WebLink("Docs", "https://docs.rs/tokio")],
                section="setup",
                total_chars=31,
                text=_PAGE["text"],
            ),
        )
        self.assertEqual(str(page), _PAGE["text"])

    def test_invalid_read_arguments_fail_before_the_host(self) -> None:
        host = AsyncMock()
        with patch.object(web, "host_request", host):
            with self.assertRaises(TypeError):
                asyncio.run(web.read(web.WebHeading(2, "Setup", "setup")))
            with self.assertRaisesRegex(ValueError, "max_chars"):
                asyncio.run(web.read("https://tokio.rs/", max_chars=0))
        host.assert_not_awaited()


class WebsearchSkillTest(unittest.TestCase):
    def _module(self):
        spec = importlib.util.spec_from_file_location("websearch_skill_test", SKILL)
        assert spec and spec.loader
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def test_the_skill_delegates_to_the_native_host_request(self) -> None:
        module = self._module()
        host = AsyncMock(return_value=_REPLY)
        with patch.object(module, "host_request", host), patch.dict(
            "os.environ", {"PRIME_AGENT_WEBSEARCH_NUM_RESULTS": "7"}, clear=False
        ):
            output = asyncio.run(module.run("tokio", timeout=9))
        host.assert_awaited_once_with("web.search", {"query": "tokio", "numResults": 7, "timeoutSeconds": 9})
        self.assertEqual(output, f'Results for query "tokio":\n\n{_REPLY["text"]}')

    def test_setup_guidance_is_the_whole_answer(self) -> None:
        module = self._module()
        guidance = "Web search is not set up yet: no Brave API key is configured."
        with patch.object(module, "host_request", AsyncMock(side_effect=RuntimeError(guidance))):
            self.assertEqual(asyncio.run(module.run("q")), guidance)


if __name__ == "__main__":
    unittest.main()
