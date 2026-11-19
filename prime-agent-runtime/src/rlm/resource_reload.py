"""Kernel client for re-reading the session's skills, prompts and context files.

``await rlm.reload()`` does what the user's ``/reload`` does, from the REPL: the
host discovers the skills, prompt templates and ``AGENTS.md`` context files
again, rebuilds the system prompt's dynamic tail from the next model call, and
installs any new Python skill into the kernel venv. This client then binds each
new Python skill into the REPL namespace, so a skill written with skill-creator
is callable at once, without the user.
"""

from __future__ import annotations

import importlib
import importlib.util
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from . import host_request

__all__ = ["ReloadResult", "reload"]


@dataclass(frozen=True)
class ReloadResult:
    """What a reload changed.

    ``python_skills_bound`` lists the import names now callable in the REPL;
    ``unbound`` maps a new Python skill that could not be bound to why.
    """

    skills_added: list[str]
    skills_removed: list[str]
    skills: int
    prompt_templates: int
    context_files: list[str]
    system_prompt_changed: bool
    python_skills_bound: list[str]
    unbound: dict[str, str] = field(default_factory=dict)
    diagnostics: list[str] = field(default_factory=list)

    def __str__(self) -> str:
        lines = [
            f"reloaded: {self.skills} skills, {self.prompt_templates} prompt templates, "
            f"{len(self.context_files)} context files"
            + ("; system prompt updated for the next call" if self.system_prompt_changed else "; system prompt unchanged")
        ]
        if self.skills_added:
            lines.append(f"added: {', '.join(self.skills_added)}")
        if self.skills_removed:
            lines.append(f"removed: {', '.join(self.skills_removed)}")
        if self.python_skills_bound:
            lines.append(f"callable now: {', '.join(self.python_skills_bound)}")
        for name, error in self.unbound.items():
            lines.append(f"not bound: {name}: {error}")
        lines.extend(f"warning: {message}" for message in self.diagnostics)
        return "\n".join(lines)


def _bind(skill: dict[str, Any], namespace: dict[str, Any]) -> str | None:
    """Bind one new Python skill under its import name; the error when it cannot be."""
    name = str(skill.get("importName", ""))
    if not name.isidentifier():
        return f"invalid import name {name!r}"
    if importlib.util.find_spec(name) is None:
        # An editable install's path hook loads only at interpreter start: reach the
        # src layout directly for this kernel (a restart binds it from the venv).
        source = Path(str(skill.get("packagePath", ""))) / "src"
        if source.is_dir() and str(source) not in sys.path:
            sys.path.append(str(source))
            importlib.invalidate_caches()
    if importlib.util.find_spec(name) is None:
        return f"no module named {name!r}"
    lazy = namespace.get("_PrimeAgentLazySkillModule")
    namespace[name] = lazy(name) if lazy is not None else importlib.import_module(name)
    return None


async def reload() -> ReloadResult:
    """Re-read the skills, prompt templates and context files (see the module docs)."""
    reply = await host_request("rlm.reload", {})
    if not isinstance(reply, dict):
        raise RuntimeError(f"rlm.reload returned an unexpected result: {reply!r}")
    namespace = sys.modules["__main__"].__dict__
    bound: list[str] = []
    unbound: dict[str, str] = {}
    for skill in reply.get("pythonSkills", []):
        if not isinstance(skill, dict):
            continue
        error = _bind(skill, namespace)
        if error is None:
            bound.append(str(skill["importName"]))
        else:
            unbound[str(skill.get("importName", ""))] = error
    return ReloadResult(
        skills_added=list(reply.get("skillsAdded", [])),
        skills_removed=list(reply.get("skillsRemoved", [])),
        skills=int(reply.get("skills", 0)),
        prompt_templates=int(reply.get("promptTemplates", 0)),
        context_files=list(reply.get("contextFiles", [])),
        system_prompt_changed=bool(reply.get("systemPromptChanged")),
        python_skills_bound=bound,
        unbound=unbound,
        diagnostics=list(reply.get("diagnostics", [])),
    )
