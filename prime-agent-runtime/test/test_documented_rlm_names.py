"""Every `rlm.*` name a capability manifest documents resolves on the kernel's `rlm` object.

The kernel binds `rlm` to `rlm.rlm` (the namespace), not to the package, so a documented
`rlm.<module>.<fn>` only works when the namespace exposes that module. Importing the
submodule (as the runtime-ready probe does) does not prove it.
"""

from __future__ import annotations

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


class DocumentedRlmNamesTest(unittest.TestCase):
    def test_every_documented_rlm_name_resolves_on_the_kernel_namespace(self) -> None:
        names = documented_rlm_names()
        self.assertIn("rlm.cells.get", names)
        self.assertEqual(unresolved(names), [])


if __name__ == "__main__":
    unittest.main()
