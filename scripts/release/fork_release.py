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
  channel   write latest.json + the `stable` pointer from a merged manifest
            (release.yml's "Merge per-target manifests" output shape)
  render    write install.sh / install.ps1 / prime-switch / prime-switch.ps1
            with the fork's URLs, prefix, and variant rendered in

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


def cmd_version(args):
    tag = args.tag
    match = re.fullmatch(r"v([0-9]+\.[0-9]+\.[0-9]+)-([a-z0-9]+)\.([0-9]+)", tag)
    if match is None or match.group(2) != args.suffix:
        sys.exit(f"tag {tag}: a fork release tag is vX.Y.Z-{args.suffix}.N")
    text = pathlib.Path(args.cargo_toml).read_text()
    cargo = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
    if cargo is None or cargo.group(1) != match.group(1):
        found = cargo.group(1) if cargo else "none"
        sys.exit(f"tag {tag}: X.Y.Z must be the workspace version (Cargo.toml: {found})")
    print(tag[1:])


def cmd_channel(args):
    merged = json.loads(pathlib.Path(args.manifest).read_text())
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
    (out / "latest.json").write_text(json.dumps(document, indent=2) + "\n")
    (out / "stable").write_text(f"{version}\n")
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
    if re.fullmatch(r"[A-Za-z0-9._/-]+", args.home_prefix) is None:
        sys.exit(f"invalid home-relative prefix {args.home_prefix!r}")
    base, assets = args.download_base.rstrip("/"), args.assets_url.rstrip("/")
    for url in (base, assets):
        if re.fullmatch(r"[A-Za-z0-9:/._~%-]+", url) is None:
            sys.exit(f"refusing to render the URL {url!r}")
    sh = (root / "install-rust.sh").read_text()
    for pattern, line in (
        (r"^DOWNLOAD_BASE_URL_DEFAULT=.*$", f'DOWNLOAD_BASE_URL_DEFAULT="{base}"'),
        (r'^RELEASE_ASSETS_URL_DEFAULT=""$', f'RELEASE_ASSETS_URL_DEFAULT="{assets}"'),
        (r'^PREFIX_DEFAULT=""$', f'PREFIX_DEFAULT="$HOME/{args.home_prefix}"'),
        (r'^VARIANT_NAME=""$', f'VARIANT_NAME="{args.variant}"'),
    ):
        sh = render_line(sh, pattern, line, "install.sh")
    (out / "install.sh").write_text(sh)
    ps1 = (root / "install.ps1").read_text()
    windows_prefix = args.home_prefix.replace("/", "\\")
    for pattern, line in (
        (r"^\$DownloadBaseUrlDefault = '.*'$", f"$DownloadBaseUrlDefault = '{base}'"),
        (r"^\$ReleaseAssetsUrlDefault = ''$", f"$ReleaseAssetsUrlDefault = '{assets}'"),
        (r"^\$PrefixDefault = ''$", f"$PrefixDefault = '{windows_prefix}'"),
        (r"^\$VariantName = ''$", f"$VariantName = '{args.variant}'"),
    ):
        ps1 = render_line(ps1, pattern, line, "install.ps1")
    (out / "install.ps1").write_text(ps1)
    switch = (root / "scripts/prime-switch").read_text()
    switch = render_line(
        switch, r"^FORK_RELEASES_URL_DEFAULT=.*$", f'FORK_RELEASES_URL_DEFAULT="{base}"', "prime-switch"
    )
    (out / "prime-switch").write_text(switch)
    (out / "prime-switch").chmod(0o755)
    switch_ps1 = (root / "scripts/prime-switch.ps1").read_text()
    switch_ps1 = render_line(
        switch_ps1,
        r"^\$ForkReleasesUrlDefault = '.*'$",
        f"$ForkReleasesUrlDefault = '{base}'",
        "prime-switch.ps1",
    )
    (out / "prime-switch.ps1").write_text(switch_ps1)
    print(f"rendered install.sh, install.ps1, prime-switch, prime-switch.ps1 for {base}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    version = sub.add_parser("version")
    version.add_argument("--tag", required=True)
    version.add_argument("--suffix", required=True)
    version.add_argument("--cargo-toml", required=True)
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
    render.add_argument("--out-dir", required=True)
    args = parser.parse_args()
    {"version": cmd_version, "channel": cmd_channel, "render": cmd_render}[args.command](args)


if __name__ == "__main__":
    main()
