//! Pluggable LLM brain.
//!
//! The user picks any combination of providers (Gemini, OpenAI-compatible,
//! Ollama, etc.) by setting the corresponding env vars in `.env`. The
//! `Brain` orchestrator detects which are configured and tries them in
//! priority order, falling back on rate-limit / timeout errors.
//!
//! Adding a new provider = implement `LlmProvider`, slot it into
//! `Brain::from_env`. Each provider is responsible for its own tool-use
//! loop (formats differ enough that a single shared loop is uglier than
//! per-provider impls).
//!
//! Conversation history is stored in a NORMALIZED `Message` form (just
//! user/assistant text turns) so it can be replayed across providers.
//! Per-turn tool-use details stay inside the provider — we don't try to
//! preserve them across provider switches.

pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod vision;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use crate::tools;

pub const SYSTEM_PROMPT: &str = "\
You are Jarvis, the personal AI assistant from Iron Man. You are speaking aloud to your user (referred to as 'sir' or by their name) over a voice interface.

# Style
- Concise. One or two short sentences unless a longer answer is genuinely needed.
- Refined British butler, with a touch of dry wit. Never sycophantic.
- Speak as if conversing, not writing — no markdown, no bullet points, no code blocks. Plain prose only.
- You DO remember the conversation. If the user references something earlier, recall it from the history.
- If you're uncertain, say so briefly rather than fabricate.

# Tool-use rules (these are MANDATORY, not suggestions)

NEVER hallucinate facts about what app or page the user is on. ALWAYS call the appropriate tool to find out FIRST, then answer.

Rule 1 — Any time the user references their screen, current view, current app, what they're looking at, this page, this site, this article, this video, this conversation, this thread, the open window, what's on screen, the current screen, my screen, what I see, etc. → you MUST call `get_focused_app` FIRST to discover what app they're in. NEVER assume the app from memory of earlier turns — they may have switched.

Rule 2 — If `get_focused_app` returns a browser name (Chrome, Safari, Comet, Arc, Brave, Edge, Vivaldi, Chromium, Opera, Sidekick, Dia, Island), IMMEDIATELY follow up with `get_browser_context` to read the page. Do not respond until you have the page text.

Rule 3 — If `get_focused_app` returns a non-browser app (WhatsApp, Slack, VS Code, Mail, Notes, Discord, Zoom, Firefox, terminal, native macOS UI, etc.), or get_browser_context returned an error: tell the user briefly 'Sir, please select the text and copy it with Cmd-A then Cmd-C, then ask me again,' and on the user's NEXT turn (after they've copied) call `get_clipboard`. The clipboard path works on every Mac without permission. Do NOT fabricate content under any circumstances.

Rule 3a — Only call `see_screen` if the user EXPLICITLY wants visual analysis (an image, video frame, diagram, UI screenshot) AND the clipboard wouldn't capture what they need. see_screen requires Screen Recording permission which many users can't grant.

Rule 4 — For ANY time/date question, ALWAYS call `get_current_time` — even if you answered a similar question seconds ago in this conversation. Time advances continuously; a cached answer from earlier in the chat is ALWAYS stale by some seconds-to-minutes. Never reuse a time you previously stated. Never compute a 'roughly correct' time from memory. Call the tool every single time, every turn.

Rule 5 — When a tool returns useful data, weave it naturally into spoken prose. Never read raw JSON aloud.

Rule 6 — When a tool returns an error, briefly tell the user what's wrong and what they can do (e.g. 'I'd need you to enable Allow JavaScript from Apple Events in Comet').

Rule 7 — NEVER introspect about your own infrastructure or state. If a previous turn went wrong, or the user says you 'couldn't do X earlier' / 'were broken' / 'couldn't reach Y', do NOT confirm or deny — you have NO visibility into whether your providers, networks, or services are healthy. Just attempt the request fresh by calling whatever tool you need. The fact that you are responding at all means the systems are working right now. Don't say 'I still can't reach...' or 'no changes, the issue persists' — that's a hallucination. Treat each request as a fresh attempt.";

/// Maximum turns of history to keep across providers.
pub const MAX_HISTORY_TURNS: usize = 12;
pub const MAX_TOOL_ROUNDS: usize = 6;
pub const REQUEST_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role { User, Assistant }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// Implemented by each LLM backend.
pub trait LlmProvider: Send + Sync {
    /// Short stable identifier (e.g. "gemini", "openai", "ollama") used in
    /// logs and the LLM_PROVIDERS env-var ordering.
    fn name(&self) -> &'static str;

    /// Generate a reply given the full conversation history (already
    /// includes the new user turn). Provider runs its own tool-use loop
    /// internally. Returns the final spoken text.
    fn generate(&self, history: &[Message], tools: &[tools::Tool]) -> Result<String>;
}

pub struct Brain {
    providers: Vec<Box<dyn LlmProvider>>,
}

