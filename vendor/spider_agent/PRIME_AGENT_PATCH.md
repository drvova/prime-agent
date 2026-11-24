# spider_agent 2.54.2 with Prime Agent's patches

Pinned source of crates.io `spider_agent` 2.54.2 (MIT, from the
spider-rs/spider repository, as `vendor/spider`), the model-driven browser
automation behind the kernel's `web-automate`
(`crates/pa-core/src/web_crawl/automate.rs`). Only `src/`, the manifest and
`README.md` are kept; the published crate ships no licence file, so
`LICENSE` is spider's own (same repository, same MIT terms). Its examples,
tests and `SKILLS.md` are not vendored.

Each change is listed here; nothing else differs from the published crate.

## `Cargo.toml`: no reqwest defaults, no examples or tests

- `reqwest` is declared with `default-features = false` (it keeps `json`
  and `query`). Upstream takes reqwest's defaults, and Cargo unifies
  features across the graph, so every HTTP client in Prime Agent would have
  gained aws-lc-rs TLS beside ring, the OS proxy settings
  (`system-proxy`) and HTTP/3 (quinn). pa-core hands the engine its own
  client (`RemoteMultimodalEngine::with_client`), so the crate never needs
  those itself.
- The `[[example]]`, `[[test]]` and `[dev-dependencies]` sections are
  removed with the sources they name.

Drop the vendored copy with the spider_agent release that declares reqwest
without default features, or once pa-core no longer needs it.
