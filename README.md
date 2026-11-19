<p align="center">
  <a href="https://primeintellect.ai">
    <picture>
      <source media="(prefers-color-scheme: light)" srcset="https://github.com/user-attachments/assets/40c36e38-c5bd-4c5a-9cb3-f7b902cd155d">
      <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/6414bc9b-126b-41ca-9307-9e982430cde8">
      <img alt="Prime Intellect" src="https://github.com/user-attachments/assets/6414bc9b-126b-41ca-9307-9e982430cde8" width="312" style="max-width: 100%;">
    </picture>
  </a>
</p>

<h3 align="center">
Prime Agent: A Self-Improving RLM Harness
</h3>

<p align="center">
  <a href="#install">Documentation</a> &bull;
  <a href="https://github.com/PrimeIntellect-ai/verifiers">Verifiers</a> &bull;
  <a href="https://github.com/PrimeIntellect-ai/prime-rl">PRIME-RL</a>
</p>

<p align="center">
  <a href="https://github.com/PrimeIntellect-ai/prime-agent/actions/workflows/ci.yml">
    <img src="https://github.com/PrimeIntellect-ai/prime-agent/actions/workflows/ci.yml/badge.svg" alt="CI" />
  </a>
  <a href="https://arxiv.org/abs/2608.23552">
    <img src="https://img.shields.io/badge/arXiv-2608.23552-b31b1b.svg" alt="arXiv" />
  </a>
</p>

<p align="center">
  <a href="https://trendshift.io/repositories/104249?utm_source=repository-badge&amp;utm_medium=badge&amp;utm_campaign=badge-repository-104249" target="_blank" rel="noopener noreferrer">
    <img src="https://trendshift.io/api/badge/repositories/104249" alt="PrimeIntellect-ai%2Fprime-agent | Trendshift" width="250" height="55" />
  </a>
</p>

## Install (drvova build)

This repository (`drvova/prime-agent`) publishes the Prime Agent (drvova build). Install it from a Linux shell (or Git Bash on Windows):

```bash
curl -fsSL https://github.com/drvova/prime-agent/releases/latest/download/install.sh | sh
```

On Windows, from PowerShell:

```powershell
irm https://github.com/drvova/prime-agent/releases/latest/download/install.ps1 | iex
```

The command is `prime`. For example, run `prime`, `prime --version` or `prime update`.

It installs side by side with the official `prime-agent` and never touches it. It keeps its own sessions, settings and logins in `~/.prime/agent-fork`, so `prime --continue` and `/resume` show only its own chats, never the ones the official build or the TypeScript Prime Agent keep in `~/.prime/agent`. Its first run copies your configuration (settings, logins, MCP servers, custom models, keybindings, skills) from `~/.prime/agent` once. Its daemon then copies your chats in the background the first time it starts, and `/resume` lists them when it's done. That copy never moves or links anything, and never overwrites a file the fork already has. `prime import-sessions` runs the import again, and `--skip` turns it off. Releases are published for Linux x64/arm64 and Windows x64. macOS builds aren't published, because the release workflow no longer builds them. See [FORK.md](FORK.md) for `prime-switch` and the details.

Prime Intellect's own CLI is also called `prime`. The installer never replaces it. If it's already in `~/.local/bin`, the installer puts the fork's `prime` in `~/.local/share/prime-agent-fork/bin` and tells you which `prime` comes first on PATH.

### Releases

