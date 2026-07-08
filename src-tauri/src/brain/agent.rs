//! Agent-driver brain — drive an external CLI coding agent as Jarvis's brain.
//!
//! Instead of calling an HTTP LLM API (Gemini/OpenAI/Ollama), this provider runs
//! a headless CLI agent that connects to Jarvis's own MCP server (see
//! `mcp_server.rs`), reasons, and calls Jarvis's gated tools — then returns its
//! final spoken text. The agent runs on the user's *subscription* (e.g. via the `claude` CLI), so there's no per-token API key needed.
//!
//! It's deliberately agent-agnostic: `Driver` is the seam. `ClaudeCodeDriver`
//! is the only implementation today; Codex (`codex exec`) and Kimi
//! (`kimi --print`) are future drop-ins — each just needs to (1) run headless,
//! (2) point at an MCP server by URL + bearer token, (3) auto-approve that
//! server's tools, and (4) print a final answer. The MCP server, the safety
//! gate, and everything below stay identical across agents.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

use super::{LlmProvider, Message, Role};
use crate::mcp_server;

/// How an agent CLI is invoked. One impl per agent; the rest of Jarvis is blind
/// to which agent is in use.
trait Driver: Send + Sync {
    /// Stable id, e.g. "claude-code".
    fn name(&self) -> &'static str;
    /// Run one turn headlessly: given the rendered prompt, the system prompt, and
    /// the Jarvis MCP endpoint, return the agent's final text.
    fn run(&self, prompt: &str, system: &str, endpoint: &mcp_server::Endpoint) -> Result<String>;
}

/// The `LlmProvider` wrapper that plugs an agent `Driver` into `Brain`.
pub struct Provider {
    driver: Box<dyn Driver>,
    model: String,
}

impl Provider {
    /// Build the agent-driver provider if an agent CLI is available and not
    /// disabled. Today: Claude Code (the `claude` binary on PATH). Opt out with
    /// `JARVIS_DISABLE_AGENT_BRAIN=1`; pick the model with `JARVIS_AGENT_MODEL`.
    pub fn from_env_optional() -> Option<Self> {
        if env_flag("JARVIS_DISABLE_AGENT_BRAIN") {
            return None;
        }
        // Future: select the driver via JARVIS_AGENT (claude-code | codex | kimi).
        // For now only claude-code exists; require its CLI on PATH.
        if !binary_on_path("claude") {
            return None;
        }
        let model = std::env::var("JARVIS_AGENT_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sonnet".to_string());
        // Effort drives how hard the agent thinks/loops. Claude Code defaults to
        // xhigh, which on a voice loop means minutes of locate→click retries.
        // Default to `low` for snappy turns; bump via JARVIS_AGENT_EFFORT.
        let effort = std::env::var("JARVIS_AGENT_EFFORT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "low".to_string());
        let driver = Box::new(ClaudeCodeDriver {
            model: model.clone(),
            effort: effort.clone(),
        });
        Some(Self {
            model: format!("{} (Claude Code subscription, effort={})", model, effort),
            driver,
        })
    }
}

impl LlmProvider for Provider {
    fn name(&self) -> &'static str {
        self.driver.name()
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn generate(&self, history: &[Message], _tools: &[crate::tools::Tool]) -> Result<String> {
        // The agent calls Jarvis tools over the local MCP server. If it isn't up,
        // signal a retryable failure so the brain falls back to API providers.
        let endpoint = mcp_server::endpoint()
            .ok_or_else(|| anyhow!("Jarvis MCP server not running — brain temporarily unavailable"))?;

        let prompt = build_prompt(history);
        let system = format!("{}{}", super::system_prompt(), AGENT_SYSTEM_SUFFIX);
        self.driver.run(&prompt, &system, &endpoint)
    }
}

/// Appended to Jarvis's system prompt so the agent knows its action tools arrive
/// over MCP (prefixed names) and that its job is one spoken reply.
const AGENT_SYSTEM_SUFFIX: &str = "\n\n# Operating as the brain via MCP\n\
Your computer-control tools are provided over MCP and named with a `mcp__jarvis__` prefix \
(e.g. `mcp__jarvis__get_focused_app`, `mcp__jarvis__get_browser_context`, `mcp__jarvis__type_text`, \
`mcp__jarvis__click_element`, `mcp__jarvis__run_shell`). They are the SAME tools described above — \
the prefix doesn't change their purpose; use them to observe and act. Use ONLY these `mcp__jarvis__` \
tools; you have no other file/shell access. End your turn with a single short spoken reply for the \
user (plain prose, no markdown) — that final message is read aloud.\n\
\n\
# Be fast and decisive (this is a real-time VOICE assistant)\n\
Latency matters more than thoroughness. Use the FEWEST tool calls that get the job done, and answer \
as soon as you can. Hard limits:\n\
- Do NOT repeat the same `locate_on_screen`/`see_screen`/`click` hunt more than TWICE for one target. \
If you can't find a UI element or a click clearly didn't take effect after two attempts, STOP \
retrying — tell the user briefly what you see and ask how they'd like to proceed. Never grind in a \
locate→click→re-check loop.\n\
- Prefer the dedicated tools (`focus_app`, `click_element` by label, `send_shortcut`, `type_text`, \
`get_browser_context`) over `run_shell`/`osascript` for controlling apps — they're more reliable and \
the user can see what you did. Reach for `run_shell` only for genuine shell work.\n\
- For a plain question or chit-chat, just answer — don't call tools at all.\n\
\n\
# Dev agents run in the BACKGROUND\n\
`mcp__jarvis__spawn_dev_agent` returns immediately and the dev agent keeps working on its own \
(you can omit project_dir to start a brand-new project under ~/claude). After you spawn one, give the \
user a ONE-LINE confirmation (its name + what it'll build) and END your turn. Do NOT sit in a \
get_agent_status/wait loop watching it build — that blocks the whole turn until it times out. If the \
user later asks how it's going, call get_agent_status or list_dev_agents ONCE and report.";

/// Render the normalized history into a single prompt. The last entry is the new
/// user turn; earlier turns are prior context.
fn build_prompt(history: &[Message]) -> String {
    let Some((last, prior)) = history.split_last() else {
        return String::new();
    };
    let latest = last.content.trim();
    if prior.is_empty() {
        return latest.to_string();
    }
    let mut out = String::from("Conversation so far:\n");
    for m in prior {
        let who = match m.role {
            Role::User => "User",
            Role::Assistant => "Jarvis",
        };
        out.push_str(who);
        out.push_str(": ");
        out.push_str(m.content.trim());
        out.push('\n');
    }
    out.push_str("\nNew user message (respond to this as Jarvis):\n");
    out.push_str(latest);
    out
}

// ──────────────────────────────────────────────────────────────────────────────
// Claude Code driver — `claude -p` headless, on the user's Claude subscription.
// ──────────────────────────────────────────────────────────────────────────────

struct ClaudeCodeDriver {
    model: String,
    effort: String,
}

impl Driver for ClaudeCodeDriver {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn run(&self, prompt: &str, system: &str, endpoint: &mcp_server::Endpoint) -> Result<String> {
        // Inline MCP config pointing the agent at Jarvis's own tool server.
        let mcp_config = json!({
            "mcpServers": {
                mcp_server::SERVER_NAME: {
                    "type": "http",
                    "url": endpoint.url,
                    "headers": { "Authorization": format!("Bearer {}", endpoint.token) }
                }
            }
        })
        .to_string();

