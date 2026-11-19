"""Kernel client for session-owned scheduled jobs.

``await schedule.add(prompt, at=...)`` runs ``prompt`` in this session once,
later; ``every=`` or ``cron=`` repeat it. The jobs live on the host's cron
store (they persist with the session and show in ``prime-agent schedule
list``), and a session lists and cancels only the jobs it created itself.
Runs without the prime-agent daemon (headless, daemonless) cannot fire a job
later, so they raise instead of accepting one.
"""

from __future__ import annotations

import builtins
import datetime as _dt
from dataclasses import dataclass
from typing import Any

from . import host_request

__all__ = ["ScheduledJob", "add", "cancel", "list"]


@dataclass(frozen=True)
class ScheduledJob:
    """One scheduled job: ``kind`` is ``once``/``interval``/``cron``; times are ISO-8601 UTC."""

    id: str
    prompt: str
    kind: str
    schedule: str
    status: str
    next_run_at: str | None = None
    last_run_at: str | None = None
    run_count: int = 0
    label: str | None = None
    last_error: str | None = None
    created_at: str | None = None

    def __str__(self) -> str:
        line = f"{self.id} {self.status} {self.kind} {self.schedule!r} next={self.next_run_at or '-'}"
        if self.label:
            line += f" label={self.label!r}"
        return f"{line} prompt={self.prompt!r}"


_FIELDS = ScheduledJob.__dataclass_fields__


def _job(payload: Any) -> ScheduledJob:
    if not isinstance(payload, dict) or not isinstance(payload.get("id"), str):
        raise RuntimeError(f"schedule request returned an unexpected job: {payload!r}")
    return ScheduledJob(**{key: value for key, value in payload.items() if key in _FIELDS})


def _seconds(value: Any, name: str) -> float:
    seconds = value.total_seconds() if isinstance(value, _dt.timedelta) else value
    if isinstance(seconds, bool) or not isinstance(seconds, (int, float)):
        raise TypeError(f"{name} must be a str, a number of seconds, or a timedelta")
    if seconds <= 0:
        raise ValueError(f"{name} must be positive")
    return float(seconds)


def _iso_utc(moment: _dt.datetime) -> str:
    # A naive datetime is local time, as datetime.now() returns it.
    utc = moment.astimezone(_dt.timezone.utc)
    return utc.strftime("%Y-%m-%dT%H:%M:%SZ")


async def add(
    prompt: str,
    *,
    at: str | _dt.datetime | _dt.timedelta | float | None = None,
    every: str | _dt.timedelta | float | None = None,
    cron: str | None = None,
    label: str | None = None,
) -> ScheduledJob:
    """Schedule ``prompt`` to run in this session later; pass exactly one of ``at``/``every``/``cron``.

    Args:
        prompt: The message the session receives when the job fires.
        at: One shot: a ``datetime`` (naive means local time), a delay
            (seconds or ``timedelta``), an ISO-8601 string (UTC unless it
            carries an offset), or ``"in 30m"``.
        every: Recurring interval: ``"30m"``, ``"2h"``, ``"1d"``, seconds, or a
            ``timedelta`` (at least 5 minutes).
        cron: Recurring five-field cron expression (UTC) or alias such as
            ``"@daily"`` (at most every 5 minutes).
        label: A short name shown in the job list.
    """
    if not isinstance(prompt, str) or not prompt.strip():
        raise TypeError("prompt must be a non-empty string")
    given = [name for name, value in (("at", at), ("every", every), ("cron", cron)) if value is not None]
    if len(given) != 1:
        raise ValueError("pass exactly one of at=, every=, or cron=")
    payload: dict[str, Any] = {"prompt": prompt}
    if isinstance(at, str):
        payload["at"] = at
    elif isinstance(at, _dt.datetime):
        payload["at"] = _iso_utc(at)
    elif at is not None:
        delay = _dt.timedelta(seconds=_seconds(at, "at"))
        payload["at"] = _iso_utc(_dt.datetime.now(_dt.timezone.utc) + delay)
    if isinstance(every, str):
        payload["every"] = every
    elif every is not None:
        payload["every"] = f"{round(_seconds(every, 'every'))}s"
    if cron is not None:
        if not isinstance(cron, str):
            raise TypeError("cron must be a string")
        payload["cron"] = cron
    if label is not None:
        if not isinstance(label, str):
            raise TypeError("label must be a string")
        payload["label"] = label
    reply = await host_request("schedule.add", payload)
    return _job(reply.get("job"))


async def list(*, include_inactive: bool = False) -> builtins.list[ScheduledJob]:  # noqa: A001 - the API name
    """This session's scheduled jobs, soonest first; ``include_inactive`` adds finished and cancelled ones."""
    if not isinstance(include_inactive, bool):
        raise TypeError("include_inactive must be a bool")
    reply = await host_request("schedule.list", {"include_inactive": include_inactive})
    jobs = reply.get("jobs")
    if not isinstance(jobs, builtins.list):
        raise RuntimeError(f"schedule.list returned an unexpected result: {reply!r}")
    return [_job(job) for job in jobs]


async def cancel(id: str) -> ScheduledJob:  # noqa: A002 - the job's id, as listed
    """Cancel one of this session's jobs by id (the user's own jobs are out of reach)."""
    if not isinstance(id, str) or not id:
        raise TypeError("id must be a non-empty string")
    reply = await host_request("schedule.cancel", {"id": id})
    return _job(reply.get("job"))
