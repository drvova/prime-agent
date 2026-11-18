#!/usr/bin/env python3
"""The fork release's publication steps (`.github/workflows/fork-release.yml`).

A fork build publishes to its own GitHub Releases instead of the official
R2 channel. GitHub serves a release's assets at
`<repo>/releases/download/<tag>/<file>` and the newest release's at
`<repo>/releases/latest/download/<file>`, so the fork channel is:

  <repo>/releases/latest/download/stable        the version pointer
  <repo>/releases/latest/download/latest.json   the channel manifest
  <repo>/releases/latest/download/install.sh    the rendered installers,
  <repo>/releases/latest/download/install.ps1   and prime-switch(.ps1)
  <repo>/releases/download/v<version>/<file>    the tarballs + SHA256SUMS

Subcommands:

  version   validate a fork tag (vX.Y.Z-<suffix>.N, X.Y.Z = Cargo.toml) and
            print the bare version the build stamps
  next-tag  read the repository's tags on stdin and print the release tag
            (the next vX.Y.Z-<suffix>.N for Cargo.toml's version, or the
            given --release-tag) and the fork release before it, as
            GITHUB_OUTPUT lines
  should-release
            read changed paths on stdin (the diff since the previous fork
            release) and print release=true when any of them is shipped code
  channel   write latest.json + the `stable` pointer from a merged manifest
            (release.yml's "Merge per-target manifests" output shape)
  render    write install.sh / install.ps1 / prime-switch / prime-switch.ps1
            with the fork's URLs, prefix, variant, and command rendered in

The installers are install-rust.sh and install.ps1 themselves: only their
publish-rendered default lines change (the same line-anchored render
release.yml's promote job applies for the official channel), so there is
no second installer to maintain. The end-to-end test
`crates/pa-cli/tests/fork_switch_e2e.rs` runs `channel` and `render` to
build the fake release it installs from.
"""
import argparse
import json
import pathlib
import re
import sys

# The platforms the update reader knows (pa-core::update::install
# KNOWN_PLATFORMS; release.yml's channel-manifest step requires all five).
PLATFORMS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64")
# release.yml's v1 `binaries` list (pre-Windows readers).
V1_PLATFORMS = {"darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"}


def read_utf8(path):
    """Read a release text file as UTF-8 whatever the platform's locale
    (a Windows runner defaults to cp1252), with CRLF folded to LF: a Windows
    checkout (core.autocrlf) hands the scripts over with CRLF, the render's
    line anchors expect LF, and the shell installers must ship with LF."""
    return pathlib.Path(path).read_bytes().decode("utf-8").replace("\r\n", "\n")


def write_utf8(path, text):
    """Write a release text file as UTF-8 bytes with the newlines as given
    (`write_text` would turn LF into CRLF on Windows and break the shell
    scripts)."""
    pathlib.Path(path).write_bytes(text.encode("utf-8"))


FORK_TAG = r"v([0-9]+)\.([0-9]+)\.([0-9]+)-([a-z0-9]+)\.([0-9]+)"

# What a fork release ships: the binary's sources (vendor/ is a patched
# dependency, install-rust.sh is compiled in), the payload
# assemble_artifacts.py stages (prime-agent-runtime/, skills/), the published
# installers and switchers, the release scripts, and this pipeline itself.
# A commit range touching none of them (docs, .changes/, other workflows)
# cuts no automatic release.
SHIPPED_PREFIXES = (
    "crates/",
    "vendor/",
    "prime-agent-runtime/",
    "skills/",
    "scripts/release/",
    "scripts/prime-switch",
)
SHIPPED_FILES = (
    "Cargo.toml",
    "Cargo.lock",
    "install-rust.sh",
    "install.ps1",
    ".github/workflows/fork-release.yml",
)


def workspace_version(cargo_toml):
    text = read_utf8(pathlib.Path(cargo_toml))
    cargo = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
    if cargo is None:
        sys.exit(f"{cargo_toml}: no workspace version")
    return cargo.group(1)


def fork_tag_key(tag, suffix):
    """The (major, minor, patch, N) order of a fork tag, or None for any
    other tag (official releases, other suffixes)."""
    match = re.fullmatch(FORK_TAG, tag)
    if match is None or match.group(4) != suffix:
        return None
    return tuple(int(match.group(index)) for index in (1, 2, 3, 5))