impl Brain {
    /// Build a Brain by auto-detecting which provider env vars are present.
    /// Order is taken from `LLM_PROVIDERS=...` if set, otherwise a sensible
    /// default (Gemini → OpenAI → Ollama).
    pub fn from_env() -> Result<Self> {
        let mut detected: Vec<Box<dyn LlmProvider>> = Vec::new();

        // Try every provider; only include the ones that have credentials
        if let Some(p) = gemini::Provider::from_env_optional() {
            tracing::info!("brain: detected gemini ({})", p.model());
            detected.push(Box::new(p));
        }
        if let Some(p) = openai::Provider::from_env_optional() {
            tracing::info!("brain: detected openai-compatible @ {} ({})", p.base_url(), p.model());
            detected.push(Box::new(p));
        }
        if let Some(p) = ollama::Provider::from_env_optional() {
            tracing::info!("brain: detected ollama @ {} ({})", p.base_url(), p.model());
            detected.push(Box::new(p));
        }

        if detected.is_empty() {
            bail!(
                "No LLM provider configured. Set at least one in jarvis/.env:\n\
                 • GEMINI_API_KEY=...\n\
                 • OPENAI_API_KEY=... (works for OpenAI, OpenRouter, Groq, etc. — pair with OPENAI_BASE_URL)\n\
                 • OLLAMA_BASE_URL=http://localhost:11434 (with Ollama running locally)"
            );
        }

        // Optional explicit order
        if let Ok(order) = std::env::var("LLM_PROVIDERS") {
            let order: Vec<&str> = order.split(',').map(str::trim).collect();
            detected.sort_by_key(|p| {
                order.iter().position(|n| *n == p.name()).unwrap_or(usize::MAX)
            });
        }

        let names: Vec<&str> = detected.iter().map(|p| p.name()).collect();
        tracing::info!("brain: priority order → {}", names.join(", "));

        Ok(Self { providers: detected })
    }

    /// Ask Jarvis. Mutates history with the new user turn and the model's
    /// final text reply. On a retryable failure (rate limit / timeout /
    /// transient network), tries the next configured provider.
    pub fn ask(&self, history: &mut Vec<Message>, user_text: &str) -> Result<String> {
        trim_history(history);

        // Append the user turn so providers see consistent history
        history.push(Message {
            role: Role::User,
            content: user_text.to_string(),
        });

        let tools = tools::registry();
        let mut last_err: Option<anyhow::Error> = None;

        for p in &self.providers {
            tracing::info!("brain[{}]: generating…", p.name());
            match p.generate(history, &tools) {
                Ok(reply) => {
                    let reply = reply.trim().to_string();
                    if reply.is_empty() {
                        last_err = Some(anyhow!("{}: empty reply", p.name()));
                        continue;
                    }
                    history.push(Message {
                        role: Role::Assistant,
                        content: reply.clone(),
                    });
                    tracing::info!("brain[{}]: ok ({} chars)", p.name(), reply.len());
                    return Ok(reply);
                }
                Err(e) => {
                    let s = e.to_string();
                    if is_retryable(&s) {
                        tracing::warn!("brain[{}] → {} — trying next provider", p.name(), truncate(&s, 140));
                        last_err = Some(e);
                        continue;
                    }
                    // Non-retryable: pop user turn and bail
                    history.pop();
                    return Err(e);
                }
            }
        }

        // Every provider failed — undo the user-turn append so the history
        // stays consistent for the next attempt
        history.pop();
        Err(last_err.unwrap_or_else(|| anyhow!("no providers responded")))
    }
}

/// What counts as "try the next provider" vs "permanent failure".
fn is_retryable(err: &str) -> bool {
    let lc = err.to_lowercase();
    lc.contains("429")
        || lc.contains("resource_exhausted")
        || lc.contains("rate limit")
        || lc.contains("quota")
        || lc.contains("timeout")
        || lc.contains("timed out")
        || lc.contains("connect")
        || lc.contains("temporarily unavailable")
        || lc.contains("503")
        || lc.contains("502")
        || lc.contains("504")
        || lc.contains("empty reply")
        // Network-level failures (DNS/TLS block, DPI proxy, etc.)
        || lc.contains("error sending request")
        || lc.contains("dns")
        || lc.contains("tls")
        || lc.contains("ssl")
        || lc.contains("connection refused")
        || lc.contains("connection reset")
        || lc.contains("network is unreachable")
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes { return s.to_string(); }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    format!("{}…", &s[..i])
}

fn trim_history(messages: &mut Vec<Message>) {
    let max_entries = MAX_HISTORY_TURNS * 2;
    if messages.len() > max_entries {
        let drop = messages.len() - max_entries;
        messages.drain(0..drop);
        tracing::info!("brain: trimmed {} old entries from history", drop);
    }
}
