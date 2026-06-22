//! MCP (Model Context Protocol) client manager.
//!
//! Spawns external MCP servers as subprocesses, holds long-lived JSON-RPC
//! connections, exposes a sync API for the rest of Jarvis (which lives in
//! sync land via reqwest::blocking, parking_lot, etc).
//!
//! Today: the Google Calendar server (@cocal/google-calendar-mcp) and the
//! Jarvis dev-agent orchestrator (the local Python sidecar in `orchestrator/`).
//! Easy to add more — wrap another `OnceLock<Peer>` on `McpManager`.
//!
//! Async ↔ sync bridge: a dedicated multi-thread tokio runtime is built
//! once and owned by the manager. All MCP calls run `runtime.block_on(...)`
//! from the calling sync thread. Safe because the runtime has its own
//! worker threads — we're not blocking the runtime that's blocking on us.

use anyhow::{anyhow, bail, Result};
use rmcp::{
    model::CallToolRequestParam,
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
    RoleClient, ServiceExt,
};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use tokio::process::Command as TokioCommand;
use tokio::runtime::Runtime;

pub struct McpManager {
    runtime: Runtime,
    /// Long-lived RunningService for the calendar MCP subprocess. We keep the
    /// whole `RunningService` (not just its `peer()`) ALIVE on purpose: it owns
    /// a `DropGuard` that cancels the connection the instant it's dropped, which
    /// would tear down the subprocess transport right after the handshake
    /// ("Transport closed"). Hold it for the life of the manager.
    calendar: OnceLock<RunningService<RoleClient, ()>>,
    /// Same, for the dev-agent orchestrator sidecar (`orchestrator/`).
    orchestrator: OnceLock<RunningService<RoleClient, ()>>,
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
            calendar: OnceLock::new(),
            orchestrator: OnceLock::new(),
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

        let service = self.runtime.block_on(async move {
            let transport =
                TokioChildProcess::new(TokioCommand::new("npx").configure(|c| {
                    c.arg("-y")
                        .arg("@cocal/google-calendar-mcp")
                        .env("GOOGLE_OAUTH_CREDENTIALS", &creds);
                }))?;
            let service = ().serve(transport).await?;
            // List tools once so we log what's available (and warm the cache)
            match service.peer().list_all_tools().await {
                Ok(tools) => {
                    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
                    tracing::info!("mcp[calendar]: {} tools — {}", names.len(), names.join(", "));
                }
                Err(e) => tracing::warn!("mcp[calendar]: list_all_tools failed: {e}"),
            }
            Ok::<_, anyhow::Error>(service)
        })?;

        let _ = self.calendar.set(service);
        tracing::info!("mcp[calendar]: connected");
        Ok(())
    }

    /// Call a tool on the calendar MCP server. Blocks until reply received.
    pub fn calendar_call(&self, tool_name: &str, args: Value) -> Result<Value> {
        let peer = self
            .calendar
            .get()
            .ok_or_else(|| {
                anyhow!(
                    "Calendar MCP is not configured. Tell the user to set GOOGLE_OAUTH_CREDENTIALS in .env (see README)."
                )
            })?
            .peer()
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

    /// Spawn the dev-agent orchestrator sidecar and complete the handshake.
    /// `dir` is the absolute path to the `orchestrator/` package; we launch its
    /// own venv interpreter (`.venv/bin/python server.py`) so it gets the
    /// `claude-agent-sdk` deps regardless of the system Python. The subprocess
    /// inherits Jarvis's environment (so the agent SDK sees ANTHROPIC_API_KEY).
    pub fn init_orchestrator(&self, dir: &str) -> Result<()> {
        let dir = std::path::Path::new(dir);
        let python = dir.join(".venv/bin/python");
        let server = dir.join("server.py");
        if !python.exists() {
            bail!(
                "orchestrator venv missing at {} — run `cd orchestrator && uv venv --python 3.12 && uv pip install claude-agent-sdk mcp`",
                python.display()
            );
        }
        if !server.exists() {
            bail!("orchestrator server.py missing at {}", server.display());
        }
        let python = python.to_string_lossy().into_owned();
        let cwd = dir.to_string_lossy().into_owned();

        let service = self.runtime.block_on(async move {
            let transport =
                TokioChildProcess::new(TokioCommand::new(&python).configure(|c| {
                    c.arg("server.py").current_dir(&cwd);
                }))?;
            let service = ().serve(transport).await?;
            match service.peer().list_all_tools().await {
                Ok(tools) => {
                    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
                    tracing::info!(
                        "mcp[orchestrator]: {} tools — {}",
                        names.len(),
                        names.join(", ")
                    );
                }
                Err(e) => tracing::warn!("mcp[orchestrator]: list_all_tools failed: {e}"),
            }
            Ok::<_, anyhow::Error>(service)
        })?;

        let _ = self.orchestrator.set(service);
        tracing::info!("mcp[orchestrator]: connected");
        Ok(())
    }

    /// True once the orchestrator sidecar is connected. Used by the background
    /// poll task to decide whether there's anything to poll.
    pub fn orchestrator_ready(&self) -> bool {
        self.orchestrator.get().is_some()
    }

    /// Call a tool on the orchestrator sidecar. Blocks until reply received.
    /// A generous 60s timeout: `spawn_dev_agent` does a `git worktree add` plus
    /// an SDK `connect()` before returning, which can take a while on first use.
    /// `qa_web_app` is heavier still — it serves the app, takes two Playwright
    /// screenshots, and runs a Claude vision judge — so it gets a longer budget.
    /// (P3b moves QA to an async task so it won't block the brain thread at all.)
    pub fn orchestrator_call(&self, tool_name: &str, args: Value) -> Result<Value> {
        let peer = self
            .orchestrator
            .get()
            .ok_or_else(|| {
                anyhow!(
                    "Dev-agent orchestrator is not running. Make sure the `orchestrator/` venv is set up (see orchestrator/README.md)."
                )
            })?
            .peer()
            .clone();
        let args_map = args.as_object().cloned();
        let tool_name = tool_name.to_string();
        let timeout_secs: u64 = if tool_name == "qa_web_app" { 180 } else { 60 };

        let result = self.runtime.block_on(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
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
            Err(_) => bail!("Orchestrator call timed out after {timeout_secs}s — the sidecar may be stuck"),
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