def plan_tag(tags, suffix, version, release_tag=None):
    """The release tag and the fork release before it ("" for the first).

    Without `release_tag`, the next tag for `version`: N is one past the
    highest N already tagged on that version (gaps are never refilled), or 1
    for a version with no fork release yet."""
    keyed = sorted(
        (key, tag) for tag in tags if (key := fork_tag_key(tag, suffix)) is not None
    )
    if release_tag is None:
        major, minor, patch = (int(part) for part in version.split("."))
        same_version = [key[3] for key, _tag in keyed if key[:3] == (major, minor, patch)]
        release_tag = f"v{version}-{suffix}.{max(same_version, default=0) + 1}"
    release_key = fork_tag_key(release_tag, suffix)
    if release_key is None:
        sys.exit(f"tag {release_tag}: a fork release tag is vX.Y.Z-{suffix}.N")
    earlier = [tag for key, tag in keyed if key < release_key]
    return release_tag, earlier[-1] if earlier else ""


def shipped_changes(paths):
    """The changed paths a fork release ships, in input order."""
    return [
        path
        for path in paths
        if path.startswith(SHIPPED_PREFIXES) or path in SHIPPED_FILES
    ]


def cmd_version(args):
    tag = args.tag
    match = re.fullmatch(FORK_TAG, tag)
    if match is None or match.group(4) != args.suffix:
        sys.exit(f"tag {tag}: a fork release tag is vX.Y.Z-{args.suffix}.N")
    found = workspace_version(args.cargo_toml)
    if found != ".".join(match.group(index) for index in (1, 2, 3)):
        sys.exit(f"tag {tag}: X.Y.Z must be the workspace version (Cargo.toml: {found})")
    print(tag[1:])


def cmd_next_tag(args):
    tags = sys.stdin.read().split()
    tag, previous = plan_tag(
        tags, args.suffix, workspace_version(args.cargo_toml), args.release_tag
    )
    print(f"tag={tag}")
    print(f"previous={previous}")


def cmd_should_release(_args):
    paths = [line.strip() for line in sys.stdin.read().splitlines() if line.strip()]
    shipped = shipped_changes(paths)
    if shipped:
        print(f"{len(shipped)} of {len(paths)} changed paths are shipped code, e.g. {shipped[0]}",
              file=sys.stderr)
    else:
        print(f"none of the {len(paths)} changed paths is shipped code: no release",
              file=sys.stderr)
    print(f"release={'true' if shipped else 'false'}")


def cmd_channel(args):
    merged = json.loads(read_utf8(pathlib.Path(args.manifest)))
    version = merged["version"].lstrip("v")
    if version != args.version.lstrip("v"):
        sys.exit(f"merged manifest version {merged['version']} != release {args.version}")
    required = set(args.platforms.split(",")) if args.platforms else set(PLATFORMS)
    rows, seen = [], set()
    for binary in merged["binaries"]:
        platform, file_name, sha = binary["platform"], binary["file"], binary["sha256"]
        if platform not in PLATFORMS:
            continue
        expected = f"prime-agent-{version}-{platform}.tar.gz"
        if file_name != expected or re.fullmatch(r"[0-9a-f]{64}", sha) is None:
            sys.exit(f"artifact row {binary}: file must be {expected} with a 64-hex sha256")
        if platform in seen:
            sys.exit(f"duplicate artifact row for platform {platform}")
        seen.add(platform)
        rows.append({"platform": platform, "file": file_name, "sha256": sha})
    missing = required - seen
    if missing:
        sys.exit(f"the release is missing artifact rows for {sorted(missing)}")
    rows.sort(key=lambda row: row["platform"])
    document = {"version": f"v{version}", "package": "prime-agent"}
    v1_rows = [row for row in rows if row["platform"] in V1_PLATFORMS]
    if v1_rows:
        document["binaries"] = v1_rows
    document["binaries_v2"] = rows
    out = pathlib.Path(args.out_dir)
    out.mkdir(parents=True, exist_ok=True)
    write_utf8(out / "latest.json", json.dumps(document, indent=2) + "\n")
    write_utf8(out / "stable", f"{version}\n")
    print(f"channel: latest.json + stable at {version}, {len(rows)} artifacts")


def render_line(text, pattern, replacement, name):
    """Replace the one line `pattern` anchors; refuse unless it occurs exactly once."""
    rendered, count = re.subn(pattern, lambda _match: replacement, text, flags=re.M)
    if count != 1:
        sys.exit(f"render {name}: expected exactly one line matching {pattern!r}, found {count}")
    return rendered


