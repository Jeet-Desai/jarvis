
//! Google Gemini provider (gemini-2.5-flash-lite by default).
//!
//! Configured via env:
//!   GEMINI_API_KEY     (required)
//!   GEMINI_MODEL       (default: gemini-2.5-flash-lite)

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools;
use super::{LlmProvider, Message, Role, MAX_TOOL_ROUNDS, REQUEST_TIMEOUT_SECS, SYSTEM_PROMPT};

const DEFAULT_MODEL: &str = "gemini-2.5-flash-lite";
const ENDPOINT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

pub struct Provider {
    api_key: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl Provider {
    pub fn from_env_optional() -> Option<Self> {
        let api_key = std::env::var("GEMINI_API_KEY").ok()?;
        if api_key.trim().is_empty() { return None; }
        let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build().ok()?;
        Some(Self { api_key, model, client })
    }
}

impl LlmProvider for Provider {
    fn name(&self) -> &'static str { "gemini" }
    fn model(&self) -> &str { &self.model }

    fn generate(&self, history: &[Message], tools_reg: &[tools::Tool]) -> Result<String> {
        // Build native Gemini contents from normalized history
        let mut contents: Vec<Value> = history.iter().map(|m| {
            let role = match m.role { Role::User => "user", Role::Assistant => "model" };
            json!({
                "role": role,
                "parts": [{ "text": m.content }]
            })
        }).collect();

        let tool_decls: Vec<Value> = tools_reg.iter().map(|t| t.declaration.clone()).collect();

        for round in 0..MAX_TOOL_ROUNDS {
            let body = json!({
                "systemInstruction": { "parts": [{ "text": SYSTEM_PROMPT }] },
                "contents": contents,
                "tools": [{ "functionDeclarations": tool_decls }],
                "generationConfig": {
                    "temperature": 0.7,
                    "maxOutputTokens": 512,
                }
            });

            let url = format!("{ENDPOINT_BASE}/{}:generateContent", self.model);
            let resp = self.client.post(&url)
                .header("x-goog-api-key", &self.api_key)
                .json(&body)
                .send()?;
            let status = resp.status();
            let text   = resp.text()?;
            if !status.is_success() {
                bail!("gemini {}: {}", status, truncate(&text, 240));
            }

            let parsed: GenerateContentResponse = serde_json::from_str(&text)
                .map_err(|e| anyhow!("gemini parse: {e} — body: {}", truncate(&text, 240)))?;

            let candidate = parsed.candidates.into_iter().next()
                .ok_or_else(|| anyhow!("gemini: no candidates"))?;
            let parts = candidate.content.parts;

            let mut function_calls: Vec<FunctionCall> = Vec::new();
            let mut model_text = String::new();
            for p in &parts {
                if let Some(t) = &p.text { model_text.push_str(t); }
                else if let Some(fc) = &p.function_call { function_calls.push(fc.clone()); }
            }

            // Log the model's own text (reasoning / plan) for this round so we
            // can see WHY it chose the tools it did — not just which.
            if !model_text.trim().is_empty() {
                tracing::info!("gemini[round {round}]: assistant ← {}", truncate(&model_text, 600));
            }

            if function_calls.is_empty() {
                return Ok(model_text);
            }

            // Echo model's tool-call turn, then attach each tool's result
            contents.push(json!({
                "role": "model",
                "parts": parts.iter().map(part_to_value).collect::<Vec<_>>()
            }));
            for fc in function_calls {
                tracing::info!("gemini: tool call → {}({})", fc.name, fc.args);
                let result = crate::tools::dispatch(tools_reg, &fc.name, &fc.args);
                tracing::info!("gemini: tool result ← {result}");
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "functionResponse": { "name": fc.name, "response": result } }]
                }));
            }
            tracing::debug!("gemini: round {round} → tool loop");
        }
        bail!("gemini: exceeded MAX_TOOL_ROUNDS")
    }
}

fn part_to_value(p: &Part) -> Value {
    if let Some(t)  = &p.text          { return json!({ "text": t }); }
    if let Some(fc) = &p.function_call { return json!({ "functionCall": fc }); }
    json!({})
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes { return s.to_string(); }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    format!("{}…", &s[..i])
}

// ── Wire types ────────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct GenerateContentResponse { candidates: Vec<Candidate> }
#[derive(Debug, Deserialize)]
struct Candidate { content: Content }
#[derive(Debug, Deserialize)]
struct Content { parts: Vec<Part> }
#[derive(Debug, Deserialize)]
struct Part {
    #[serde(default)] text: Option<String>,
    #[serde(default, rename = "functionCall")] function_call: Option<FunctionCall>,
}
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct FunctionCall {
    name: String,
    #[serde(default)] args: Value,
}
