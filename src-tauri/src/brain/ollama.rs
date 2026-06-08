//! Ollama (local LLM) provider — runs models on the user's own machine.
//!
//! Ollama exposes an OpenAI-compatible /v1/chat/completions endpoint, so
//! we use that path (rather than the older /api/chat). This means tool-use
//! works the same way as `openai.rs` — but pointed at localhost.
//!
//! Configured via env:
//!   OLLAMA_BASE_URL    (default: http://localhost:11434)
//!   OLLAMA_MODEL       (default: qwen2.5:7b — strong on tool-use)
//!
//! Provider is auto-included only if OLLAMA_BASE_URL is explicitly set OR
//! a default-port Ollama daemon answers a quick health ping at startup.

use anyhow::{anyhow, bail, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::OnceLock;

use crate::tools;
use super::{LlmProvider, Message, Role, MAX_TOOL_ROUNDS, REQUEST_TIMEOUT_SECS, SYSTEM_PROMPT};

const DEFAULT_BASE_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL:    &str = "qwen2.5:7b";

pub struct Provider {
    base_url: String,
    model:    String,
    client:   reqwest::blocking::Client,
}

impl Provider {
    pub fn from_env_optional() -> Option<Self> {
        // Include if explicit OLLAMA_BASE_URL is set, OR if the default-port
        // daemon is running. This way users with Ollama installed get it for
        // free as a fallback, but it doesn't try-and-fail loudly otherwise.
        let explicit = std::env::var("OLLAMA_BASE_URL").ok().filter(|s| !s.trim().is_empty());
        let base_url = explicit.unwrap_or_else(|| DEFAULT_BASE_URL.into())
            .trim_end_matches('/').to_string();

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build().ok()?;

        // Cheap health probe — Ollama responds to GET / with a banner
        let alive = client.get(&base_url).send().map(|r| r.status().is_success()).unwrap_or(false);
        if !alive { return None; }

        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        Some(Self { base_url, model, client })
    }

    pub fn base_url(&self) -> &str { &self.base_url }
}

impl LlmProvider for Provider {
    fn name(&self) -> &'static str { "ollama" }
    fn model(&self) -> &str { &self.model }

    fn generate(&self, history: &[Message], tools_reg: &[tools::Tool]) -> Result<String> {
        // OpenAI-compatible messages format
        let mut messages: Vec<Value> = vec![json!({
            "role": "system",
            "content": SYSTEM_PROMPT,
        })];
        for m in history {
            let role = match m.role { Role::User => "user", Role::Assistant => "assistant" };
            messages.push(json!({ "role": role, "content": m.content }));
        }

        let tools_payload: Vec<Value> = tools_reg.iter().map(|t| {
            let d = &t.declaration;
            json!({
                "type": "function",
                "function": {
                    "name":        d.get("name"),
                    "description": d.get("description"),
                    "parameters":  d.get("parameters"),
                }
            })
        }).collect();

        let url = format!("{}/v1/chat/completions", self.base_url);

        // Track which tools the model called this turn, so the hallucination
        // guard can verify any factual claims (times, calendar events,
        // weather) were actually grounded in a tool response.
        let mut tools_called: HashSet<String> = HashSet::new();

        for round in 0..MAX_TOOL_ROUNDS {
            let body = json!({
                "model":       self.model,
                "messages":    messages,
                "tools":       tools_payload,
                "tool_choice": "auto",
                "stream":      false,
                "options": {
                    "temperature": 0.7,
                    // Reasoning models (gpt-oss, deepseek-r1, qwen3) spend
                    // hidden thinking tokens against this budget BEFORE
                    // emitting the visible answer; 512 could be consumed
                    // entirely by reasoning on a tool-use turn, leaving empty
                    // content (and a spurious fallback to the next provider).
                    "num_predict": 2048,
                }
            });

            let resp = self.client.post(&url)
                .json(&body)
                .send()?;
            let status = resp.status();
            let text   = resp.text()?;
            if !status.is_success() {
                bail!("ollama {}: {}", status, truncate(&text, 240));
            }

            let parsed: ChatCompletionResponse = serde_json::from_str(&text)
                .map_err(|e| anyhow!("ollama parse: {e} — body: {}", truncate(&text, 240)))?;
            let choice = parsed.choices.into_iter().next()
                .ok_or_else(|| anyhow!("ollama: no choices"))?;
            let msg = choice.message;

            let tool_calls = msg.tool_calls.unwrap_or_default();
            if tool_calls.is_empty() {
                let reply = msg.content.unwrap_or_default();
                if let Some(reason) = guard_reply(&reply, &tools_called) {
                    tracing::warn!(
                        "ollama: hallucination guard blocked reply ({reason}) — original: {:?}",
                        truncate(&reply, 160),
                    );
                    return Ok(
                        "I don't have a confirmed source for that, sir. \
                         Could you tell me specifically what you'd like me to check?"
                            .into()
                    );
                }
                return Ok(reply);
            }

            let assistant_turn = json!({
                "role": "assistant",
                "content": msg.content.unwrap_or_default(),
                "tool_calls": tool_calls.iter().map(|tc| json!({
                    "id":   tc.id,
                    "type": "function",
                    "function": { "name": tc.function.name, "arguments": tc.function.arguments },
                })).collect::<Vec<_>>(),
            });
            messages.push(assistant_turn);

            for tc in tool_calls {
                let args: Value = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| json!({}));
                tracing::info!("ollama: tool call → {}({})", tc.function.name, args);
                tools_called.insert(tc.function.name.clone());
                let result = run_tool(tools_reg, &tc.function.name, &args);
                tracing::info!("ollama: tool result ← {result}");
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": result.to_string(),
                }));
            }
            tracing::debug!("ollama: round {round} → tool loop");
        }
        bail!("ollama: exceeded MAX_TOOL_ROUNDS")
    }
}

