from __future__ import annotations

import asyncio
import importlib.util
import tempfile
import unittest
from pathlib import Path

SKILL = Path(__file__).parents[2] / "skills/edit/src/edit/__init__.py"


def _load_edit():
    spec = importlib.util.spec_from_file_location("edit_skill_under_test", SKILL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class EditMissTest(unittest.TestCase):
    def test_a_miss_says_where_old_str_diverges(self) -> None:
        # The error names the split, both sides there, and the whitespace
        # cause, so the next edit fixes old_str without re-reading the file.
        edit = _load_edit()
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp, "f.py")
            path.write_text("def f():\n    return 1\n", encoding="utf-8")
            with self.assertRaises(ValueError) as caught:
                asyncio.run(edit.run(str(path), "def f():\n\treturn 1\n", "x"))
            self.assertEqual(
                str(caught.exception),
                f"string not found in {path}: old_str matches its first 9 of 19 characters "
                "and diverges at line 2, column 1:\n"
                "  file continues    '    return 1\\n'\n"
                "  old_str continues '\\treturn 1\\n'\n"
                "hint: whitespace differs at the split (indentation, tabs vs spaces, or trailing spaces)",
            )
            self.assertEqual(path.read_text(encoding="utf-8"), "def f():\n    return 1\n")


if __name__ == "__main__":
    unittest.main()
