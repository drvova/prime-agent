"""Typed summaries of test runs: the counts and each failure, parsed from the runner's output.

An awaited `bash()` command that runs a test suite (cargo test, pytest, unittest, go test, jest,
vitest) returns its BashResult with `tests`: a TestSummary with the passed/failed/skipped counts
and, for each failure, its name, location, assertion text (the diff the runner printed), and a
handle to that failure's own section of the output. The whole output stays on the result and,
when the session keeps a spill store, under the summary's own handle. Each parser was built from
captured real output (`test/fixtures/test_runs/`); a run no parser recognizes gets no summary.
"""

from __future__ import annotations

import asyncio
import re
import textwrap
from collections.abc import Callable
from dataclasses import dataclass, replace

__all__ = ["TestFailure", "TestSummary", "attach_handles", "looks_like_test_run", "summarize"]

# A command that starts a test runner (anywhere in a shell line: `cd x && cargo test -p y`).
_TEST_COMMAND = re.compile(
    r"(?:^|[\s;&|(])(?:cargo(?:\s+\+\S+)?\s+(?:test|nextest\s+run)|py\.?test|"
    r"(?:\S*/)?python[\d.]*\s+-m\s+(?:pytest|unittest)|go\s+test|jest|vitest|"
    r"(?:npm|pnpm|yarn|bun)\s+(?:run\s+)?test)(?=[\s;&|)]|$)"
)
_ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
# Bounds of the rendered summary (the typed fields keep everything).
_SHOWN_FAILURES = 10
_MESSAGE_LINES = 12
_MESSAGE_CHARS = 1000
# How long an await waits for the host to store the outputs before returning without handles.
_HANDLE_WAIT_SECONDS = 2.0


def looks_like_test_run(command: str) -> bool:
    """Whether `command` starts a test runner this module can summarize."""
    return _TEST_COMMAND.search(command) is not None


@dataclass(frozen=True)
class TestFailure:
    """One failed test: `location` is `path:line[:col]` when the output names one; `message` is
    the assertion text the runner printed (its expected/actual diff included)."""

    __test__ = False  # not a pytest test class

    name: str
    location: str | None
    message: str
    output: str
    output_handle: str | None = None

    def __repr__(self) -> str:
        return f"TestFailure(name={self.name!r}, location={self.location!r}, output_handle={self.output_handle!r})"


@dataclass(frozen=True)
class TestSummary:
    """One test run: `passed`/`skipped` are None when the runner does not print them (go test
    without -v); `failures` holds every failed test in output order."""

    __test__ = False  # not a pytest test class

    runner: str
    passed: int | None
    failed: int
    skipped: int | None
    failures: tuple[TestFailure, ...]
    output_handle: str | None = None

    def __str__(self) -> str:
        counts = [f"{self.failed} failed"]
        if self.passed is not None:
            counts.insert(0, f"{self.passed} passed")
        if self.skipped is not None:
            counts.append(f"{self.skipped} skipped")
        head = f"[tests: {self.runner} · {' · '.join(counts)}"
        if self.output_handle:
            head += f" · output {self.output_handle}"
        lines = [head + "]"]
        for failure in self.failures[:_SHOWN_FAILURES]:
            where = f" · {failure.location}" if failure.location else ""
            handle = f" · {failure.output_handle}" if failure.output_handle else ""
            lines.append(f"FAILED {failure.name}{where}{handle}")
            message = failure.message[:_MESSAGE_CHARS].splitlines()[:_MESSAGE_LINES]
            lines.extend(f"    {line}" for line in message)
        hidden = len(self.failures) - _SHOWN_FAILURES
        if hidden > 0:
            lines.append(f"[{hidden} more failures in .tests.failures]")
        return "\n".join(lines)

    def _repl_text_(self) -> str:
        return str(self)


def _section_lines(lines: list[str], start: int, stop: Callable[[str], bool]) -> list[str]:
    """Lines after `start` up to (not including) the first one `stop` accepts."""
    taken: list[str] = []
    for line in lines[start + 1 :]:
        if stop(line):
            break
        taken.append(line)
    return taken


