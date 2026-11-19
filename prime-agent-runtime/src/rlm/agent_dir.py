"""The Prime Agent agent dir, resolved the way the host resolves it.

The host exports the session's agent dir into the kernel as
``PRIME_AGENT_CODING_AGENT_DIR``, so inside a kernel (and every ``bash()``
shell it starts) that variable is the answer. The fallback, for a process
started without it, follows the build's state layout: a distribution build
(the host exports ``PRIME_AGENT_DIST_VARIANT``) keeps ``~/.prime/agent-<variant>``,
the official build ``~/.prime/agent``.
"""

from __future__ import annotations

import os
from pathlib import Path

__all__ = ["ENV_AGENT_DIR", "ENV_DIST_VARIANT", "agent_dir"]

ENV_AGENT_DIR = "PRIME_AGENT_CODING_AGENT_DIR"
ENV_DIST_VARIANT = "PRIME_AGENT_DIST_VARIANT"


def _env(name: str) -> str | None:
    # Set-but-empty values behave as unset.
    value = (os.environ.get(name) or "").strip()
    return value or None


def agent_dir() -> Path:
    """The active agent dir (absolute; a relative override resolves against the cwd)."""
    raw = _env(ENV_AGENT_DIR) or _env("PI_CODING_AGENT_DIR")
    if raw is None:
        variant = _env(ENV_DIST_VARIANT)
        raw = str(Path.home() / ".prime" / (f"agent-{variant}" if variant else "agent"))
    return Path(raw).expanduser().resolve()
