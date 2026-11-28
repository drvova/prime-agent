//! The `/mcp` page's catalog on the runtime daemon, ported from the old
//! daemon's `mcp_catalog_e2e`, with its expectations:
//!
//! - `session.mcp_connections` serves the resolved catalog the fetch lane's
//!   disk cache holds: every service of it, the compiled connectors, the
//!   pasteable services marked, and the api-key credential rows;
//! - a connection whose service left the catalog keeps a pinned card, which
//!   claims the catalog source unavailable only while a validated snapshot
//!   proves the service gone.
//!
//! The old test's paste install and disconnect are not the daemon's on the
//! runtime: the TUI runs them in its own process (pa-cli's `mcp_login`,
//! behind `ClientAuthCommands`), and `wire2_compat` sends no
//! `set_mcp_static_token` or `remove_mcp_connection`.

use std::path::Path;

use serde_json::{json, Value};

use super::{ok, Bed, Client};

/// Every daemon runs offline: the catalog is the disk cache's, never one a
/// live fetch replaced.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// The old daemon's pinned-from-record hint, byte for byte.
const PINNED_FROM_RECORD_HINT: &str =
    "This service's catalog source is unavailable; its connection keeps the pinned definition.";

/// The shipped catalog payload projected to the v2 client contract (the
/// fixture the pa-core parity tests parse).
const REAL_CATALOG: &str =
    include_str!("../../../pa-core/tests/fixtures/mcp/plugins-catalog.v2.json");

/// The fetch lane's disk form of `payload`: the snapshot envelope
/// `{url, scope, fetchedAt, payload}` the cache reader validates (a bare
/// catalog at the cache path never serves). The URL and scope are the
/// lane's own, spelled out: they are the on-disk contract.
fn snapshot_envelope(payload: &Value) -> String {
    json!({
        "url": "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/plugins/catalog.v2.json",
        "scope": "public",
        "fetchedAt": 1_790_082_036_135_u64,
        "payload": payload,
    })
    .to_string()
}

/// The fetch lane's cache file under `agent_dir`.
fn cache_path(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join("mcp-service-catalog.v2.json")
}

/// A fresh session's `/mcp` page on the runtime daemon over `bed`'s agent
/// dir.
async fn mcp_page(bed: &Bed) -> Value {
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": ["idle"] }), OFFLINE);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = ok(client
        .call("session.create", json!({ "cwd": bed.dir.path() }))
        .await);
    ok(client
        .query(
            "session.mcp_connections",
            json!({ "session": created["session"] }),
        )
        .await)
}

/// The service row `id` of an `/mcp` page.
fn service<'page>(page: &'page Value, id: &str) -> &'page Value {
    page["services"]
        .as_array()
        .unwrap_or_else(|| panic!("a services array: {page}"))
        .iter()
        .find(|service| service["serviceId"] == id)
        .unwrap_or_else(|| panic!("no {id} row: {page}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_page_serves_the_catalog_the_disk_cache_holds() {
    let bed = Bed::new();
    // One pasteable service whose endpoint cannot resolve, beside the
    // shipped catalog.
    let mut catalog: Value = serde_json::from_str(REAL_CATALOG).unwrap();
    catalog["entries"].as_array_mut().unwrap().push(json!({
        "server": "paste-fixture", "service": "paste-fixture",
        "label": "Paste Fixture", "url": "https://paste-fixture.invalid/mcp",
        "aliases": [],
        "transport": { "type": "http", "url": "https://paste-fixture.invalid/mcp" },
        "auth": { "strategy": "api_key", "clientRegistration": "unknown" },
        "setup": {
            "status": "requires-setup",
            "reason": "paste a fixture token",
            "fields": [
                { "id": "FIXTURE_PAT_TOKEN", "label": "FIXTURE_PAT_TOKEN",
                  "required": true, "kind": "bearer-token",
                  "credentialSet": "fixture-pat" }
            ]
        },
        "verification": { "status": "unverified" },
        "legacyBuiltin": false, "provenance": [{ "source": "prime" }]
    }));
    std::fs::write(cache_path(&bed.agent_dir()), snapshot_envelope(&catalog)).unwrap();
    let page = mcp_page(&bed).await;
    let row = |id: &str| {
        let service = service(&page, id);
        vec![
            service["connectionStatus"].clone(),
            service["connectable"].clone(),
            service["pasteToken"].clone(),
        ]
    };
    let serper = page["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|credential| credential["id"] == "serper")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (
            page["services"].as_array().unwrap().len(),
            row("linear")[..2].to_vec(),
            row("slack")[..2].to_vec(),
            row("paste-fixture")[0].clone(),
            row("paste-fixture")[2].clone(),
            row("github")[2].clone(),
            json!([serper["label"], serper["configured"]]),
        ),
        (
            70,
            vec![json!("not_connected"), json!(true)],
            vec![json!("not_connected"), json!(true)],
            json!("setup_required"),
            json!(true),
            json!(true),
            json!(["Serper (web search)", false]),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_card_claims_its_source_unavailable_only_from_a_snapshot() {
    let bed = Bed::new();
    let agent_dir = bed.agent_dir();
    // A fetched catalog without the service, and the connection's record.
    let cache = json!({
        "version": 2,
        "counts": {},
        "entries": [{
            "server": "cache-only", "service": "cache-only", "label": "Cache Only",
            "url": "https://cache-only.example/mcp", "aliases": [],
            "transport": { "type": "http", "url": "https://cache-only.example/mcp" },
            "auth": { "strategy": "oauth", "clientRegistration": "dynamic" },
            "setup": { "status": "ready" },
            "verification": { "status": "unverified" },
            "legacyBuiltin": false, "provenance": [{ "source": "prime" }]
        }]
    });
    std::fs::write(cache_path(&agent_dir), snapshot_envelope(&cache)).unwrap();
    std::fs::write(
        agent_dir.join("mcp-connections.json"),
        json!({
            "version": 1,
            "connections": {
                "vanished-service": {
                    "connectionId": "vanished-service",
                    "serviceId": "vanished-service",
                    "endpoint": "https://vanished.example/mcp",
                    "label": "Vanished",
                    "status": "pending",
                    "createdAt": 1u64,
                    "updatedAt": 1u64,
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let card = |page: &Value| {
        let pinned = service(page, "vanished-service");
        json!([
            pinned["connectionStatus"],
            pinned["connectable"],
            pinned["setupHint"].as_str() == Some(PINNED_FROM_RECORD_HINT)
        ])
    };
    // The snapshot proves the service gone: the hint shows.
    let with_snapshot = card(&mcp_page(&bed).await);
    // No snapshot (and no packaged bundle): the card stays, the claim does not.
    std::fs::remove_file(cache_path(&agent_dir)).unwrap();
    let without_snapshot = card(&mcp_page(&bed).await);
    assert_eq!(
        (with_snapshot, without_snapshot),
        (
            json!(["not_connected", false, true]),
            json!(["not_connected", false, false])
        )
    );
}