def _trimmed(lines: list[str]) -> str:
    return "\n".join(lines).strip("\n")


# cargo test

_CARGO_RESULT = re.compile(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored")
_CARGO_SECTION = re.compile(r"^---- (\S+) stdout ----$")
_RUST_PANIC = re.compile(r"panicked at (?:'(.*)', )?([^\s:'][^:']*?:\d+:\d+):?\s*$")


def _cargo(lines: list[str]) -> TestSummary | None:
    totals = [0, 0, 0]
    seen = False
    for line in lines:
        if match := _CARGO_RESULT.match(line):
            seen = True
            for at in range(3):
                totals[at] += int(match.group(at + 1))
    if not seen:
        return None
    failures = []
    for at, line in enumerate(lines):
        if not (header := _CARGO_SECTION.match(line)):
            continue
        section = _section_lines(
            lines, at, lambda line: line.startswith("---- ") or line in {"failures:", "successes:"}
        )
        location, message = None, []
        for index, body in enumerate(section):
            if panic := _RUST_PANIC.search(body):
                location = panic.group(2)
                message = [panic.group(1)] if panic.group(1) else []
                message.extend(
                    rest for rest in _section_lines(section, index, lambda rest: rest.startswith("note: ")) if rest
                )
                break
        failures.append(
            TestFailure(
                name=header.group(1),
                location=location,
                message="\n".join(message),
                output=_trimmed([line, *section]),
            )
        )
    return TestSummary("cargo", totals[0], totals[1], totals[2], tuple(failures))


# pytest

_PYTEST_FINAL = re.compile(r"^=+ (.*\d+ (?:passed|failed|skipped|error|errors|deselected|xfailed|xpassed).*) in [\d.]+s(?: \(.*\))? =+$")
_PYTEST_SECTION = re.compile(r"^_{3,} (\S.*?) _{3,}$")
_PYTEST_FRAME = re.compile(r"^([^\s:]+\.py):(\d+): ?\w*\s*$")


def _counts(text: str, pattern: str) -> dict[str, int]:
    return {word: int(number) for number, word in re.findall(pattern, text)}


def _pytest(lines: list[str]) -> TestSummary | None:
    final = next((match for line in reversed(lines) if (match := _PYTEST_FINAL.match(line))), None)
    if final is None:
        return None
    counts = _counts(final.group(1), r"(\d+) (\w+)")
    failures = []
    for at, line in enumerate(lines):
        if not (header := _PYTEST_SECTION.match(line)):
            continue
        section = _section_lines(lines, at, lambda line: bool(_PYTEST_SECTION.match(line)) or line.startswith("="))
        frames = [frame for body in section if (frame := _PYTEST_FRAME.match(body))]
        location = f"{frames[-1].group(1)}:{frames[-1].group(2)}" if frames else None
        message = textwrap.dedent("\n".join(body[1:] for body in section if body.startswith("E "))).strip("\n")
        failures.append(
            TestFailure(
                name=header.group(1),
                location=location,
                message=message,
                output=_trimmed([line, *section]),
            )
        )
    return TestSummary(
        "pytest",
        counts.get("passed", 0),
        counts.get("failed", 0) + counts.get("error", 0) + counts.get("errors", 0),
        counts.get("skipped", 0),
        tuple(failures),
    )


# unittest

_UNITTEST_RAN = re.compile(r"^Ran (\d+) tests? in ")
_UNITTEST_STATUS = re.compile(r"^(OK|FAILED)(?: \((.*)\))?$")
_UNITTEST_SECTION = re.compile(r"^(?:ERROR|FAIL): (\S+) \((\S+)\)")
_PY_FRAME = re.compile(r'^\s*File "([^"]+)", line (\d+)')


def _unittest(lines: list[str]) -> TestSummary | None:
    ran = next((match for line in lines if (match := _UNITTEST_RAN.match(line))), None)
    status = next((match for line in reversed(lines) if (match := _UNITTEST_STATUS.match(line))), None)
    if ran is None or status is None:
        return None
    counts = {word: int(number) for word, number in re.findall(r"(\w+)=(\d+)", status.group(2) or "")}
    failed = counts.get("failures", 0) + counts.get("errors", 0)
    skipped = counts.get("skipped", 0)
    expected = counts.get("expected_failures", 0)
    failures = []
    for at, line in enumerate(lines):
        if not (header := _UNITTEST_SECTION.match(line)):
            continue
        section = _section_lines(lines, at, lambda line: line.startswith(("=" * 10, "Ran ")))
        while section and set(section[-1]) <= {"-", ""}:
            section.pop()
        frames = [frame for body in section if (frame := _PY_FRAME.match(body))]
        location = f"{frames[-1].group(1)}:{frames[-1].group(2)}" if frames else None
        # The exception line (and an assertEqual diff under it) ends the traceback.
        tail = next(
            (
                index
                for index, body in enumerate(section)
                if body and not body[0].isspace() and not body.startswith(("Traceback", "-"))
            ),
            None,
        )
        failures.append(
            TestFailure(
                name=header.group(2),
                location=location,
                message=_trimmed(section[tail:]) if tail is not None else "",
                output=_trimmed([line, *section]),
            )
        )
    return TestSummary("unittest", int(ran.group(1)) - failed - skipped - expected, failed, skipped, tuple(failures))


# go test

_GO_RESULT = re.compile(r"^\s*--- (PASS|FAIL|SKIP): (\S+)")
_GO_RUN = re.compile(r"^=== RUN\s+(\S+)")
_GO_PACKAGE = re.compile(r"^(?:ok|FAIL|\?)\s+\S+")
_GO_LOCATION = re.compile(r"^\s+(\S+\.go:\d+): (.*)$")


def _go(lines: list[str]) -> TestSummary | None:
    results = [match for line in lines if (match := _GO_RESULT.match(line))]
    if not results and not any(_GO_PACKAGE.match(line) for line in lines):
        return None
    verbose = any(_GO_RUN.match(line) for line in lines)
    tally = {"PASS": 0, "FAIL": 0, "SKIP": 0}
    for match in results:
        tally[match.group(1)] += 1
    # A test's messages print under `=== RUN` (-v) or under its `--- FAIL:` line.
    messages: dict[str, list[str]] = {}
    current: str | None = None
    for line in lines:
        if run := _GO_RUN.match(line):
            current = run.group(1)
            continue
        if result := _GO_RESULT.match(line):
            current = result.group(2) if result.group(1) == "FAIL" and not verbose else None
            continue
        if current is not None and line.startswith("    "):
            messages.setdefault(current, []).append(line.strip())
        elif not line.startswith(" "):
            current = None
    failures = []
    for match in results:
        if match.group(1) != "FAIL":
            continue
        body = messages.get(match.group(2), [])
        located = next((found for text in body if (found := _GO_LOCATION.match("    " + text))), None)
        failures.append(
            TestFailure(
                name=match.group(2),
                location=located.group(1) if located else None,
                message="\n".join(body),
                output=_trimmed([match.group(0).strip(), *body]),
            )
        )
    return TestSummary(
        "go",
        tally["PASS"] if verbose else None,
        tally["FAIL"],
        tally["SKIP"] if verbose else None,
        tuple(failures),
    )


# jest

_JEST_TESTS = re.compile(r"^Tests:\s+(.*?),?\s+(\d+) total$")
_JEST_SECTION = re.compile(r"^\s+● (.+)$")
_JEST_AT = re.compile(r"^\s+at .*\(([^()\s]+:\d+:\d+)\)$")
_CODE_FRAME = re.compile(r"^\s*>?\s*\d*\s*\|")


def _jest(lines: list[str]) -> TestSummary | None:
    totals = next((match for line in lines if (match := _JEST_TESTS.match(line))), None)
    if totals is None:
        return None
    counts = _counts(totals.group(1), r"(\d+) (\w+)")
    failures = []
    for at, line in enumerate(lines):
        if not (header := _JEST_SECTION.match(line)):
            continue
        section = _section_lines(
            lines, at, lambda line: bool(_JEST_SECTION.match(line)) or line.startswith(("Test Suites:", "Tests:"))
        )
        located = next((found for body in section if (found := _JEST_AT.match(body))), None)
        frame = next((index for index, body in enumerate(section) if _CODE_FRAME.match(body)), len(section))
        failures.append(
            TestFailure(
                name=header.group(1).strip(),
                location=located.group(1) if located else None,
                message=textwrap.dedent("\n".join(section[:frame])).strip("\n"),
                output=_trimmed([line, *section]),
            )
        )
    return TestSummary(
        "jest", counts.get("passed", 0), counts.get("failed", 0), counts.get("skipped", 0), tuple(failures)
    )


# vitest

_VITEST_TESTS = re.compile(r"^\s+Tests\s+(.*?)\s+\((\d+)\)$")
_VITEST_SECTION = re.compile(r"^\s*FAIL\s+(\S+ > .+)$")
_VITEST_AT = re.compile(r"^\s*❯ (\S+:\d+:\d+)$")


def _vitest(lines: list[str]) -> TestSummary | None:
    totals = next((match for line in lines if (match := _VITEST_TESTS.match(line))), None)
    if totals is None:
        return None
    counts = _counts(totals.group(1), r"(\d+) (\w+)")
    failures = []
    for at, line in enumerate(lines):
        if not (header := _VITEST_SECTION.match(line)):
            continue
        section = _section_lines(lines, at, lambda line: line.startswith("⎯") or bool(_VITEST_SECTION.match(line)))
        located_at = next((index for index, body in enumerate(section) if _VITEST_AT.match(body)), None)
        message = section[:located_at] if located_at is not None else section
        located = _VITEST_AT.match(section[located_at]) if located_at is not None else None
        failures.append(
            TestFailure(
                name=header.group(1).split(" > ", 1)[1],
                location=located.group(1) if located else None,
                message=_trimmed(message),
                output=_trimmed([line, *section]),
            )
        )
    return TestSummary(
        "vitest", counts.get("passed", 0), counts.get("failed", 0), counts.get("skipped", 0), tuple(failures)
    )


# The registry, most specific first: jest and vitest print lines a looser parser could misread.
_PARSERS: tuple[Callable[[list[str]], TestSummary | None], ...] = (_cargo, _pytest, _unittest, _jest, _vitest, _go)


def summarize(output: str) -> TestSummary | None:
    """The summary of one test run's output, or None when no parser recognizes it."""
    lines = _ANSI.sub("", output).splitlines()
    for parser in _PARSERS:
        summary = parser(lines)
        if summary is not None:
            return summary
    return None


async def attach_handles(summary: TestSummary, output: str) -> TestSummary:
    """The summary with handles: the host stores the whole output and each failure's section in
    the session's spill store (and counts the run). A host that does not answer within
    _HANDLE_WAIT_SECONDS, or a session without a store, leaves the handles unset."""
    from . import host_request

    payload = {
        "runner": summary.runner,
        "failed": summary.failed,
        "output": output,
        "failures": [failure.output for failure in summary.failures],
    }
    try:
        reply = await asyncio.wait_for(host_request("tests.summary", payload), _HANDLE_WAIT_SECONDS)
    except (TimeoutError, OSError, RuntimeError, ValueError):
        return summary
    handles = reply.get("failures")
    if not isinstance(handles, list) or len(handles) != len(summary.failures):
        handles = [None] * len(summary.failures)
    return replace(
        summary,
        output_handle=reply.get("output") if isinstance(reply.get("output"), str) else None,
        failures=tuple(
            replace(failure, output_handle=handle if isinstance(handle, str) else None)
            for failure, handle in zip(summary.failures, handles)
        ),
    )