fn run_tool(tools_reg: &[tools::Tool], name: &str, args: &Value) -> Value {
    let Some(t) = tools_reg.iter().find(|t| t.name == name) else {
        return json!({ "error": format!("unknown tool: {name}") });
    };
    match (t.handler)(args) {
        Ok(v)  => v,
        Err(e) => json!({ "error": e.to_string() }),
    }
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes { return s.to_string(); }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    format!("{}…", &s[..i])
}

// ── Wire types ────────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct ChatCompletionResponse { choices: Vec<Choice> }
#[derive(Debug, Deserialize)]
struct Choice { message: AssistantMessage }
#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)] content:    Option<String>,
    #[serde(default)] tool_calls: Option<Vec<ToolCall>>,
}
#[derive(Debug, Deserialize)]
struct ToolCall { id: String, function: ToolCallFunction }
#[derive(Debug, Deserialize)]
struct ToolCallFunction { name: String, arguments: String }

// ─── hallucination guard ───────────────────────────────────────────────────
//
// Coding-tuned LLMs (qwen3-coder, etc.) are notorious for confidently
// asserting facts they have no source for — "the time is 10:47 AM", "you
// have a meeting at 11:30", "weather is 72°F" — even when explicit system
// rules forbid it. Stronger prompts help marginally but the model still
// drifts.
//
// This guard runs over the final assistant text. If the reply contains
// claims that should have been grounded by a tool call this turn AND the
// matching tool wasn't actually called, we replace the reply with a
// refusal. Better to admit we don't know than to lie convincingly.
//
// Patterns are anchored on the specific failure modes we've observed,
// not a fuzzy "looks suspicious" heuristic — false positives erode trust
// in the assistant.

struct GuardPatterns {
    /// Explicit clock time: 10:47, 12:30 AM, 9:15pm, 9:15 p.m.
    clock: Regex,
    /// Friendly time-of-day greetings ("good morning/afternoon/evening/night")
    /// — these imply knowledge of the current time.
    greeting: Regex,
    /// Weather words (and there's no weather tool in Jarvis right now —
    /// any weather claim is automatically a hallucination).
    weather: Regex,
    /// Calendar/meeting language with specifics — generic "let's set up a
    /// meeting" won't trip this, "you have a meeting at X" will.
    calendar: Regex,
}

fn patterns() -> &'static GuardPatterns {
    static P: OnceLock<GuardPatterns> = OnceLock::new();
    P.get_or_init(|| GuardPatterns {
        clock:    Regex::new(r"(?i)\b\d{1,2}:\d{2}\s*(?:am|pm|a\.m\.|p\.m\.)?\b").unwrap(),
        greeting: Regex::new(r"(?i)\bgood\s+(?:morning|afternoon|evening|night)\b").unwrap(),
        weather:  Regex::new(
            r"(?i)\b(?:weather|cloudy|sunny|rainy?|snowy?|fahrenheit|celsius|temperature|°\s*[fc]|forecast|humidity|breezy|windy)\b"
        ).unwrap(),
        calendar: Regex::new(
            r"(?i)\b(?:meeting|appointment|scheduled|calendar\s+event|your\s+(?:agenda|schedule))\b"
        ).unwrap(),
    })
}

/// Returns Some(reason) if the reply has unsourced claims, None if clean.
fn guard_reply(reply: &str, tools_called: &HashSet<String>) -> Option<&'static str> {
    let p = patterns();
    let time_called = tools_called.contains("get_current_time");
    let cal_called  = tools_called.contains("list_calendar_events")
                   || tools_called.contains("create_calendar_event");

    if p.weather.is_match(reply) {
        return Some("weather claim — no weather tool exists");
    }
    if p.clock.is_match(reply) && !time_called {
        return Some("specific clock time without get_current_time");
    }
    if p.greeting.is_match(reply) && !time_called {
        return Some("time-of-day greeting without get_current_time");
    }
    if p.calendar.is_match(reply) && !cal_called {
        return Some("calendar/meeting claim without list_calendar_events");
    }
    None
}
