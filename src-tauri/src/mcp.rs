//! MCP (Model Context Protocol) client manager.
//!
//! Spawns external MCP servers as subprocesses, holds long-lived JSON-RPC
//! connections, exposes a sync API for the rest of Jarvis (which lives in
//! sync land via reqwest::blocking, parking_lot, etc).
//!
//! Today: only the Google Calendar server (@cocal/google-calendar-mcp).
//! Easy to add more — wrap another `OnceLock<Peer>` on `McpManager`.
//!
//! Async ↔ sync bridge: a dedicated multi-thread tokio runtime is built
//! once and owned by the manager. All MCP calls run `runtime.block_on(...)`
//! from the calling sync thread. Safe because the runtime has its own
//! worker threads — we're not blocking the runtime that's blocking on us.

use anyhow::{anyhow, bail, Result};
use rmcp::{
    model::CallToolRequestParam,
    transport::{ConfigureCommandExt, TokioChildProcess},
    RoleClient, ServiceExt,
};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tokio::process::Command as TokioCommand;
use tokio::runtime::Runtime;

pub struct McpManager {
    runtime: Runtime,
    /// Long-lived peer (RPC client handle) to the calendar MCP subprocess.
    calendar_peer: OnceLock<rmcp::service::Peer<RoleClient>>,
}

static GLOBAL: OnceLock<Arc<McpManager>> = OnceLock::new();

pub fn global() -> Option<Arc<McpManager>> {
    GLOBAL.get().cloned()
}

impl McpManager {
    pub fn new() -> Result<Arc<Self>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("jarvis-mcp")
            .build()?;
        Ok(Arc::new(Self {
            runtime,
            calendar_peer: OnceLock::new(),
        }))
    }

    pub fn install_global(self: &Arc<Self>) {
        let _ = GLOBAL.set(self.clone());
    }

    /// Spawn the Google Calendar MCP server and complete the handshake.
    /// `credentials_path` points at the OAuth client JSON (Desktop app type)
    /// the user downloaded from Google Cloud Console.
    pub fn init_calendar(&self, credentials_path: &str) -> Result<()> {
        if !std::path::Path::new(credentials_path).exists() {
            bail!(
                "calendar credentials missing at {credentials_path} — see README for the Google OAuth setup steps"
            );
        }
        let creds = credentials_path.to_string();

        let peer = self.runtime.block_on(async move {
            let transport =
                TokioChildProcess::new(TokioCommand::new("npx").configure(|c| {
                    c.arg("-y")
                        .arg("@cocal/google-calendar-mcp")
                        .env("GOOGLE_OAUTH_CREDENTIALS", &creds);
                }))?;
            let service = ().serve(transport).await?;
            let peer = service.peer().clone();
            // List tools once so we log what's available (and warm the cache)
            match peer.list_all_tools().await {
                Ok(tools) => {
                    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
                    tracing::info!("mcp[calendar]: {} tools — {}", names.len(), names.join(", "));
                }
                Err(e) => tracing::warn!("mcp[calendar]: list_all_tools failed: {e}"),
            }
            Ok::<_, anyhow::Error>(peer)
        })?;

        let _ = self.calendar_peer.set(peer);
        tracing::info!("mcp[calendar]: connected");
        Ok(())
    }

    /// Call a tool on the calendar MCP server. Blocks until reply received.
    pub fn calendar_call(&self, tool_name: &str, args: Value) -> Result<Value> {
        let peer = self
            .calendar_peer
            .get()
            .ok_or_else(|| {
                anyhow!(
                    "Calendar MCP is not configured. Tell the user to set GOOGLE_OAUTH_CREDENTIALS in .env (see README)."
                )
            })?
            .clone();
        let args_map = args.as_object().cloned();
        let tool_name = tool_name.to_string();

        // Bound every MCP call so a slow/dead server subprocess can't make the
        // user wait the full brain timeout (and the same protects future MCP
        // servers like the Playwright browser one).
        let result = self.runtime.block_on(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                peer.call_tool(CallToolRequestParam {
                    name: tool_name.into(),
                    arguments: args_map,
                }),
            )
            .await
        });

        match result {
            Ok(Ok(r)) => Ok(extract_text_content(&r)),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => bail!("MCP call timed out after 15s — the server may be slow or stuck"),
        }
    }
}

/// Flatten MCP `CallToolResult` (a list of typed content blocks — text,
/// image, audio, resource refs) into something the brain can feed straight
/// back to the LLM. We keep just text; future: handle images via vision.
fn extract_text_content(result: &rmcp::model::CallToolResult) -> Value {
    let mut chunks: Vec<String> = Vec::new();
    for block in &result.content {
        // serialize each content block and pluck "text" if present
        let v = serde_json::to_value(block).unwrap_or(Value::Null);
        if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
            chunks.push(t.to_string());
        }
    }
    if chunks.is_empty() {
        // fall back to whole result as JSON so the LLM at least sees structure
        return serde_json::to_value(result).unwrap_or(json!({}));
    }
    json!({ "text": chunks.join("\n") })
}