        let mut cmd = claude_command();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--system-prompt")
            .arg(system)
            .arg("--model")
            .arg(&self.model)
            .arg("--effort")
            .arg(&self.effort)
            .arg("--mcp-config")
            .arg(&mcp_config)
            .arg("--strict-mcp-config") // ignore any user/project MCP config
            // Allowlist ONLY Jarvis's tools: in print mode this auto-approves
            // them and denies Claude Code's own built-ins (Bash/Edit/Read), so
            // the agent can act only through Jarvis's gate.
            .arg("--allowed-tools")
            .arg(format!("mcp__{}__*", mcp_server::SERVER_NAME))
            .arg("--output-format")
            .arg("json")
            // Neutral cwd so a stray CLAUDE.md (e.g. the Jarvis repo's) isn't
            // auto-loaded into the agent's context.
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Inherit env so the CLI uses the user's logged-in subscription. We must
        // NOT inject ANTHROPIC_API_KEY (would switch to metered API billing);
        // Jarvis's env has none.

        let timeout = std::env::var("JARVIS_AGENT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(90)
            .clamp(15, 600);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow!("failed to launch `claude` ({e}) — brain temporarily unavailable")
        })?;
        // Close stdin so the CLI never blocks waiting on it.
        drop(child.stdin.take());
        let pid = child.id();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });

        let output = match rx.recv_timeout(Duration::from_secs(timeout)) {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => bail!("claude exec error: {e} (brain temporarily unavailable)"),
            Err(_) => {
                kill_pid(pid);
                bail!("claude timed out after {timeout}s (brain temporarily unavailable)");
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            // Non-zero exit: surface stderr; mark retryable so we can fall back.
            let detail = stderr.trim();
            let detail = if detail.is_empty() { stdout.trim() } else { detail };
            bail!(
                "claude exited with {}: {} (brain temporarily unavailable)",
                output.status.code().unwrap_or(-1),
                super::truncate(detail, 300)
            );
        }

        parse_result(&stdout).ok_or_else(|| {
            anyhow!(
                "couldn't parse claude output: {} (brain temporarily unavailable)",
                super::truncate(stdout.trim(), 300)
            )
        })
    }
}

/// Build the base `claude` invocation. On Windows an npm-installed `claude`
/// is a `.cmd` batch wrapper that CreateProcess can't run directly — if only
/// the `.cmd`/`.bat` form exists on PATH, route through `cmd /C` (console
/// window suppressed). A native `claude.exe` (or unix binary) spawns directly.
fn claude_command() -> std::process::Command {
    #[cfg(windows)]
    {
        let direct = std::env::var("PATH")
            .ok()
            .map(|p| {
                std::env::split_paths(&p)
                    .any(|dir| dir.join("claude.exe").is_file() || dir.join("claude").is_file())
            })
            .unwrap_or(false);
        let mut cmd = if direct {
            std::process::Command::new("claude")
        } else {
            let mut c = std::process::Command::new("cmd");
            c.arg("/C").arg("claude");
            c
        };
        crate::platform::hide_console(&mut cmd);
        cmd
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("claude")
    }
}

/// Pull the final assistant text out of `claude -p --output-format json`.
/// Shape: `{ "type":"result", "subtype":"success", "is_error":false,
/// "result":"<final text>", ... }`. Returns None on error/empty.
fn parse_result(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        return None;
    }
    let text = v.get("result").and_then(|r| r.as_str())?.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ── small helpers ─────────────────────────────────────────────────────────────

fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Is `name` executable from PATH? On Windows also checks the PATHEXT-style
/// variants (`.exe`, `.cmd`, `.bat`) that npm-installed CLIs use.
fn binary_on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        if dir.join(name).is_file() {
            return true;
        }
        if cfg!(windows) {
            for ext in ["exe", "cmd", "bat"] {
                if dir.join(format!("{name}.{ext}")).is_file() {
                    return true;
                }
            }
        }
        false
    })
}

fn kill_pid(pid: u32) {
    crate::platform::kill_pid(pid);
}
