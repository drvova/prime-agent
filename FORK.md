# Prime Agent (drvova build)

This repository (`drvova/prime-agent`) is a fork of Prime Agent with its own
features on top of the official build (for example the native `web.search`
engine, token-exchange provider auth, and the `/usage` page). It ships its own
releases, and it installs **next to** the official build instead of replacing
it. The two builds have different commands, so you can't mix them up:

| Build | Command | Launcher |
| --- | --- | --- |
| Prime Agent (drvova build) | `prime` | `~/.local/share/prime-agent-fork/bin/prime` (plus `~/.local/bin/prime`, see below) |
| Official Prime Agent | `prime-agent` | `~/.local/bin/prime-agent` |

`prime` is the full CLI: `prime`, `prime --version`, `prime update`, and every
`prime-agent` subcommand work the same way.

## Install

Install the fork build (Linux x64/arm64, Git Bash on Windows). macOS builds are
not published, because the release workflow no longer builds them:

```sh
curl -fsSL https://github.com/drvova/prime-agent/releases/latest/download/install.sh | sh
```

Windows x64 (PowerShell):

```powershell
irm https://github.com/drvova/prime-agent/releases/latest/download/install.ps1 | iex
```

These are the official installers (`install-rust.sh`, `install.ps1`) with the
fork's release URLs, prefix, variant and command filled in at release time.
They install into `~/.local/share/prime-agent-fork` (on Windows
`%USERPROFILE%\.local\share\prime-agent-fork`) and create the `prime`
launcher there. They create no `prime-agent` launcher and never write to the
official build's `~/.local/bin/prime-agent` or `~/.local/share/prime-agent`.
A fork installed before it had its own command still has a
`bin/prime-agent` launcher in the fork prefix. The next install or
`prime-agent update` of the fork replaces it with `bin/prime`.

## The `prime` name and Prime Intellect's `prime` CLI

Prime Intellect's own CLI (the `prime` pip package) is also run as `prime`,
and pip often puts it in `~/.local/bin`. The fork installer **never replaces
or removes a `prime` it did not create**:

