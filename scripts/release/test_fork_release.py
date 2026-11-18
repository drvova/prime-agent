#!/usr/bin/env python3
"""Exercise fork_release.py render against a Windows-style (CRLF) checkout."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/release/fork_release.py'
SOURCES = ('install-rust.sh', 'install.ps1', 'scripts/prime-switch', 'scripts/prime-switch.ps1')
RENDERED = ('install.sh', 'install.ps1', 'prime-switch', 'prime-switch.ps1')
RENDER_ARGS = (
    '--download-base', 'https://example.invalid/latest/download',
    '--assets-url', 'https://example.invalid/download',
    '--home-prefix', '.local/share/prime-agent-fork',
    '--variant', 'fork',
    '--command', 'prime',
)


def render(repo_root, out_dir):
    return subprocess.run(
        [sys.executable, str(SCRIPT), 'render', '--repo-root', str(repo_root), *RENDER_ARGS,
         '--out-dir', str(out_dir)],
        capture_output=True, text=True, check=False,
    )


class ForkRenderTest(unittest.TestCase):
    def test_a_crlf_checkout_renders_the_same_lf_installers(self):
        # A Windows runner's checkout (core.autocrlf) hands the scripts over
        # with CRLF; the render must still find its anchors and ship LF.
        with tempfile.TemporaryDirectory() as scratch:
            scratch = Path(scratch)
            crlf_root = scratch / 'crlf'
            for name in SOURCES:
                target = crlf_root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes((ROOT / name).read_bytes().replace(b'\r\n', b'\n').replace(b'\n', b'\r\n'))
            from_lf = render(ROOT, scratch / 'from-lf')
            self.assertEqual(from_lf.returncode, 0, from_lf.stderr)
            from_crlf = render(crlf_root, scratch / 'from-crlf')
            self.assertEqual(from_crlf.returncode, 0, from_crlf.stderr)
            for name in RENDERED:
                rendered = (scratch / 'from-crlf' / name).read_bytes()
                self.assertNotIn(b'\r\n', rendered, name)
                self.assertEqual(rendered, (scratch / 'from-lf' / name).read_bytes(), name)


if __name__ == '__main__':
    unittest.main()
