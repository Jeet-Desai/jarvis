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

/// Build the command that runs `npx`. On Windows `npx` is a `.cmd` batch
/// wrapper which CreateProcess can't spawn directly — go through `cmd /C`
/// (with the console window suppressed). Unix spawns it directly.
fn npx_command() -> TokioCommand {
    #[cfg(windows)]
    {
        // `npx` on Windows is a `.cmd` batch wrapper that CreateProcess can't
        // spawn directly, and the `cmd /C npx …` chain proved unreliable with
        // piped MCP stdio. Bypass both: run npm's own `npx-cli.js` under
        // node.exe directly — a single real process with clean stdin/stdout.
        // Fall back to `cmd /C npx` if the script can't be located.
        let mut c = match find_npx_cli() {
            Some((node, cli)) => {
                let mut c = TokioCommand::new(node);
                c.arg(cli);
                c
            }
            None => {
                let mut c = TokioCommand::new("cmd");
                c.arg("/C").arg("npx");
                c
            }
        };
        // Under `pnpm tauri dev` the process env carries pnpm's npm_config_*
        // variables (e.g. verify-deps-before-run), which npm warns about and
        // can choke on. Scrub them so npx runs with a clean npm config.
        for (k, _) in std::env::vars() {
            let lk = k.to_ascii_lowercase();
            if lk.starts_with("npm_config_")
                || lk.starts_with("npm_lifecycle_")
                || lk.starts_with("npm_package_")
                || lk.starts_with("pnpm_")
            {
                c.env_remove(&k);
            }
        }
        // Neutral cwd: from inside the Jarvis repo (a pnpm workspace), npx
        // mis-resolves the package bin ("'playwright-mcp' is not recognized")
        // because of the project .npmrc + pnpm node_modules layout. From a
        // neutral directory it runs from its own cache correctly.
        c.current_dir(std::env::temp_dir());
        crate::platform::hide_console_tokio(&mut c);
        c
    }
    #[cfg(not(windows))]
    {
        TokioCommand::new("npx")
    }
}

/// Build the command that runs the Playwright MCP server. The package is
/// pinned as a repo devDependency, so we can run its `cli.js` under node
/// DIRECTLY — no npx at runtime. This matters on Windows, where npx's
/// batch-shim bin resolution breaks under the pnpm/tauri dev environment
/// ("'playwright-mcp' is not recognized"); a plain `node cli.js` process has
/// none of that machinery. Falls back to `npx -y @playwright/mcp@latest`
/// when the local install isn't there (e.g. running outside the repo).
fn playwright_command() -> TokioCommand {
    let local_cli = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("node_modules").join("@playwright").join("mcp").join("cli.js"));
    if let Some(cli) = local_cli.filter(|p| p.is_file()) {
        let mut c = TokioCommand::new("node");
        c.arg(cli);
        crate::platform::hide_console_tokio(&mut c);
        return c;
    }
    let mut c = npx_command();
    c.arg("-y").arg("@playwright/mcp@latest");
    c
}