- **`~/.local/bin/prime` is free** (or is the fork's own earlier copy): the
  installer writes a small `prime` there that runs the fork launcher. Its
  second line is `# prime - public command written by install-rust.sh`.
  `~/.local/bin` is usually on PATH already, so `prime` works right away.
- **Something else is at `~/.local/bin/prime`**: the installer leaves it
  byte for byte as it is. The fork's `prime` stays only in
  `~/.local/share/prime-agent-fork/bin`, and the installer prints a notice.
  Whichever `prime` comes first on PATH runs. To run the fork as `prime`, put
  its directory first:

  ```sh
  export PATH="$HOME/.local/share/prime-agent-fork/bin:$PATH"   # add this to your shell profile
  ```

  The Prime Intellect CLI is then still available by its full path
  (`~/.local/bin/prime`). To keep it as `prime`, don't reorder PATH. Run the
  fork as `~/.local/share/prime-agent-fork/bin/prime` or with a shell alias.
- **Another `prime` comes first on PATH** (a virtualenv, for example): the
  installer names it and the directory to move ahead of it.

On Windows the installer adds `%USERPROFILE%\.local\share\prime-agent-fork\bin`
(with `prime.cmd`) to the end of your user PATH. A `prime.exe` that pip put
earlier on PATH keeps winning, and the installer warns about it and says
which directory to move first. It never writes to `%USERPROFILE%\.local\bin`.

`prime-switch status` shows which `prime` and `prime-agent` your PATH runs.

## prime-switch

Because the commands differ, nothing needs switching any more: `prime` runs
the fork and `prime-agent` runs the official build, both at once.
`prime-switch` stays as a helper that installs, updates and removes the two
builds:

```sh
curl -fsSL https://github.com/drvova/prime-agent/releases/latest/download/prime-switch | sh -s -- install fork
export PATH="$HOME/.local/share/prime-switch/bin:$PATH"   # to run prime-switch itself later
```

| Command | What it does |
| --- | --- |
| `prime-switch status` | Shows both builds' versions, commands and launcher paths, plus which `prime` / `prime-agent` your PATH runs. |
| `prime-switch install fork\|official` | Installs a missing build with its own installer: the official one from `app.primeintellect.ai`, the fork from this repository's releases. |
| `prime-switch update [fork\|official]` | Runs one build's own `update` (`prime update` or `prime-agent update`), or every installed build's when no build is named. |
| `prime-switch uninstall fork` | Stops the fork daemon, then removes `~/.local/share/prime-agent-fork` and the fork's own `~/.local/bin/prime`. A `prime` the fork did not write stays. |
| `prime-switch fork` / `prime-switch official` | Kept for old scripts. They only remove the old shim (see below) and say there is nothing to switch. |

Earlier versions kept a `prime-agent` shim in `~/.local/share/prime-switch/bin`
that ran the "active" build. Any `prime-switch` command now removes that shim
(only the file it wrote, checked by its marker), so `prime-agent` is the
official build again.

If the fork daemon has a session in the middle of a turn, `uninstall fork`
stops and tells you. Wait for the turn to finish, or add `--force`. Every
command is idempotent and prints a `changed:` line for each thing it did. It
needs no sudo. The only things it deletes are its old shim, the fork prefix
and the fork's own `~/.local/bin/prime` (only when that file carries the fork
installer's marker and runs that fork prefix).

Windows has `prime-switch.ps1` with the same commands:

```powershell
& ([scriptblock]::Create((irm https://github.com/drvova/prime-agent/releases/latest/download/prime-switch.ps1))) install fork
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
  a shared venv would be rebuilt every time you alternated builds.
- **`auth.json` is safe to share.** The official build reads each entry on
  its own and skips the fork's `token_exchange` entries. It keeps them in the
  file when it writes `auth.json` back.
- **Watch out for `models.json`.** A provider that uses the fork's
  `auth: { "strategy": "token-exchange" }` block has no `apiKey`. The
  official build's `models.json` check rejects such a file, and then **none**
  of your custom models load there. `prime-switch install official` warns
  when this applies.
- **Settings:** the official build reads settings field by field. When a
  value is fork-only, such as `webSearchProvider: "auto"`, it falls back to
  its own default for that field.

## Updates

A fork binary has its update source and its command compiled in (the
`PRIME_AGENT_DIST_*` build variables, `PRIME_AGENT_DIST_COMMAND=prime` among
them). Its `prime update` and `/update` download this repository's
`install.sh`, read `latest.json` from the latest GitHub Release, and install
into the fork prefix. A fork build never reinstalls the official build. The
official binary updates exactly as before. `prime update --rollback` and
`--archive` run the installer bundled into the binary, and the release
workflow bundles the fork-rendered one, so a rollback keeps the `prime`
launcher and its pins.

The fork's help and update messages say `prime`. Some messages from deeper
subcommands still say `prime-agent`; read them as `prime` on a fork build.

The fork publishes only the **stable** channel. `prime update --nightly`
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
3. `.github/workflows/fork-release.yml` builds linux x64/arm64 and Windows
   x64 (no macOS: GitHub never assigned this repository a macOS runner). It packages them with the official release
   scripts, merges them with `release.yml`'s own promote steps, and publishes
   the GitHub Release (marked latest). The release carries the tarballs,
   `SHA256SUMS`, `latest.json`, `stable`, `install.sh`, `install.ps1`,
   `prime-switch` and `prime-switch.ps1`.
4. Check it: `curl -fsSL .../releases/latest/download/stable` prints the
   version, the install one-liner works, and `prime --version` prints
   `X.Y.Z-drvova.N`.

The packaged version comes from the tag. `Cargo.toml` stays at the upstream
version, and the archive's `package.json` carries `X.Y.Z-drvova.N`, which is
what `prime --version` prints.
