# session: the model, settings, usage, and images

Read-only views of what this session runs with, except `model-set-effort`; nothing here is a credential.

- `(model-info)`: the session model `{id, provider, input, contextWindow, thinkingLevel}`; `input` lists what it accepts (`"text"`, `"image"`)
- `(model-set-effort level)`: change this session's thinking level (`"off"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`) from the next model call; the effective level. It changes cost, so it raises unless the user set `"replCanChangeEffort": true` in the global settings. Children take their own level with `rlm-spawn`'s `#:thinking`
- `(settings-get)`: the session's effective settings `{session, host}`, each as the host reports it: the model, context window, thinking level, cell time limit, compaction, recursion depth and its limit, web search provider, and whether the session is daemonless or autonomous
- `(usage-get #:refresh)`: `{session, quotas}`: this session's cost in USD (its own and its subagents') and tokens, and the quota windows of each signed-in subscription provider. Quota cards are cached for a few minutes; `#:refresh #t` re-reads them. Check before expensive work and back off before a quota runs out
- `(attach-image path ...)`: load image files (PNG, JPEG, GIF, WebP) into your context so you can see them; the only way to see an image, since reading its bytes does not show it. Returns a line saying what was loaded. Large images are scaled down and re-encoded as JPEG (an animated one from its first frame); the files are not changed. Raises when the session's model does not take images, or a path is not a readable image under 20 MB
