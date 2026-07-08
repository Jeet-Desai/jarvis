//! OpenAI-compatible provider.
//!
//! Works with anything that speaks the OpenAI `/v1/chat/completions` API:
//!   - OpenAI itself
//!   - OpenRouter (point BASE_URL to https://openrouter.ai/api/v1)
//!   - Groq        (https://api.groq.com/openai/v1)
//!   - Together    (https://api.together.xyz/v1)
//!   - Mistral     (https://api.mistral.ai/v1)
//!   - DeepSeek    (https://api.deepseek.com/v1)
//!   - Any other OpenAI-compatible gateway
//!
//! Configured via env:
//!   OPENAI_API_KEY     (required)
//!   OPENAI_BASE_URL    (default: https://api.openai.com/v1)
//!   OPENAI_MODEL       (default: gpt-4o-mini)

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools;
use super::{system_prompt, LlmProvider, Message, Role, MAX_TOOL_ROUNDS, REQUEST_TIMEOUT_SECS};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL:    &str = "gpt-4o-mini";

pub struct Provider {
    api_key:  String,
    base_url: String,
    model:    String,
    client:   reqwest::blocking::Client,
}

impl Provider {
    pub fn from_env_optional() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").ok()?;
        if api_key.trim().is_empty() { return None; }
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.into())
            .trim_end_matches('/').to_string();
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build().ok()?;
        Some(Self { api_key, base_url, model, client })
    }

    pub fn base_url(&self) -> &str { &self.base_url }
}

impl LlmProvider for Provider {
    fn name(&self) -> &'static str { "openai" }
    fn model(&self) -> &str { &self.model }

    fn generate(&self, history: &[Message], tools_reg: &[tools::Tool]) -> Result<String> {
        // Build messages array — start with system prompt, then history
        let mut messages: Vec<Value> = vec![json!({
            "role": "system",
            "content": system_prompt(),
        })];
        for m in history {
            let role = match m.role { Role::User => "user", Role::Assistant => "assistant" };
            messages.push(json!({ "role": role, "content": m.content }));
        }

        // OpenAI tool format: { type: "function", function: { name, description, parameters } }
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

        let url = format!("{}/chat/completions", self.base_url);

        for round in 0..MAX_TOOL_ROUNDS {
            let body = json!({
                "model":       self.model,
                "messages":    messages,
                "tools":       tools_payload,
                "tool_choice": "auto",
                "temperature": 0.7,
                "max_tokens":  512,
            });

            let resp = self.client.post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .json(&body)
                .send()?;
            let status = resp.status();
            let text   = resp.text()?;
            if !status.is_success() {
                bail!("openai {}: {}", status, truncate(&text, 240));
            }

            let parsed: ChatCompletionResponse = serde_json::from_str(&text)
                .map_err(|e| anyhow!("openai parse: {e} — body: {}", truncate(&text, 240)))?;
            let choice = parsed.choices.into_iter().next()
                .ok_or_else(|| anyhow!("openai: no choices"))?;
            let msg = choice.message;

            // Log the assistant's own text (its reasoning / plan for this round)
            // so we can see WHY it chose the tools it did — not just which.
            if let Some(c) = msg.content.as_deref() {
                let c = c.trim();
                if !c.is_empty() {
                    tracing::info!("openai[round {round}]: assistant ← {}", truncate(c, 600));
                }
            }

            // No tool calls? We have our final text.
            let tool_calls = msg.tool_calls.unwrap_or_default();
            if tool_calls.is_empty() {
                return Ok(msg.content.unwrap_or_default());
            }

            // Echo assistant turn (with tool_calls) and append tool results.
            // We must include the assistant message verbatim including tool_calls so
            // the next round knows which call IDs to respond to.
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
                let result = match serde_json::from_str::<Value>(&tc.function.arguments) {
                    Ok(args) => {
                        tracing::info!("openai: tool call → {}({})", tc.function.name, args);
                        crate::tools::dispatch(tools_reg, &tc.function.name, &args)
                    }
                    Err(e) => {
                        // Don't silently run with empty args — for a mutating
                        // tool that would be a wrong, possibly harmful action.
                        tracing::warn!(
                            "openai: bad tool args for {} ({e}): {}",
                            tc.function.name,
                            truncate(&tc.function.arguments, 200)
                        );
                        json!({ "error": format!("Could not parse tool arguments as JSON ({e}). Re-issue the call with valid JSON arguments.") })
                    }
                };
                tracing::info!("openai: tool result ← {result}");
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": result.to_string(),
                }));
            }
            tracing::debug!("openai: round {round} → tool loop");
        }
        bail!("openai: exceeded MAX_TOOL_ROUNDS")
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
