//! `session.set_mcp_servers`: the MCP servers one client supplies an open
//! session (ACP's per-connection servers), the old daemon's
//! `replace_acp_mcp_servers` contract.
//!
//! The servers go into the session engine's MCP manager, fenced by the
//! client's owner id: while one owner's servers are in, another owner's are
//! refused, and an empty list from the owner clears them (an empty list from
//! anyone else changes nothing). A non-empty list is refused while a turn
//! runs, since the agent cannot take a different tool list mid-turn. A
//! replacement that fails is rolled back with the owner's clear. The client
//! validates the entries (names, transports, credentials) before it sends
//! them; a list whose entries are not server configs is refused whole.

use anyhow::Context as _;
use pa_core::mcp::AcpMcpServerConfig;
use serde_json::{json, Value};

use super::{string_arg, RuntimeHandlers};
use crate::session::SessionBuilder;

impl<B: SessionBuilder> RuntimeHandlers<B> {
    pub(super) async fn set_mcp_servers(&self, args: &Value) -> anyhow::Result<Value> {
        let id = string_arg(args, "session")?;
        let owner = string_arg(args, "owner")?;
        let servers: Vec<AcpMcpServerConfig> = serde_json::from_value(
            args.get("servers")
                .cloned()
                .context("missing the `servers` argument")?,
        )
        .context("the `servers` argument is not a list of MCP server configs")?;
        anyhow::ensure!(
            self.open.contains(&id),
            "session {id} is not open in the runtime daemon"
        );
        if !servers.is_empty() && self.runtime.lane(&id).running.is_some() {
            anyhow::bail!("Cannot replace ACP MCP servers while the agent is running");
        }
        let live = self.runtime.built_engine(&id).await?;
        let manager = live
            .engine
            .mcp_manager
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = match manager.replace_acp_servers(&servers, &owner) {
            Ok(changed) => changed,
            Err(error) => {
                if manager.can_release_acp_servers(&owner) {
                    if let Err(clear) = manager.replace_acp_servers(&[], &owner) {
                        tracing::warn!(
                            "session {id}'s MCP servers were not rolled back: {clear:#}"
                        );
                    }
                }
                return Err(error);
            }
        };
        let names: Vec<String> = manager
            .get_acp_servers()
            .iter()
            .map(|server| server.name().to_string())
            .collect();
        Ok(json!({ "session": id, "changed": changed, "servers": names }))
    }
}
