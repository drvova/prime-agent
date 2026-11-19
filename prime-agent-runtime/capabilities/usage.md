## Usage and quotas

Read-only, for budgeting long runs: check before expensive work and back off before a quota runs out.

- `usage.get(*, refresh: bool = False) -> Usage`: `str(usage)` summarizes it; `usage.cost` holds `Cost(total, own, subagents)` in USD (`subagents` is your children's rolled-up spend); `usage.tokens` holds `Tokens(input, output, cache_read, cache_write, total)`; `usage.quotas` lists `ProviderQuota(provider, plan, state, windows, resets_at, error)` for each signed-in subscription provider, `state` being `windows`, `rate_limited`, or `failed`, with `QuotaWindow(label, used, limit, unit, resets_at)` (`unit` is `percent`/`requests`, `resets_at` a UTC datetime, `window.used_fraction`). Quota cards are cached for a few minutes; `refresh=True` re-reads them. `usage.quotas_unavailable` says why `quotas` is empty when no daemon serves the session.
