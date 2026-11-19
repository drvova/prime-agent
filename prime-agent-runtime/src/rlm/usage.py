"""Kernel client for the session's usage and the provider quotas (read-only).

``await usage.get()`` reports what this session has spent so far (cost with the
subagents' rolled-up share, and token counts including cache reads and writes)
and, when a daemon serves the session, each signed-in provider's quota windows
with their reset times, the same cards the ``/usage`` page shows. Long
autonomous runs use it to budget and to back off before a quota runs out.
Nothing in the reply is a credential.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from . import host_request

__all__ = ["Cost", "ProviderQuota", "QuotaWindow", "Tokens", "Usage", "get"]


@dataclass(frozen=True)
class Cost:
    """Session cost in USD: ``total`` is ``own`` (this session's calls) plus ``subagents``."""

    total: float
    own: float
    subagents: float


@dataclass(frozen=True)
class Tokens:
    """Session token counts; ``total`` sums input, output, and both cache counts."""

    input: int
    output: int
    cache_read: int
    cache_write: int
    total: int


@dataclass(frozen=True)
class QuotaWindow:
    """One quota window: ``used`` of ``limit`` in ``unit`` (``"percent"`` or ``"requests"``).

    ``resets_at`` is the reset time (UTC) when the provider reports one.
    """

    label: str
    used: float
    limit: float | None
    unit: str
    resets_at: datetime | None

    @property
    def used_fraction(self) -> float | None:
        """``used / limit`` (``None`` without a positive limit)."""
        if not self.limit or self.limit <= 0:
            return None
        return self.used / self.limit


@dataclass(frozen=True)
class ProviderQuota:
    """One provider's card. ``state`` is ``"windows"``, ``"rate_limited"``, or ``"failed"``.

    ``windows`` is filled for ``"windows"``; ``resets_at`` is set for a
    ``"rate_limited"`` card that reports its reset; ``error`` explains a
    ``"failed"`` card.
    """

    provider: str
    plan: str | None
    state: str
    windows: list[QuotaWindow]
    resets_at: datetime | None
    error: str | None


@dataclass(frozen=True)
class Usage:
    """``usage.get()``'s reply. ``quotas_unavailable`` says why ``quotas`` is empty, when it is."""

    cost: Cost
    tokens: Tokens
    quotas: list[ProviderQuota]
    quotas_unavailable: str | None = None

    def __str__(self) -> str:
        lines = [
            f"cost: ${self.cost.total:.4f} (own ${self.cost.own:.4f}, subagents ${self.cost.subagents:.4f})",
            (
                f"tokens: {self.tokens.total} (input {self.tokens.input}, output {self.tokens.output}, "
                f"cache read {self.tokens.cache_read}, cache write {self.tokens.cache_write})"
            ),
        ]
        if self.quotas_unavailable:
            lines.append(f"quotas: {self.quotas_unavailable}")
        for quota in self.quotas:
            title = quota.provider + (f" ({quota.plan})" if quota.plan else "")
            if quota.state == "failed":
                lines.append(f"{title}: unavailable: {quota.error}")
            elif quota.state == "rate_limited":
                reset = f", resets {quota.resets_at.isoformat()}" if quota.resets_at else ""
                lines.append(f"{title}: rate limited{reset}")
            else:
                lines.append(f"{title}:")
                for window in quota.windows:
                    amount = f"{window.used:g}" + (f"/{window.limit:g}" if window.limit is not None else "")
                    unit = "%" if window.unit == "percent" else f" {window.unit}"
                    reset = f", resets {window.resets_at.isoformat()}" if window.resets_at else ""
                    lines.append(f"  {window.label}: {amount}{unit}{reset}")
        return "\n".join(lines)


def _time(epoch_ms: Any) -> datetime | None:
    if not isinstance(epoch_ms, (int, float)) or isinstance(epoch_ms, bool):
        return None
    return datetime.fromtimestamp(epoch_ms / 1000, tz=timezone.utc)


def _quota(card: dict[str, Any]) -> ProviderQuota:
    state = card.get("state") or {}
    kind = str(state.get("kind", "failed"))
    return ProviderQuota(
        provider=str(card.get("provider", "")),
        plan=card.get("plan"),
        state=kind,
        windows=[
            QuotaWindow(
                label=str(window.get("label", "")),
                used=float(window.get("used", 0)),
                limit=None if window.get("limit") is None else float(window["limit"]),
                unit=str(window.get("unit", "percent")),
                resets_at=_time(window.get("resetsAtMs")),
            )
            for window in state.get("windows", [])
            if isinstance(window, dict)
        ],
        resets_at=_time(state.get("resetsAtMs")),
        error=state.get("error"),
    )


async def get(*, refresh: bool = False) -> Usage:
    """This session's cost and tokens, and the provider quota windows.

    Args:
        refresh: Re-read every provider's quota now instead of serving a card
            fetched in the last few minutes.
    """
    if not isinstance(refresh, bool):
        raise TypeError("refresh must be a bool")
    session = await host_request("usage.session", {})
    quotas = await host_request("usage.quotas", {"refresh": refresh})
    if not isinstance(session, dict) or not isinstance(quotas, dict):
        raise RuntimeError(f"usage request returned an unexpected result: {session!r}, {quotas!r}")
    cost = session.get("cost") or {}
    tokens = session.get("tokens") or {}
    return Usage(
        cost=Cost(
            total=float(cost.get("total", 0)),
            own=float(cost.get("own", 0)),
            subagents=float(cost.get("subagents", 0)),
        ),
        tokens=Tokens(
            input=int(tokens.get("input", 0)),
            output=int(tokens.get("output", 0)),
            cache_read=int(tokens.get("cacheRead", 0)),
            cache_write=int(tokens.get("cacheWrite", 0)),
            total=int(tokens.get("total", 0)),
        ),
        quotas=[_quota(card) for card in quotas.get("providers", []) if isinstance(card, dict)],
        quotas_unavailable=quotas.get("unavailable"),
    )