Every release is listed at [github.com/drvova/prime-agent/releases](https://github.com/drvova/prime-agent/releases). Releases are tagged `vX.Y.Z-drvova.N` and published automatically when CI passes on `main` and the shipped code changed. The commands above always install the latest release.

To install a specific release, set `PRIME_AGENT_VERSION` to its version (the tag without the leading `v`):

```bash
curl -fsSL https://github.com/drvova/prime-agent/releases/latest/download/install.sh | PRIME_AGENT_VERSION=0.10.3-drvova.2 sh
```

```powershell
$env:PRIME_AGENT_VERSION = "0.10.3-drvova.2"; irm https://github.com/drvova/prime-agent/releases/latest/download/install.ps1 | iex
```

Run `prime update` to move to the latest release, or `prime update --rollback` to go back to the version you had before.

Prime Agent is an open-source coding and research agent for general and long-running work. It is designed around two core abstractions:

- The **[Recursive Language Model (RLM)](https://www.primeintellect.ai/blog/rlm)** treats context as variables (*prompt-as-a-variable*) and tools like recursive subagents as function calls (*programmatic tool /sub-agent calling*) inside a persistent REPL.
- The **[Continual Harness](https://arxiv.org/abs/2605.09998)** stores supplemental prompts, memories, skill descriptions, and reusable subagent specifications as durable state that Prime Agent can refine through small, evidence-backed updates, local to the session by default.

## Install

Install the latest stable version with the one-command installer:

```bash
curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh | sh
```

On Windows, install from PowerShell:

```powershell
irm https://app.primeintellect.ai/prime-agent/install.ps1 | iex
```

The Windows installer downloads the latest stable Windows x64 build and verifies its checksum before installation.

## Why Prime Agent

Prime Agent combines a persistent Python control environment with durable harness state, so useful working context and reusable operating patterns can outlive a single chat window.

- **Everything is programmatic:** a persistent Python REPL is the built-in model tool; file operations, shell commands, tool use, subagents, and context management happen through code.
- **Subagents are built in:** `rlm.spawn(...)` spawns real child agents for parallel or background work and returns their results programmatically.
- **The harness can improve:** `/refine` reviews the current trajectory and can apply small, evidence-backed updates to supplemental harness state. It never rewrites the immutable base system prompt, and recorded snapshots support rollback.
- **Skills can be executable:** Python skills in your user configuration are imported into the kernel; repository skills provide Markdown instructions without automatic code execution.
- **Sessions run in the background:** daemon-backed agents keep running when the terminal disconnects and can be reattached later.
- **Agents communicate directly:** running agents can exchange messages and orchestrate one another without routing everything through the user.
- **Long tasks keep moving:** automatic compaction, persistent goals, heartbeats, schedules, autonomous mode, and retained subagents preserve progress across turns and terminal sessions.

## Slack

Connect Slack from `/mcp` or run `/mcp login slack` inside Prime Agent to search
your workspace and use Slack tools. See the [Slack connector guide](docs/slack-mcp.md)
for OAuth setup and troubleshooting.

## Google Drive and Google Sheets

Connect Google's official Drive and Sheets MCP servers with your own Google
OAuth client: add them to `mcpServers` with `clientId`, `clientSecret`,
`callbackPort`, and `scopes`, then run `/mcp login <name>`. See the
[Google Drive and Google Sheets guide](docs/google-workspace-mcp.md) for the
Google Cloud setup and the tool mapping.

## Clipboard over SSH and tmux

Prime Agent uses the local clipboard tool (such as `pbcopy`) when available. In tmux it asks the attached tmux client to forward a clipboard buffer if that client advertises the `Ms` capability. This works with tmux's default `set-clipboard external`, unlike application-origin OSC 52. Without tmux, remote copies request delivery using OSC 52. Terminal requests cannot confirm that your local clipboard changed, so Prime Agent reports them as unconfirmed.

Nested tmux servers and terminals may still block delivery. An outer tmux with `set-clipboard external` can reject the inner server's OSC 52. In that case, run `tmux set -s set-clipboard on` on the outer server, or use native terminal selection (Shift-drag in most terminals; Option-drag in iTerm2) followed by Cmd-C. macOS Terminal.app does not support OSC 52; iTerm2 requires its “Applications in terminal may access clipboard” setting.

## Custom certificate authorities

HTTPS requests trust your operating system's trust store together with the bundled Mozilla roots. For servers behind a custom CA, point `NODE_EXTRA_CA_CERTS` at a PEM bundle of your CA certificates — the same variable the Node/TypeScript build honors:

```bash
export NODE_EXTRA_CA_CERTS=/path/to/your-ca.pem
```

Each transport reads the variable once when its first client is initialized. The background daemon keeps the environment it started with, so run `prime-agent shutdown` after changing it.

## Getting Started

Start Prime Agent from the repository or directory you want it to work in:

```bash
cd /path/to/project
prime-agent
```

On first launch, run `/login` to choose a subscription or API-key provider. Prime Agent works in the current directory and can run commands and modify files there. Use a disposable clone, clean worktree, or another checkpoint you can inspect and restore.

> [!WARNING]
> Prime Agent executes model-generated Python and project commands with your user permissions. Its worker and kernel processes improve lifecycle isolation and recovery; they are **not** a security sandbox. Review changes and use trusted repositories, instructions, and skills only. Run untrusted code or instructions in an external sandbox or restricted environment.

Useful commands:

```bash
prime-agent agents                   # Browse running, idle, and saved sessions
prime-agent attach <agent>           # Reattach to a running session
prime-agent --resume [path|id]       # Browse sessions or resume one directly
prime-agent status                   # Inspect background service state
prime-agent doctor [--fix]           # Inspect or repair background services
prime-agent update [--force]         # Update Prime Agent
prime-agent shutdown [--force]       # Stop every agent, worker, and background service
```

## Built for Long-Running Work
Prime Agent is built for long-running work, especially for evaluations in research. These features are available in the TUI, and when run autonomously.

- **Continual Harness:** `/refine` can persist focused, reviewable lessons as supplemental prompts, memories, reusable skill descriptions, or subagent specifications, with recorded refinement history. It does not replace packaging and reviewing new executable skills.
- **Direct agent-to-agent communication:** running agents and retained subagents can discover one another, exchange messages, and steer active work.
- **Daemon-backed continuity:** active sessions, Python REPL state, schedules, and subagents keep running when the terminal detaches and can be reattached later.
- **Heartbeats and schedules:** `/heartbeat`, `rlm_heartbeat`, and `prime-agent schedule` can re-enter a session periodically or at a specific time.
- **Persistent goals:** `/goal` keeps an objective and its progress active across turns until it is completed, paused, or cleared.
- **Bounded autonomous mode:** `/autonomous` continues within configured turn, token, and time budgets and can run user-defined quality gates. A passed gate checks only what that gate verifies; reaching a limit does not imply task success.

## Contributing

Start with a GitHub Discussion for [general questions](https://github.com/PrimeIntellect-ai/prime-agent/discussions/categories/general), [bug reports](https://github.com/PrimeIntellect-ai/prime-agent/discussions/categories/bug-reports), and [feature requests](https://github.com/PrimeIntellect-ai/prime-agent/discussions/categories/feature-requests). Maintainers promote accepted work into Issues, and pull requests are reviewed from maintainers and vouched contributors.

## License

Prime Agent is fully open source and released under the [MIT License](LICENSE).

## Citation

If you use this codebase in your research, please cite Prime Agent:

```bibtex
@article{karten2026prime,
  title={Prime Agent: A Self-Improving RLM Harness},
  author={Karten, Seth and Zhang, Alex L. and Thomas, Kevin and Müller, Sebastian and Bakouch, Elie and Auras, Daniel and Senghaas, Mika and Obeid, Fares and Dunas, Konstantin and Hagemann, Johannes and Jaghouar, Sami},
  journal={arXiv preprint arXiv:2608.23552},
  year={2026}
}
```

Available at [https://arxiv.org/abs/2608.23552](https://arxiv.org/abs/2608.23552).

### Automatic compaction settings

`/settings` exposes the global automatic compaction percentage in five-point steps,
with a 95% default. A saved off-grid percentage stays selectable. The effective
trigger also reserves room for retained history and summaries; the model's input
and requested-output ceiling always takes priority. Manual compaction budgets are
unchanged. The underlying `compaction.thresholdPercent` setting accepts 1–100,
including fractional values; project settings can override the global setting.

For per-model early compaction, use the existing provider-scoped
`modelOverrides.<id>.contextWindow` option in `models.json`, for example:

```json
{
  "providers": {
    "prime-inference": {
      "modelOverrides": {
        "your-model-id": { "contextWindow": 524288 }
      }
    }
  }
}
```

Use the actual model ID for your provider. This sets that model's effective context
window; it does not change every model's global compaction percentage.

### Prompt cache settings

On providers with explicit cache markers (Anthropic, and Claude on Bedrock), the
system prompt goes out as two cached blocks: the static layers every session shares,
then the session-specific tail. A new session or subagent on the same model reads the
static layers from the cache instead of writing them again.

- `cacheRetention`: `"short"` (default, the provider's five-minute cache) or `"long"`
  (one hour; cache writes cost 2x input instead of 1.25x). The `PI_CACHE_RETENTION`
  environment variable (`long` or `short`) overrides the setting.
- `cacheWarming`: `"active"` (default), `"idle"`, or `"off"`. Shortly before the cache
  entry expires, a waiting session re-sends its last request with a one-token reply to
  keep the cache alive, but only when the expected saving is at least $0.05: the cache
  write the next request avoids, weighted by how likely that request is, minus the
  refresh's own cache read. `active` warms only while the next request is near-certain:
  a tool, a background `bash()` job, or a subagent is still running, or a subagent is
  waiting on its parent. `idle` also warms an idle session, weighted at a 15% chance of
  a next turn. Warming stops after 60 minutes (30 when nothing is running) or 12
  refreshes, never delays a real request, and is cancelled the moment one starts.
  Requests that think on a fixed token budget are never warmed.

### Provider auth descriptors

A `models.json` provider entry can declare how the provider authenticates with an
optional `auth` block. Without one, the provider keeps its usual API key (stored,
environment, or the entry's `apiKey`). Three strategies exist:

- `"api-key"`: the usual key. Use it to add a `sessionHeader` only.
- `"token-exchange"`: before inference calls, Prime Agent trades long-lived
  credentials from `auth.json` for a short-lived token at `tokenUrl`, and caches the
  token until shortly before it expires.
- `"oauth"`: you sign in once in the browser (or with a device code), and Prime
  Agent refreshes the access token before it expires. See
  [OAuth sign-in](#oauth-sign-in) below.

```json
{
  "providers": {
    "example-gateway": {
      "baseUrl": "https://gateway.example.com/v1",
      "api": "openai-completions",
      "auth": {
        "strategy": "token-exchange",
        "tokenUrl": "https://auth.example.com/v1/token",
        "method": "POST",
        "requestHeaders": { "x-account": "{{accountId}}" },
        "requestBody": {
          "refresh_token": "{{refresh}}",
          "account": "{{accountId}}",
          "org": "{{orgId}}"
        },
        "tokenPath": "/access_token",
        "refreshPath": "/refresh_token",
        "expiresInPath": "/expires_in",
        "header": { "name": "Authorization", "prefix": "Bearer " },
        "sessionHeader": "x-example-session",
        "timeoutMs": 10000
      },
      "models": [{ "id": "example-model" }]
    }
  }
}
```

The long-lived credentials live in `auth.json` under the same provider id:

```json
{
  "example-gateway": {
    "type": "token_exchange",
    "refresh": "<long-lived refresh credential>",
    "accountId": "acc_123",
    "orgId": "org_456"
  }
}
```

Fields:

- `tokenUrl` (required for `token-exchange`): must use `https`; plain `http` is
  accepted only for loopback hosts.
- `method`: `POST` (default), `PUT`, or `GET` (a `GET` carries no body).
- `requestHeaders` / `requestBody`: templates for the token request only. The
  placeholders are the stored credential fields `{{refresh}}`, `{{accountId}}`, and
  `{{orgId}}`; nothing else (such as environment variables) renders. The body is JSON,
  or form-encoded with `"requestEncoding": "form"` (string values only).
- `tokenPath`: a JSON pointer to the token in the response.
- `refreshPath` (optional): a JSON pointer to a rotated refresh credential in the
  response. For providers that issue a new refresh credential on every exchange and
  invalidate the old one: a non-empty string there replaces the stored `refresh` in
  `auth.json`, together with the new token. Without it, or when the response has no
  value there, the stored `refresh` is kept.
- `expiresAtPath` or `expiresInPath` (at most one): a JSON pointer to an absolute
  expiry (values at or above 10^12 are epoch milliseconds, smaller values epoch
  seconds) or to a lifetime in seconds. With neither, the token serves until the
  provider rejects it.
- `header`: how the token is attached (default `Authorization` with prefix
  `Bearer `). The token also fills the provider's own key slot.
- `sessionHeader` (any strategy): a header that carries the conversation's session id,
  the same on every request of one conversation.
- `timeoutMs`: the token request deadline, 1 to 60000 (default 10000).

The exchange runs only before inference calls, never at startup. Concurrent sessions
(including subagent workers) share one exchange per provider and account. When the
provider answers 401, the cached token is dropped and the call is retried once with a
fresh token; a second 401 is reported. A 429 reports the provider's reset time
(`Retry-After` or `x-ratelimit-reset*`) under the usual retry policy. An unknown field
or a malformed block is a `models.json` load error that names the provider.

#### OAuth sign-in

The `"oauth"` strategy lets any OAuth 2.0 provider sign in through the browser, with
no provider-specific code:

```json
{
  "providers": {
    "example-gateway": {
      "baseUrl": "https://gateway.example.com/v1",
      "api": "openai-completions",
      "auth": {
        "strategy": "oauth",
        "authorizationUrl": "https://auth.example.com/oauth/authorize",
        "deviceAuthorizationUrl": "https://auth.example.com/oauth/device/code",
        "tokenUrl": "https://auth.example.com/oauth/token",
        "clientId": "prime-agent-cli",
        "clientSecret": "EXAMPLE_GATEWAY_CLIENT_SECRET",
        "scopes": ["openid", "offline_access"],
        "redirect": { "port": 8976, "path": "/callback" },
        "audience": "https://gateway.example.com",
        "extraAuthParams": { "prompt": "consent" }
      },
      "models": [{ "id": "example-model" }]
    }
  }
}
```

Sign in with `prime-agent login example-gateway`, or pick the provider in `/login`.
Prime Agent opens the authorization page in your browser and always prints the URL
too. The sign-in uses the authorization code flow with PKCE (S256), and the callback
listens only on `127.0.0.1`. A callback whose `state` does not match is refused, and
the browser sign-in gives up after five minutes. When the provider has a
`deviceAuthorizationUrl` and no browser can open here (an SSH session or a host
without a display), or you pass `--device`, Prime Agent shows a code and a
verification URL to open on any device, then waits for you to finish there.

The tokens are stored in `auth.json` under the provider id, as an `oauth` credential
bound to the `tokenUrl` and `clientId` it was issued for. Before an inference call,
a token that expires within a minute is refreshed with the refresh token. A new
refresh token from the provider replaces the stored one. Concurrent sessions share
one refresh. When a refresh fails, the call fails with
`Sign in again: prime-agent login <provider>`. When the provider answers 401, the
token is refreshed once and the call is retried. `prime-agent logout <provider>`
removes the stored credential.

Fields:

- `authorizationUrl` (browser sign-in) and/or `deviceAuthorizationUrl` (device
  code): at least one is required. Each must use `https`; plain `http` is accepted
  only for loopback hosts.
- `tokenUrl` (required): the token endpoint for the code exchange, the device poll,
  and the refresh. Same `https` rule.
- `clientId` (required) and `clientSecret` (optional). The secret is a config value:
  an environment variable name, a `!command`, or a literal. It is resolved only when a
  token request is sent, and it is sent as `client_secret` in the form body.
- `scopes`: a list of scopes, sent space-separated.
- `redirect`: the loopback callback's `port` (default: a free port in 53700-53709) and
  `path` (default `/callback`). The redirect URI is `http://127.0.0.1:<port><path>`.
  Register it with the provider.
- `audience` and `extraAuthParams`: extra authorization (and device authorization)
  request parameters. `extraAuthParams` cannot set the parameters the sign-in sets
  itself (`state`, `redirect_uri`, `scope`, and so on).
- `tokenPath` (default `/access_token`), `refreshPath` (default `/refresh_token`),
  and `expiresInPath` (default `/expires_in`, optional in the response) or
  `expiresAtPath`: JSON pointers into the token response, as for `token-exchange`.
- `header`, `sessionHeader`, and `timeoutMs`: as for `token-exchange`.