def cmd_render(args):
    root = pathlib.Path(args.repo_root)
    out = pathlib.Path(args.out_dir)
    out.mkdir(parents=True, exist_ok=True)
    if re.fullmatch(r"[a-z0-9-]+", args.variant) is None:
        sys.exit(f"invalid variant {args.variant!r}")
    if re.fullmatch(r"[a-z0-9-]+", args.command_name) is None or args.command_name == "prime-agent":
        sys.exit(f"invalid command {args.command_name!r} (the distribution's own command, never prime-agent)")
    if re.fullmatch(r"[A-Za-z0-9._/-]+", args.home_prefix) is None:
        sys.exit(f"invalid home-relative prefix {args.home_prefix!r}")
    base, assets = args.download_base.rstrip("/"), args.assets_url.rstrip("/")
    for url in (base, assets):
        if re.fullmatch(r"[A-Za-z0-9:/._~%-]+", url) is None:
            sys.exit(f"refusing to render the URL {url!r}")
    sh = read_utf8(root / "install-rust.sh")
    for pattern, line in (
        (r"^DOWNLOAD_BASE_URL_DEFAULT=.*$", f'DOWNLOAD_BASE_URL_DEFAULT="{base}"'),
        (r'^RELEASE_ASSETS_URL_DEFAULT=""$', f'RELEASE_ASSETS_URL_DEFAULT="{assets}"'),
        (r'^PREFIX_DEFAULT=""$', f'PREFIX_DEFAULT="$HOME/{args.home_prefix}"'),
        (r'^VARIANT_NAME=""$', f'VARIANT_NAME="{args.variant}"'),
        (r'^COMMAND_NAME=""$', f'COMMAND_NAME="{args.command_name}"'),
    ):
        sh = render_line(sh, pattern, line, "install.sh")
    write_utf8(out / "install.sh", sh)
    ps1 = read_utf8(root / "install.ps1")
    windows_prefix = args.home_prefix.replace("/", "\\")
    for pattern, line in (
        (r"^\$DownloadBaseUrlDefault = '.*'$", f"$DownloadBaseUrlDefault = '{base}'"),
        (r"^\$ReleaseAssetsUrlDefault = ''$", f"$ReleaseAssetsUrlDefault = '{assets}'"),
        (r"^\$PrefixDefault = ''$", f"$PrefixDefault = '{windows_prefix}'"),
        (r"^\$VariantName = ''$", f"$VariantName = '{args.variant}'"),
        (r"^\$CommandName = ''$", f"$CommandName = '{args.command_name}'"),
    ):
        ps1 = render_line(ps1, pattern, line, "install.ps1")
    write_utf8(out / "install.ps1", ps1)
    switch = read_utf8(root / "scripts/prime-switch")
    switch = render_line(
        switch, r"^FORK_RELEASES_URL_DEFAULT=.*$", f'FORK_RELEASES_URL_DEFAULT="{base}"', "prime-switch"
    )
    write_utf8(out / "prime-switch", switch)
    (out / "prime-switch").chmod(0o755)
    switch_ps1 = read_utf8(root / "scripts/prime-switch.ps1")
    switch_ps1 = render_line(
        switch_ps1,
        r"^\$ForkReleasesUrlDefault = '.*'$",
        f"$ForkReleasesUrlDefault = '{base}'",
        "prime-switch.ps1",
    )
    write_utf8(out / "prime-switch.ps1", switch_ps1)
    print(f"rendered install.sh, install.ps1, prime-switch, prime-switch.ps1 for {base}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    version = sub.add_parser("version")
    version.add_argument("--tag", required=True)
    version.add_argument("--suffix", required=True)
    version.add_argument("--cargo-toml", required=True)
    next_tag = sub.add_parser("next-tag")
    next_tag.add_argument("--suffix", required=True)
    next_tag.add_argument("--cargo-toml", required=True)
    next_tag.add_argument("--release-tag",
                          help="the release's own tag (a pushed or dispatched one); default: the next one")
    sub.add_parser("should-release")
    channel = sub.add_parser("channel")
    channel.add_argument("--manifest", required=True)
    channel.add_argument("--version", required=True)
    channel.add_argument("--out-dir", required=True)
    channel.add_argument("--platforms", default="",
                         help="comma-separated platforms the release must carry (default: all five)")
    render = sub.add_parser("render")
    render.add_argument("--repo-root", required=True)
    render.add_argument("--download-base", required=True)
    render.add_argument("--assets-url", required=True)
    render.add_argument("--home-prefix", required=True)
    render.add_argument("--variant", required=True)
    render.add_argument("--command", dest="command_name", required=True,
                        help="the command the installers create instead of prime-agent (e.g. prime)")
    render.add_argument("--out-dir", required=True)
    args = parser.parse_args()
    {
        "version": cmd_version,
        "next-tag": cmd_next_tag,
        "should-release": cmd_should_release,
        "channel": cmd_channel,
        "render": cmd_render,
    }[args.command](args)


if __name__ == "__main__":
    main()
