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
Unlike the official installer, they don't take over from the TypeScript
Prime Agent: they never stop its daemon (or the official build's) and never
uninstall its npm package. The only daemon they stop is the fork's own.
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

## Where each build keeps its data

- **The fork has its own data directory: `~/.prime/agent-fork`.** It holds
  the fork's sessions, settings, `auth.json`, `models.json`, skills, logs,
  session leases and daemon state. The official build (and the TypeScript
  Prime Agent) keep theirs in `~/.prime/agent`. So `prime --continue`,
  `prime --resume` and `/resume` list only the fork's own chats, and the two
  builds never open, lease or rewrite each other's session files.
  `PRIME_AGENT_CODING_AGENT_DIR` still overrides the location for either
  build.
- **The first run imports your configuration once.** When
  `~/.prime/agent-fork` doesn't exist yet, the fork copies these from
  `~/.prime/agent`: `settings.json`, `auth.json`, `models.json`,
  `keybindings.json`, `mcp-services.json`, `mcp-connections.json`, the
  global `AGENTS.md`/`CLAUDE.md`, `SYSTEM.md` and `APPEND_SYSTEM.md`, and the
  `skills/`, `prompts/` and `themes/` folders. You stay logged in, and your
  MCP servers, custom models and system prompts come along. An entry over
  the import's 32 MB budget is skipped and named. This copy is synchronous
  and small.
- **Your chats follow once, in the background.** The first time the fork's
  daemon starts, it copies the official build's history into
  `~/.prime/agent-fork`: the chats (`sessions/`, `sessions-archive/`), their
  subagent runs and RLM registries (`session-artifacts/`), and the global
  harness memory (`harness/`, the same format the official build reads). The
  copy runs off the UI path: the chat shows "Importing N chats from Prime
  Agent…" when it starts and a second notice when it's done, and `/resume`
  then lists them (an open `/resume` list reloads on its own). The import:
  - **copies, never moves or links.** The official build and the TypeScript
    Prime Agent keep using `~/.prime/agent`. Where the filesystem supports
    copy-on-write clones (APFS, Btrfs, XFS), the copy is a clone; elsewhere
    it's a real copy. It never hard-links, so later appends on one side never
    show up on the other. A JSON file that names a chat or subagent run by its
    absolute path under `~/.prime/agent` is rewritten to name the copy, so the
    fork never opens a file in the official directory.
  - **merges and never overwrites.** A file the fork already has under the same
    name stays as it is, and the done notice counts it. That also covers
    users of the first fork release, whose `~/.prime/agent-fork` already
    exists.
  - **fills in missing configuration.** Any of the configuration entries above
    that `~/.prime/agent-fork` lacks but `~/.prime/agent` has (for example an
    MCP server or `models.json` you set up in the TypeScript version after
    the fork's first run) is copied, and the notice names each one. A file
    the fork already has is never replaced.
  - **resumes after a crash.** Each file is copied to a hidden temp file and
    published in one step. The marker `~/.prime/agent-fork/.imported-sessions-v1`
    is written last, so an interrupted import picks up where it stopped the
    next time the daemon starts.
  - **runs once.** Once the marker exists it never runs again on its own, even
    if `~/.prime/agent` gets new chats.
  - **leaves runtime state behind.** It never copies leases, worker state,
    persistent shells, sockets, logs, lock files, the kernel venv,
    update/restart state, caches or the telemetry id.

  After that the two directories are independent: a chat, login or setting
  changed in one build doesn't reach the other.
- **Re-run or skip the import.** `prime import-sessions` runs it again now,
  in the foreground. It copies only what the fork is missing (new chats
  included) and never overwrites anything. `prime import-sessions --skip`
  turns the automatic import off by writing the marker without copying. To
  start over completely, move `~/.prime/agent-fork` aside (it holds the
  fork's own sessions) and start `prime`.
- **Packages reinstall on demand.** The git and npm packages listed under
  `packages` in the copied `settings.json` aren't copied (they're checkouts
  under the data directory). The first session the fork starts resolves its
  resources and installs every configured package that's missing into
  `~/.prime/agent-fork`, unless `--offline` or `PI_OFFLINE` is set.
  `prime package install <source>` and `prime package update` work too.
- **Separate daemon:** the fork's launcher pins its own daemon socket
  (`${TMPDIR:-/tmp}/prime-agent-fork-<uid>/daemon.sock`). On Windows it pins
  the pipe `\\.\pipe\prime-agent-fork-daemon`. The fork binary uses the same
  names by default, even when started without the launcher. A running
  official daemon and a fork CLI therefore never talk to each other.
- **Separate kernel venv:** the fork's launcher sets
  `PRIME_AGENT_KERNEL_VENV=<fork prefix>/kernel-venv`. The fork's Python
  runtime differs from the official one (it adds `rlm.web`, for example), and
  a shared venv would be rebuilt every time you alternated builds.
- **`models.json` from older fork versions.** Before the fork had its own
  directory, a provider using the fork's
  `auth: { "strategy": "token-exchange" }` block may have been written to
  `~/.prime/agent/models.json`. The official build's `models.json` check
  rejects such a file, and then **none** of your custom models load there.
  `prime-switch install official` warns when this applies.

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
2. Releases are **automatic**: when the `ci` workflow passes for a push to
   `main` (every merge), `fork-release` cuts the next tag for the commit CI
   tested. `N` is one more than the highest `N` already tagged on the
   `Cargo.toml` version, or `1` after a version bump. It releases only if
   shipped code changed since the previous fork release: `crates/`,
   `vendor/`, `prime-agent-runtime/`, `skills/`, `Cargo.toml`, `Cargo.lock`,
   the installers, `scripts/prime-switch*`, `scripts/release/` or
   `fork-release.yml` itself. Test-only paths (`tests/` and `test/`
   directories, `tests.rs`, `*_tests.rs`) do not count. A merge that only
   touches tests, docs, `.changes/` or other workflows ends green with a "no release" line in the `tag-check`
   log. Merges that land close together release in order, one at a time.

   The manual ways still work. Push a tag from an up-to-date `main`:

   ```sh
   git tag v0.10.3-drvova.3
   git push origin v0.10.3-drvova.3
   ```

   Or use **Actions → fork-release → Run workflow**, on `main`, with the tag
   to create (for example `v0.10.3-drvova.3`). The workflow checks the tag
   is new and matches `Cargo.toml`, builds the dispatched `main` commit, and
   the publish job creates the tag there. Either way, the next automatic
   release counts on from the highest tag.
3. `.github/workflows/fork-release.yml` builds linux x64/arm64 and Windows
   x64 (no macOS: GitHub never assigned this repository a macOS runner). It packages them with the official release
   scripts, merges them with `release.yml`'s own promote steps, and publishes
   the GitHub Release (marked latest). Its notes list the merged PRs since
   the previous fork release. The release carries the tarballs,
   `SHA256SUMS`, `latest.json`, `stable`, `install.sh`, `install.ps1`,
   `prime-switch` and `prime-switch.ps1`.
4. Check it: `curl -fsSL .../releases/latest/download/stable` prints the
   version, the install one-liner works, and `prime --version` prints
   `X.Y.Z-drvova.N`.

The packaged version comes from the tag. `Cargo.toml` stays at the upstream
version, and the archive's `package.json` carries `X.Y.Z-drvova.N`, which is
what `prime --version` prints.
