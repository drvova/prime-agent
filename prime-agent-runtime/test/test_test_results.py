from __future__ import annotations

import asyncio
import importlib
import io
import shlex
import sys
import unittest
from dataclasses import replace
from pathlib import Path
from unittest import mock

from rlm import bash
from rlm.test_results import TestFailure, TestSummary, looks_like_test_run, summarize

test_results_module = sys.modules["rlm.test_results"]
_FIXTURES = Path(__file__).parent / "fixtures" / "test_runs"


def _fixture(name: str) -> str:
    return (_FIXTURES / f"{name}.txt").read_text()


class SummarizeTest(unittest.TestCase):
    def test_every_captured_run_summarizes_to_its_counts_and_failures(self):
        # Each parser was built from these real runs; a passing run has no failures.
        self.assertEqual(
            {name: summarize(_fixture(name)) for name in EXPECTED},
            EXPECTED,
        )

    def test_only_test_runner_commands_are_summarized(self):
        commands = {
            "cargo test -p pa-core": True,
            "cd crates/x && cargo +1.98.1 test --lib": True,
            "uv run pytest -q tests/": True,
            "python3 -m unittest discover -s test": True,
            "go test ./...": True,
            "npx vitest run": True,
            "npm test": True,
            "cargo build": False,
            "cat pytest.ini": False,
            "git log --grep=test": False,
        }
        self.assertEqual(
            {command: looks_like_test_run(command) for command in commands}, commands
        )

    def test_output_no_parser_recognizes_has_no_summary(self):
        self.assertIsNone(summarize("compiling...\nerror: linker failed\n"))


class AwaitedTestRunTest(unittest.IsolatedAsyncioTestCase):
    def _command(self) -> str:
        # Prints a captured cargo test run, then fails like it.
        return f"cat {shlex.quote(str(_FIXTURES / 'cargo_test.txt'))}; : cargo test; exit 101"

    async def test_an_awaited_test_run_returns_its_summary_with_handles(self):
        requests = []

        async def host_request(request_type, payload):
            requests.append(
                (
                    request_type,
                    payload["runner"],
                    payload["failed"],
                    len(payload["failures"]),
                )
            )
            return {
                "output": "out:00000000ff",
                "failures": ["out:00000000a1", "out:00000000a2"],
            }

        rlm_module = importlib.import_module("rlm")
        with (
            mock.patch.object(rlm_module, "host_request", host_request),
            mock.patch("sys.stdout", new_callable=io.StringIO) as stdout,
        ):
            result = await bash(self._command())
        first, second = EXPECTED["cargo_test"].failures
        expected = replace(
            EXPECTED["cargo_test"],
            output_handle="out:00000000ff",
            failures=(
                replace(first, output_handle="out:00000000a1"),
                replace(second, output_handle="out:00000000a2"),
            ),
        )
        self.assertEqual(
            (result.exit_code, result.tests, stdout.getvalue(), requests),
            (101, expected, f"{expected}\n", [("tests.summary", "cargo", 2, 2)]),
        )

    async def test_a_host_that_never_answers_never_holds_the_await(self):
        never = asyncio.Event()

        async def host_request(request_type, payload):
            await never.wait()

        rlm_module = importlib.import_module("rlm")
        with (
            mock.patch.object(rlm_module, "host_request", host_request),
            mock.patch.object(test_results_module, "_HANDLE_WAIT_SECONDS", 0.05),
            mock.patch("sys.stdout", new_callable=io.StringIO),
        ):
            result = await asyncio.wait_for(
                asyncio.ensure_future(bash(self._command())), 10
            )
        self.assertEqual(result.tests, EXPECTED["cargo_test"])


