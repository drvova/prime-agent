#!/usr/bin/env python3
"""Exercise fork_release.py: the automatic release plan (next-tag,
should-release) and render against a Windows-style (CRLF) checkout."""

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


def run_script(*args, stdin=''):
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        input=stdin, capture_output=True, text=True, check=False,
    )


def next_tag(version, tags, *extra):
    with tempfile.TemporaryDirectory() as scratch:
        cargo = Path(scratch) / 'Cargo.toml'
        cargo.write_text(f'[workspace.package]\nversion = "{version}"\n')
        result = run_script('next-tag', '--suffix', 'drvova', '--cargo-toml', str(cargo), *extra,
                            stdin='\n'.join(tags) + '\n')
    assert result.returncode == 0, result.stderr
    return result.stdout


def should_release(*paths):
    result = run_script('should-release', stdin=''.join(f'{path}\n' for path in paths))
    assert result.returncode == 0, result.stderr
    return result.stdout


# The tags a fork clone carries: official releases synced from upstream, the
# fork's own, and an unrelated suffix.
TAGS = ('v0.10.2', 'v0.10.3', 'v0.10.4-beta.1', 'v0.10.2-drvova.7', 'v0.10.3-drvova.1',
        'v0.10.3-drvova.4', 'v0.10.3-other.9', 'v0.10.3-drvova.x')


class ForkReleasePlanTest(unittest.TestCase):
    def test_the_first_fork_release_of_a_version_is_n_1(self):
        self.assertEqual(next_tag('0.10.4', TAGS), 'tag=v0.10.4-drvova.1\nprevious=v0.10.3-drvova.4\n')

    def test_the_next_tag_increments_past_gaps_and_ignores_other_tags(self):
        self.assertEqual(next_tag('0.10.3', TAGS), 'tag=v0.10.3-drvova.5\nprevious=v0.10.3-drvova.4\n')

    def test_a_repository_without_fork_releases_starts_at_1_with_no_previous(self):
        self.assertEqual(next_tag('0.10.3', ('v0.10.3',)), 'tag=v0.10.3-drvova.1\nprevious=\n')

    def test_a_pushed_tag_keeps_its_name_and_finds_the_release_before_it(self):
        self.assertEqual(next_tag('0.10.3', TAGS, '--release-tag', 'v0.10.3-drvova.4'),
                         'tag=v0.10.3-drvova.4\nprevious=v0.10.3-drvova.1\n')

    def test_docs_changelog_and_other_workflows_cut_no_release(self):
        self.assertEqual(should_release('FORK.md', 'docs/guide.md', '.changes/x.md',
                                        '.github/workflows/ci.yml', 'scripts/ci_test_shard.py'),
                         'release=false\n')

    def test_a_crate_change_cuts_a_release(self):
        self.assertEqual(should_release('FORK.md', 'crates/pa-core/src/lib.rs'), 'release=true\n')

    def test_test_only_changes_cut_no_release_until_source_changes_too(self):
        tests = ('crates/pa-cli/tests/fork_switch_e2e.rs', 'crates/pa-core/src/kernel/tests/mod.rs',
                 'crates/pa-core/src/session_engine/context_thinning/tests.rs',
                 'crates/pa-tui/src/daemon_client/wire2_tests.rs', 'skills/goal/test/test_goal.py')
        self.assertEqual(should_release(*tests), 'release=false\n')
        self.assertEqual(should_release(*tests, 'crates/pa-core/src/lib.rs'), 'release=true\n')

    def test_the_release_pipeline_and_installers_cut_a_release(self):
        for path in ('.github/workflows/fork-release.yml', 'install.ps1', 'scripts/prime-switch.ps1',
                     'scripts/release/fork_release.py', 'Cargo.lock'):
            self.assertEqual(should_release(path), 'release=true\n', path)


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