/// Locate node.exe and npm's npx-cli.js next to it (or via npm's prefix
/// layout). Returns (node path, npx-cli.js path).
#[cfg(windows)]
fn find_npx_cli() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let node = dir.join("node.exe");
        if !node.is_file() {
            continue;
        }
        // Standard Node-for-Windows install: npx-cli.js lives inside the
        // bundled npm package right next to node.exe.
        let cli = dir.join("node_modules").join("npm").join("bin").join("npx-cli.js");
        if cli.is_file() {
            return Some((node, cli));
        }
    }
    None
}

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
    /// Same, for the Playwright browser-control server (`@playwright/mcp`) — a
    /// DEDICATED Chrome Jarvis drives via the real DOM (accurate clicks), kept
    /// fully separate from the user's everyday browser.
    playwright: OnceLock<RunningService<RoleClient, ()>>,
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
            playwright: OnceLock::new(),
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
                TokioChildProcess::new(npx_command().configure(|c| {
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
        #[cfg(windows)]
        let python = dir.join(".venv").join("Scripts").join("python.exe");
        #[cfg(not(windows))]
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
                    crate::platform::hide_console_tokio(c);
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

    /// Spawn the Playwright browser-control server (`@playwright/mcp`) and finish
    /// the handshake. Two modes, selected by env `JARVIS_BROWSER_MODE`:
    ///
    /// - `extension` → drive the user's REAL browser via the "Playwright
    ///   Extension" (it bridges to their LIVE tabs over `chrome.debugger` — real
    ///   session, no relaunch, sidesteps the Chrome-136 default-profile CDP
    ///   block). The user installs the extension once and approves a tab on first
    ///   use (or sets `PLAYWRIGHT_MCP_EXTENSION_TOKEN`, inherited by the child, to
    ///   skip the approval dialog).
    /// - anything else (default) → Jarvis's OWN dedicated Chrome under a private
    ///   persistent `profile_dir`, separate from the user's everyday browser.
    ///   The proven fallback.
    ///
    /// stdio transport (no `--port`) is what the TokioChildProcess bridge speaks
    /// — same pattern as the calendar server. The browser connects lazily on the
    /// first browser tool call, so spawning here is cheap either way.
    pub fn init_playwright(&self, profile_dir: &str) -> Result<()> {
        let extension_mode = std::env::var("JARVIS_BROWSER_MODE")
            .map(|m| m.eq_ignore_ascii_case("extension"))
            .unwrap_or(false);
        let profile = profile_dir.to_string();
        // Pin @playwright/mcp's session output (snapshot/console/network dumps) to
        // an app-owned dir. Otherwise it writes a `.playwright-mcp/` folder into
        // the process CWD — which is the repo during dev and an unwritable path
        // (`/`) for a Finder-launched bundle.
        let output_dir = std::path::Path::new(&profile)
            .parent()
            .map(|p| p.join("playwright-output"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{profile}-output"));
        let service = self.runtime.block_on(async move {
            let transport =
                TokioChildProcess::new(playwright_command().configure(|c| {
                    c.arg("--output-dir")
                        .arg(&output_dir);
                    if extension_mode {
                        // Connect to the user's real browser tabs. The token (if
                        // present in Jarvis's env) is inherited by this child so
                        // @playwright/mcp can auto-approve connections.
                        c.arg("--extension");
                    } else {
                        c.arg("--browser")
                            .arg("chrome")
                            .arg("--user-data-dir")
                            .arg(&profile);
                    }
                }))?;
            let service = ().serve(transport).await?;
            match service.peer().list_all_tools().await {
                Ok(tools) => {
                    tracing::info!("mcp[playwright]: {} browser tools available", tools.len())
                }
                Err(e) => tracing::warn!("mcp[playwright]: list_all_tools failed: {e}"),
            }
            Ok::<_, anyhow::Error>(service)
        })?;

        let _ = self.playwright.set(service);
        if extension_mode {
            tracing::info!("mcp[playwright]: connected (EXTENSION mode — drives the user's real browser tabs)");
        } else {
            tracing::info!("mcp[playwright]: connected (dedicated Chrome, profile={profile_dir})");
        }
        Ok(())
    }

    /// True once the Playwright browser server is connected. The browser tool
    /// handlers use this to give a friendly "still starting up" message instead
    /// of a hard error if the brain reaches for a browser tool too early.
    pub fn playwright_ready(&self) -> bool {
        self.playwright.get().is_some()
    }

    /// Call a tool on the Playwright browser server. Blocks until reply.
    /// 45s budget: a `browser_navigate` to a slow site can take a while (the
    /// server's own navigation timeout is 60s), but we cap below the brain
    /// timeout so a stuck page can't wedge the turn.
    pub fn playwright_call(&self, tool_name: &str, args: Value) -> Result<Value> {
        let peer = self
            .playwright
            .get()
            .ok_or_else(|| {
                anyhow!(
                    "The browser isn't ready yet — Playwright is still starting up (it spins up a few seconds after launch). Tell the user to try again in a moment."
                )
            })?
            .peer()
            .clone();
        let args_map = args.as_object().cloned();
        let tool_name = tool_name.to_string();

        let result = self.runtime.block_on(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(45),
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
            Err(_) => bail!("Browser action timed out after 45s — the page may be slow or stuck"),
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
