# pa-ai

Provider APIs and model registry.

## Scope
Provider trait + per-provider streaming clients (anthropic, openai-completions/responses, google, bedrock, mistral, azure, prime-inference), model registry/resolution, usage accounting, stream-failure retry, provider-error shapes (per-SDK user-facing texts, diagnostic error names, connection-error profiles), bedrock transport selection (h2c prior-knowledge HTTP/2 cleartext, h2-preferred TLS ALPN, the http1 AWS_BEDROCK_FORCE_HTTP1/proxy mode), overflow handling, JSON repair parsing, faux provider for tests.

## Non-goals
No agent loop, no tool execution, no session state, no UI. Receives/returns `pa-types` messages.

## Public API
`Provider` trait, `ProviderRegistry`, model lookup/resolution, faux provider. `utils::tls::http_client_builder` lets caller-owned HTTP transports apply the shared extra-CA trust configuration. Per-provider internals are `pub(crate)`. `utils::prewarm::{prewarm_connection, NetworkMode, PrewarmOutcome}` opens one pooled keep-alive connection to a provider base URL ahead of the first request (once per origin per process, never offline) so the first turn skips the TCP/TLS handshake. `SESSION_ID_TEMPLATE` is the caller-header value the shared stream entry replaces with the request's session id (a provider's configured session header; dropped on a session-less request).

## Depends on
pa-types (one-way).
