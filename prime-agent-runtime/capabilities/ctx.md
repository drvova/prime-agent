## Paged-out output

A truncated output shows a handle, `[out:<id> · N chars · L lines · M chars not shown]`, where its text was cut; the whole output is kept for the session, so page it with ctx.* instead of re-running the command. Await these; line numbers are 0-based page offsets and each reply is capped near 16k characters.

- `ctx.page(handle: str, *, start: int = 0, lines: int = 200) -> Page`: lines `start` to `page.end`; read on with `start=page.end`
- `ctx.grep(handle: str, pattern: str, *, context: int = 2, max_matches: int = 50) -> Matches`: regex matches as `N:line`, context as `N-line`
- `ctx.tail(handle: str, n: int = 200) -> Page`: the last lines
- `ctx.info(handle: str) -> Info`: the counts, or why there is no text (`evicted`, `not_stored`), which `page`/`grep`/`tail` raise as `ctx.PagedOutUnavailable`
