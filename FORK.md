# Prime Agent (drvova build)

This repository (`drvova/prime-agent`) is a fork of Prime Agent with its own
features on top of the official build (for example the native `web.search`
engine, token-exchange provider auth, and the `/usage` page). It ships its own
releases, and it installs **next to** the official build instead of replacing
it. `prime-switch` picks which of the two `prime-agent` runs.

## Install

Install the fork build (macOS, Linux x64/arm64, Git Bash on Windows):

```sh
curl -fsSL https://github.com/drvova/prime-agent/releases/latest/download/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://github.com/drvova/prime-agent/releases/latest/download/install.ps1 | iex
```

These are the official installers (`install-rust.sh`, `install.ps1`) with the
fork's release URLs, prefix and variant filled in at release time. They
install into `~/.local/share/prime-agent-fork` (on Windows
`%USERPROFILE%\.local\share\prime-agent-fork`). They never write to the
official build's `~/.local/bin/prime-agent` or `~/.local/share/prime-agent`.

## Switching with prime-switch

Install the switch script and the build you are missing:

```sh
curl -fsSL https://github.com/drvova/prime-agent/releases/latest/download/prime-switch | sh -s -- install fork
export PATH="$HOME/.local/share/prime-switch/bin:$PATH"   # add this to your shell profile
```

`prime-switch` keeps a small `prime-agent` shim in
`~/.local/share/prime-switch/bin`. That directory has to come **before**
`~/.local/bin` on your PATH. `prime-switch status` tells you when it doesn't.

| Command | What it does |
| --- | --- |
| `prime-switch status` | Shows the active build, plus both builds' versions and launcher paths. |
| `prime-switch fork` / `prime-switch official` | Points the shim at that build. It first stops the other build's daemon with that build's own `prime-agent shutdown --force`. |
| `prime-switch install fork\|official` | Installs a missing build with its own installer: the official one from `app.primeintellect.ai`, the fork from this repository's releases. |
| `prime-switch update` | Runs `prime-agent update` for the active build. |
| `prime-switch uninstall fork` | Removes `~/.local/share/prime-agent-fork`. If the fork was active, the shim goes back to the official build. |

If the other build's daemon has a session in the middle of a turn, the
switch stops and tells you. Wait for the turn to finish, or add `--force`.
Every command is idempotent and prints a `changed:` line for each thing it
did. It needs no sudo. The only things it deletes are its own shim
temp files and the fork prefix.

Windows has `prime-switch.ps1` with the same commands. The first switch puts
`%USERPROFILE%\.local\share\prime-switch\bin` at the front of your user PATH.

```powershell
& ([scriptblock]::Create((irm https://github.com/drvova/prime-agent/releases/latest/download/prime-switch.ps1))) install fork
prime-switch.ps1 fork
```

## What the two builds share

- **Shared:** `~/.prime/agent`, which holds sessions, `auth.json` and
  settings. A session started in one build resumes in the other.
- **Separate daemon:** the fork's launcher pins its own daemon socket
  (`${TMPDIR:-/tmp}/prime-agent-fork-<uid>/daemon.sock`). On Windows it pins
  the pipe `\\.\pipe\prime-agent-fork-daemon`. A running official daemon and
  a fork CLI therefore never talk to each other.
- **Separate kernel venv:** the fork's launcher sets
  `PRIME_AGENT_KERNEL_VENV=<fork prefix>/kernel-venv`. The fork's Python
  runtime differs from the official one (it adds `rlm.web`, for example), and
  a shared venv would be rebuilt on every switch.
- **`auth.json` is safe to share.** The official build reads each entry on
  its own and skips the fork's `token_exchange` entries. It keeps them in the
  file when it writes `auth.json` back.
- **Watch out for `models.json`.** A provider that uses the fork's
  `auth: { "strategy": "token-exchange" }` block has no `apiKey`. The
  official build's `models.json` check rejects such a file, and then **none**
  of your custom models load there. `prime-switch official` warns when this
  applies.
- **Settings:** the official build reads settings field by field. When a
  value is fork-only, such as `webSearchProvider: "auto"`, it falls back to
  its own default for that field.

## Updates

A fork binary has its update source compiled in (the `PRIME_AGENT_DIST_*`
build variables). Its `prime-agent update` and `/update` download this
repository's `install.sh`, read `latest.json` from the latest GitHub Release,
and install into the fork prefix. A fork build never reinstalls the official
build. The official binary updates exactly as before.

The fork publishes only the **stable** channel. `prime-agent update --nightly`
fails on a fork build, because the fork has no `install-beta.sh`.

## Cutting a fork release

Tags look like `vX.Y.Z-drvova.N`. `X.Y.Z` must equal the `version` in
`Cargo.toml`, and `N` counts fork releases on top of that version. The
official `release.yml` never matches these tags (see
`crates/pa-cli/tests/fork_switch_e2e.rs`).

1. Enable GitHub Actions on the fork (repository **Actions** tab, then the
   enable button). Under **Settings → Actions → General → Workflow
   permissions**, the default read permission is enough, because the publish
   job asks for `contents: write` itself.
2. From an up-to-date `main`:

   ```sh
   git tag v0.10.3-drvova.1
   git push origin v0.10.3-drvova.1
   ```

   Or, without pushing a tag: **Actions → fork-release → Run workflow**, on
   `main`, with the tag to create (for example `v0.10.3-drvova.1`). The
   workflow checks the tag is new and matches `Cargo.toml`, builds the
   dispatched `main` commit, and the publish job creates the tag there.
3. `.github/workflows/fork-release.yml` builds linux x64/arm64, macOS
   arm64/x64 and Windows x64. It packages them with the official release
   scripts, merges them with `release.yml`'s own promote steps, and publishes
   the GitHub Release (marked latest). The release carries the tarballs,
   `SHA256SUMS`, `latest.json`, `stable`, `install.sh`, `install.ps1`,
   `prime-switch` and `prime-switch.ps1`.
4. Check it: `curl -fsSL .../releases/latest/download/stable` prints the
   version, the install one-liner works, and `prime-agent --version` prints
   `X.Y.Z-drvova.N`.

The packaged version comes from the tag. `Cargo.toml` stays at the upstream
version, and the archive's `package.json` carries `X.Y.Z-drvova.N`, which is
what `prime-agent --version` prints.
