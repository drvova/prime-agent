# pa-ai

Provider APIs and model registry.

## Scope
Provider trait + per-provider streaming clients (anthropic, openai-completions/responses, google, bedrock, mistral, azure, prime-inference), model registry/resolution, usage accounting, stream-failure retry, provider-error shapes (per-SDK user-facing texts, diagnostic error names, connection-error profiles), bedrock transport selection (h2c prior-knowledge HTTP/2 cleartext, h2-preferred TLS ALPN, the http1 AWS_BEDROCK_FORCE_HTTP1/proxy mode), overflow handling, JSON repair parsing, faux provider for tests. The stream loop guard (`loop_guard`, crate-private): the shared stream entry screens every provider's streamed text and thinking for runaway repetition and cuts a looping stream as a `repetition_loop` stream failure. Provider quota fetchers (`quota`): the read-only subscription usage endpoints (Claude `/api/oauth/usage`, ChatGPT `/wham/usage`, GitHub `/copilot_internal/user`) parsed into `pa_types::provider_usage::QuotaWindow` through the `oauth::ProviderHttp` transport with a bounded timeout.

## Non-goals
No agent loop, no tool execution, no session state, no UI. Receives/returns `pa-types` messages.

## Public API
`Provider` trait, `ProviderRegistry`, model lookup/resolution, faux provider. `utils::tls::http_client_builder` lets caller-owned HTTP transports apply the shared extra-CA trust configuration. Per-provider internals are `pub(crate)`. `utils::prewarm::{prewarm_connection, NetworkMode, PrewarmOutcome}` opens one pooled keep-alive connection to a provider base URL ahead of the first request (once per origin per process, never offline) so the first turn skips the TCP/TLS handshake. `SESSION_ID_TEMPLATE` is the caller-header value the shared stream entry replaces with the request's session id (a provider's configured session header; dropped on a session-less request). `StreamOptions::loop_guard` names a request's loop guard level (`pa_types::ai::LoopGuardLevel`); `None` takes `Provider::default_loop_guard` (by model family; off for the faux provider, whose scripted fixtures stream as written).

## Depends on
pa-types (one-way).
