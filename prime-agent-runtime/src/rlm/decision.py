"""Kernel client for one bounded model decision mid-cell: ``rlm.decide``.

A cell that runs a whole search, edit, test, retry loop at kernel speed pays a
model call only where judgment is needed: ``await rlm.decide("the test still
fails; next step?", ["retry with the wider fix", "revert and stop"])`` asks the
session's own model one short question (it sees the question, the options and
``context``, not the conversation) and returns one of ``options`` verbatim.

The host bounds the decisions of one cell and prints each as one compact
``[decide]`` line in the cell's output, the only transcript growth a decision
causes.
"""

from __future__ import annotations

import sys
from collections.abc import Sequence

from . import host_request

__all__ = ["decide"]

QUESTION_CHARS_MAX = 2_000
OPTION_CHARS_MAX = 200
OPTIONS_MAX = 16
CONTEXT_CHARS_MAX = 8_000


def _current_cell() -> str | None:
    from . import repl

    return repl._current_cell.get()


async def decide(question: str, options: Sequence[str], *, context: str | None = None) -> str:
    """Ask the session's model to pick one of ``options``; the chosen option, verbatim.

    Args:
        question: The decision to make (at most 2,000 characters).
        options: 2 to 16 distinct non-empty choices (each at most 200 characters).
        context: What the model needs to decide (at most 8,000 characters): the
            failing output, the candidate diff. The model sees nothing else.

    Raises:
        RuntimeError: the cell used up its decisions, or the model call failed
            (the provider's error, verbatim).
    """
    if not isinstance(question, str) or not question.strip():
        raise ValueError("question must be a non-empty string")
    if len(question) > QUESTION_CHARS_MAX:
        raise ValueError(f"question is at most {QUESTION_CHARS_MAX} characters (got {len(question)})")
    if isinstance(options, str) or not isinstance(options, Sequence):
        raise TypeError("options must be a list of strings")
    choices = list(options)
    if not 2 <= len(choices) <= OPTIONS_MAX:
        raise ValueError(f"options must hold 2 to {OPTIONS_MAX} choices (got {len(choices)})")
    for choice in choices:
        if not isinstance(choice, str) or not choice.strip():
            raise ValueError("every option must be a non-empty string")
        if len(choice) > OPTION_CHARS_MAX:
            raise ValueError(f"an option is at most {OPTION_CHARS_MAX} characters (got {len(choice)})")
    if len(set(choices)) != len(choices):
        raise ValueError("options must be distinct")
    if context is not None:
        if not isinstance(context, str):
            raise TypeError("context must be a string or None")
        if len(context) > CONTEXT_CHARS_MAX:
            raise ValueError(f"context is at most {CONTEXT_CHARS_MAX} characters (got {len(context)})")
    reply = await host_request(
        "rlm.decide",
        {"question": question, "options": choices, "context": context, "cell": _current_cell()},
    )
    index = reply.get("index") if isinstance(reply, dict) else None
    if not isinstance(index, int) or not 0 <= index < len(choices):
        raise RuntimeError(f"rlm.decide returned an unexpected result: {reply!r}")
    row = reply.get("row")
    if isinstance(row, str) and row:
        print(row, file=sys.stdout, flush=True)
    return choices[index]
