"""Every `rlm.*` name a capability manifest documents resolves on the kernel's `rlm` object, and
every `<bound>.*` name resolves on the module the bootstrap binds as `<bound>`.

The kernel binds `rlm` to `rlm.rlm` (the namespace), not to the package, so a documented
`rlm.<module>.<fn>` only works when the namespace exposes that module. Importing the
submodule (as the runtime-ready probe does) does not prove it. A bound module
(`import rlm.frida as frida`) documents `frida.<fn>`, which must be an attribute of that module.
"""

from __future__ import annotations

import importlib
import tomllib
import unittest
from pathlib import Path

import rlm

CAPABILITIES = Path(__file__).resolve().parents[1] / "capabilities"


def documented_rlm_names() -> list[str]:
    names: set[str] = set()
    for manifest in CAPABILITIES.glob("*.toml"):
        with manifest.open("rb") as handle:
            for name in tomllib.load(handle).get("documented", []):
                if name.startswith("rlm."):
                    names.add(name)
    return sorted(names)


def unresolved(names: list[str]) -> list[str]:
    missing = []
    for name in names:
        target: object = rlm.rlm
        try:
            for part in name.split(".")[1:]:
                target = getattr(target, part)
        except AttributeError:
            missing.append(name)
    return missing


def unresolved_bound_names() -> list[str]:
    missing = []
    for manifest in CAPABILITIES.glob("*.toml"):
        with manifest.open("rb") as handle:
            parsed = tomllib.load(handle)
        bound = parsed.get("bound_module")
        if bound is None:
            continue
        module = importlib.import_module(parsed["module"])
        for name in parsed.get("documented", []):
            head, _, attribute = name.partition(".")
            if head == bound and not callable(getattr(module, attribute, None)):
                missing.append(name)
    return sorted(missing)


class DocumentedRlmNamesTest(unittest.TestCase):
    def test_every_documented_rlm_name_resolves_on_the_kernel_namespace(self) -> None:
        names = documented_rlm_names()
        self.assertIn("rlm.cells.get", names)
        self.assertEqual(unresolved(names), [])

    def test_every_documented_bound_module_name_is_a_callable_of_that_module(self) -> None:
        self.assertEqual(unresolved_bound_names(), [])


if __name__ == "__main__":
    unittest.main()
