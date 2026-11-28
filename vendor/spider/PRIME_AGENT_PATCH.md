# spider 2.54.2 with Prime Agent's patches

Pinned source of crates.io `spider` 2.54.2 (MIT), the crawler behind the
kernel's `web-crawl` (`crates/pa-core/src/web_crawl/`). Only `src/`, the
manifest, `LICENSE` and `README.md` are kept; the crate's tests and benches
are not vendored.

Each change is listed here; nothing else differs from the published crate.

## `Cargo.toml`: no reqwest defaults, no build-time downloads

- `reqwest` is declared with `default-features = false` (both targets), and
  its `brotli`, `deflate` and `zstd` features are dropped. Upstream takes
  reqwest's defaults, and Cargo unifies features across the graph, so every
  HTTP client in Prime Agent would have gained aws-lc-rs TLS beside ring,
  the OS proxy settings (`system-proxy`) and extra `Accept-Encoding`s.
  pa-core hands spider its own client and fetch engine, so spider never
  needs those itself.
- `spider_fingerprint` is built without `dynamic-versions`. With it, that
  crate's build script downloads Chrome version lists from the network on
  every build (and writes into its own source directory); without it, the
  crate uses the version list it ships.
- `ua_generator` is built without its default `fetch` feature. With it, that
  crate's build script pulls in an HTTP client (`ureq`, a second
  `webpki-roots`) and, when `BUILD_ENABLED` is set in the build's environment
  or a `.env` file, downloads user-agent lists from spider.cloud; without
  it, the crate uses the lists it ships.
- The `[[test]]`, `[[bench]]` and `[dev-dependencies]` sections are removed
  with the sources they name.

## `src/website.rs`: no background connect runtime for a supplied client

`Website::setup_base` calls `utils::connect::init_background_runtime()` on
every crawl, which starts (once per process) a thread holding a multi-thread
tokio runtime with one worker per CPU, outside any session. Only a client
spider builds itself routes its connects through that runtime; a client set
with `Website::set_http_client`, which pa-core always does, never does. The
patch starts the runtime only when no client was supplied.

## `src/page.rs`: a smart crawl keeps the page it rendered

`Page::links_stream_smart` (both copies, with and without `full_resources`)
takes the HTTP body, and when its upgrade score says the page needs
JavaScript renders it in the browser and assigns the rendered page to
`self`, then puts the HTTP body back over it unconditionally, so
`crawl_smart` delivered the unrendered HTML and kept only the rendered
page's links. The patch puts the HTTP body back only when nothing replaced
it.

## `src/utils/mod.rs`: a browser cookie as a `Set-Cookie` line, public

`utils::browser_cookie_to_set_cookie` is `pub(crate)` upstream; it is made
`pub` so `web-capture` keeps the cookies its page ends with
in the session's jar exactly as a rendered crawl's pages do (domain, path,
expiry and flags kept). Nothing else about it changes.

## `src/features/automation.rs`, `src/configuration.rs`: automation on the configs' client

The three places spider builds a `RemoteMultimodalEngine` from a crawl's
`RemoteMultimodalConfigs` (`run_remote_multimodal_extraction`,
`prefilter_urls`, `Configuration::build_remote_multimodal_engine`) hand it
the configs' `client` (a field vendor/spider_agent adds), so every model
request of a crawl's automation goes out on the client pa-core supplies,
never spider_agent's static one. The per-page browser automation itself is
built in spider_agent (`run_remote_multimodal_with_page`), patched there.

## `src/utils/mod.rs`: a rendered page's automation runs in a task of its own

`fetch_page_html_chrome_base` awaits a page's model-driven automation
(`run_remote_multimodal_if_enabled`) inline, under the crawl's whole poll
chain; with the engine's DOM reads on top, that overflowed a 2 MiB worker
stack (the size of a session runtime's workers), which aborts the process.
The patch spawns the automation as its own task (it starts at the top of a
worker's stack) and aborts that task when the page's future is dropped, so a
stopped crawl stops it.

Drop the vendored copy with the spider release that declares reqwest without
default features and makes `dynamic-versions` optional (and the runtime
lazy), or once pa-core no longer needs spider.
