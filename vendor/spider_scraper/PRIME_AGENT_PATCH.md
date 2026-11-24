# spider_scraper 0.1.2 with Prime Agent's patches

Pinned source of crates.io `spider_scraper` 0.1.2 (MIT), the HTML tree and
CSS selection that `spider_utils` (`web-extract`, `web-crawl #:extract`) and
`spider_transformations` (`web-read` / `web-crawl` `#:format` and
`#:transform`) are built on. Only `src/`, the manifest, `LICENSE` and
`README.md` are kept.

Each change is listed here; nothing else differs from the published crate.

## Why: selectors 0.25 depends on the unmaintained fxhash

Every published `spider_scraper` (0.1.x through 0.3.0) depends on
`selectors` 0.25, which depends on `fxhash` 0.2, unmaintained
(RUSTSEC-2025-0057). `selectors` dropped `fxhash` for `rustc-hash` in 0.32.
The vendored copy moves to `selectors` 0.37 and `cssparser` 0.36, the
versions `lol_html` already brings into the graph, so the port also removes
the second copies of `selectors`, `cssparser`, `servo_arc`, `phf` 0.10,
`derive_more` 0.99 and `siphasher` 0.3.

## `Cargo.toml`

- `selectors` `0.25.0` -> `0.37`, `cssparser` `0.31.2` -> `0.36`.
- `precomputed-hash` `0.1` is added (already in the graph through
  `selectors` and `string_cache`), for the impl below.

## `src/selector.rs`: selectors 0.37's API

- `SelectorList` is no longer a public `SmallVec`; `Selector::parse` collects
  `list.slice()` into the same `SmallVec` field.
- `MatchingContext::new` takes `SelectorCaches` instead of `NthIndexCache`,
  and `MatchingForInvalidation::No` instead of
  `IgnoreNthChildForInvalidation::No`.
- `SelectorImpl::Identifier` and `LocalName` now require `PrecomputedHash`;
  `CssLocalName` implements it with its `LocalName` atom's hash.

## `src/element_ref/element.rs`: two new `Element` methods

- `has_custom_state` (`:state(...)`) returns `false`: the tree has no custom
  element states, as `match_non_ts_pseudo_class` already matches nothing.
- `add_element_unique_hashes` returns `false`: no bloom filter is passed to
  matching (`MatchingContext::new(.., None, ..)`), so no hashes are needed.

Drop the vendored copy with the `spider_scraper` release that depends on a
`selectors` without `fxhash`.