EXPECTED = {
    "cargo_test": TestSummary(
        runner="cargo",
        passed=1,
        failed=2,
        skipped=1,
        failures=(
            TestFailure(
                name="tests::panics_with_message",
                location="src/lib.rs:26:26",
                message="index three is present",
                output="---- tests::panics_with_message stdout ----\n\nthread 'tests::panics_with_message' (24282) panicked at src/lib.rs:26:26:\nindex three is present",
            ),
            TestFailure(
                name="tests::adds_two_numbers",
                location="src/lib.rs:11:9",
                message="assertion `left == right` failed\n  left: 5\n right: 4",
                output="---- tests::adds_two_numbers stdout ----\n\nthread 'tests::adds_two_numbers' (24280) panicked at src/lib.rs:11:9:\nassertion `left == right` failed\n  left: 5\n right: 4\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
            ),
        ),
    ),
    "cargo_test_ok": TestSummary(
        runner="cargo",
        passed=3,
        failed=0,
        skipped=1,
        failures=(),
    ),
    "pytest": TestSummary(
        runner="pytest",
        passed=1,
        failed=3,
        skipped=1,
        failures=(
            TestFailure(
                name="test_divide_wrong",
                location="test_calc.py:11",
                message="assert 3.5 == 3\n +  where 3.5 = divide(7, 2)",
                output="______________________________ test_divide_wrong _______________________________\n\n    def test_divide_wrong():\n>       assert divide(7, 2) == 3\nE       assert 3.5 == 3\nE        +  where 3.5 = divide(7, 2)\n\ntest_calc.py:11: AssertionError",
            ),
            TestFailure(
                name="test_parse",
                location="calc.py:6",
                message="ValueError: invalid literal for int() with base 10: 'x1'",
                output="__________________________________ test_parse __________________________________\n\n    def test_parse():\n>       assert parse(\"x1\") == 1\n               ^^^^^^^^^^^\n\ntest_calc.py:15: \n_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ \n\ntext = 'x1'\n\n    def parse(text):\n>       return int(text)\n               ^^^^^^^^^\nE       ValueError: invalid literal for int() with base 10: 'x1'\n\ncalc.py:6: ValueError",
            ),
            TestFailure(
                name="test_dict",
                location="test_calc.py:19",
                message="AssertionError: assert {'a': 1, 'b': 2} == {'a': 1, 'b': 3}\n\n  Omitting 1 identical items, use -vv to show\n  Differing items:\n  {'b': 2} != {'b': 3}\n  Use -v to get more diff",
                output="__________________________________ test_dict ___________________________________\n\n    def test_dict():\n>       assert {\"a\": 1, \"b\": 2} == {\"a\": 1, \"b\": 3}\nE       AssertionError: assert {'a': 1, 'b': 2} == {'a': 1, 'b': 3}\nE         \nE         Omitting 1 identical items, use -vv to show\nE         Differing items:\nE         {'b': 2} != {'b': 3}\nE         Use -v to get more diff\n\ntest_calc.py:19: AssertionError",
            ),
        ),
    ),
    "pytest_ok": TestSummary(
        runner="pytest",
        passed=1,
        failed=0,
        skipped=0,
        failures=(),
    ),
    "unittest": TestSummary(
        runner="unittest",
        passed=1,
        failed=2,
        skipped=1,
        failures=(
            TestFailure(
                name="test_unit.DivideTest.test_zero",
                location="/work/app/calc.py:2",
                message="ZeroDivisionError: division by zero",
                output='ERROR: test_zero (test_unit.DivideTest.test_zero)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File "/work/app/test_unit.py", line 14, in test_zero\n    divide(1, 0)\n    ~~~~~~^^^^^^\n  File "/work/app/calc.py", line 2, in divide\n    return a / b\n           ~~^~~\nZeroDivisionError: division by zero',
            ),
            TestFailure(
                name="test_unit.DivideTest.test_bad",
                location="/work/app/test_unit.py:11",
                message="AssertionError: 2.5 != 2",
                output='FAIL: test_bad (test_unit.DivideTest.test_bad)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File "/work/app/test_unit.py", line 11, in test_bad\n    self.assertEqual(divide(5, 2), 2)\n    ~~~~~~~~~~~~~~~~^^^^^^^^^^^^^^^^^\nAssertionError: 2.5 != 2',
            ),
        ),
    ),
    "go_test": TestSummary(
        runner="go",
        passed=None,
        failed=1,
        skipped=None,
        failures=(
            TestFailure(
                name="TestAdd",
                location="calc_test.go:7",
                message="calc_test.go:7: Add(2, 2) = 5, want 4",
                output="--- FAIL: TestAdd\ncalc_test.go:7: Add(2, 2) = 5, want 4",
            ),
        ),
    ),
    "go_test_v": TestSummary(
        runner="go",
        passed=1,
        failed=1,
        skipped=1,
        failures=(
            TestFailure(
                name="TestAdd",
                location="calc_test.go:7",
                message="calc_test.go:7: Add(2, 2) = 5, want 4",
                output="--- FAIL: TestAdd\ncalc_test.go:7: Add(2, 2) = 5, want 4",
            ),
        ),
    ),
    "jest": TestSummary(
        runner="jest",
        passed=1,
        failed=2,
        skipped=1,
        failures=(
            TestFailure(
                name="add › adds two numbers",
                location="math.test.js:5:23",
                message="expect(received).toBe(expected) // Object.is equality\n\nExpected: 4\nReceived: 5",
                output='  ● add › adds two numbers\n\n    expect(received).toBe(expected) // Object.is equality\n\n    Expected: 4\n    Received: 5\n\n      3 | describe("add", () => {\n      4 |   test("adds two numbers", () => {\n    > 5 |     expect(add(2, 2)).toBe(4);\n        |                       ^\n      6 |   });\n      7 |\n      8 |   test("adds objects", () => {\n\n      at Object.toBe (math.test.js:5:23)',
            ),
            TestFailure(
                name="add › adds objects",
                location="math.test.js:9:36",
                message='expect(received).toEqual(expected) // deep equality\n\n- Expected  - 1\n+ Received  + 1\n\n  Object {\n    "a": 1,\n-   "b": 0,\n+   "b": 1,\n  }',
                output='  ● add › adds objects\n\n    expect(received).toEqual(expected) // deep equality\n\n    - Expected  - 1\n    + Received  + 1\n\n      Object {\n        "a": 1,\n    -   "b": 0,\n    +   "b": 1,\n      }\n\n       7 |\n       8 |   test("adds objects", () => {\n    >  9 |     expect({ a: 1, b: add(0, 0) }).toEqual({ a: 1, b: 0 });\n         |                                    ^\n      10 |   });\n      11 |\n      12 |   test("zero is fine", () => {\n\n      at Object.toEqual (math.test.js:9:36)',
            ),
        ),
    ),
    "vitest": TestSummary(
        runner="vitest",
        passed=1,
        failed=1,
        skipped=1,
        failures=(
            TestFailure(
                name="add > adds two numbers",
                location="math.spec.mjs:8:23",
                message="AssertionError: expected 5 to be 4 // Object.is equality\n\n- Expected\n+ Received\n\n- 4\n+ 5",
                output=' FAIL  math.spec.mjs > add > adds two numbers\nAssertionError: expected 5 to be 4 // Object.is equality\n\n- Expected\n+ Received\n\n- 4\n+ 5\n\n ❯ math.spec.mjs:8:23\n      6| describe("add", () => {\n      7|   test("adds two numbers", () => {\n      8|     expect(add(2, 2)).toBe(4);\n       |                       ^\n      9|   });\n     10|   test("zero is fine", () => {',
            ),
        ),
    ),
}
