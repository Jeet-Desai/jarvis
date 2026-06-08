//! Screenshot + vision-LLM bridge, with provider fallback.
//!
//! Used by the `see_screen` tool. Captures the user's screen via macOS
//! `screencapture`, base64-encodes the JPEG, and asks a vision-capable LLM to
//! describe what's relevant to the user's question.
//!
//! Provider chain (first success wins):
//!   1. Gemini multimodal   — if GEMINI_API_KEY set (GEMINI_VISION_MODEL).
//!   2. OpenRouter / OpenAI — if OPENAI_API_KEY set. OPENAI_VISION_MODEL must
//!                            be a MULTIMODAL model (defaults to a free
//!                            OpenRouter vision model).
//!   3. Ollama LOCAL vision — if OLLAMA_VISION_MODEL set, e.g.
//!                            `llama3.2-vision:11b`. NOTE: Ollama *cloud*
//!                            models (`…-cloud`) are text-only and CANNOT see
//!                            images — you must `ollama pull` a local vision
//!                            model and name it here.
//!
//! Why a dedicated vision call instead of feeding the image to the main brain:
//! the brain may be a text-only model (e.g. qwen3-coder); keeping images inside
//! the tool boundary makes vision work regardless of the main provider — and
//! gives `see_screen` its own fallback so a single provider's outage (e.g.
//! Gemini's daily free quota) doesn't kill screen-vision entirely.
//!
//! Requirements:
//!   - At least one of the three providers configured.
//!   - First call triggers the macOS Screen Recording permission prompt.

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

/// Gemini vision defaults. `flash-lite` matches the brain default and has
/// higher free-tier availability than `flash`. Override via GEMINI_VISION_MODEL.
const GEMINI_DEFAULT_MODEL: &str = "gemini-2.5-flash-lite";
const GEMINI_ENDPOINT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
/// Free OpenRouter multimodal default; override via OPENAI_VISION_MODEL.
const OPENAI_DEFAULT_VISION_MODEL: &str = "google/gemma-4-31b-it:free";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434";
const VISION_TIMEOUT_SECS: u64 = 45;

/// Capture the screen and ask a vision LLM what's on it, in the context of the
/// user's question. Tries each configured provider until one answers.
pub fn describe_screen(query: &str) -> Result<String> {
    let path = capture_screen()?;
    let bytes = std::fs::read(&path)?;
    let _ = std::fs::remove_file(&path); // clean up
    let b64 = B64.encode(&bytes);
    tracing::info!(
        "vision: captured {} ({} KB), describing query={:?}",
        path.display(),
        bytes.len() / 1024,
        query
    );

    let prompt = format!(
        "The user asked: \"{query}\"\n\n\
         Look at this screenshot of their Mac screen and describe ONLY what's relevant \
         to answering them. Be specific about visible UI elements, text content, app or \
         window names, error messages, code, conversations, video frames — whatever \
         applies. If the answer to their question is right there in the image, give it. \
         Keep your description under 250 words and use plain prose (no markdown)."
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(VISION_TIMEOUT_SECS))
        .build()?;

    let mut errors: Vec<String> = Vec::new();

    // 1) Gemini
    if let Some(key) = env_nonempty("GEMINI_API_KEY") {
        let model =
            env_nonempty("GEMINI_VISION_MODEL").unwrap_or_else(|| GEMINI_DEFAULT_MODEL.into());
        match gemini_vision(&client, &key, &model, &prompt, &b64) {
            Ok(d) => return Ok(d),
            Err(e) => record(&mut errors, "gemini", &model, &e),
        }
    }

    // 2) OpenRouter / OpenAI-compatible multimodal
    if let Some(key) = env_nonempty("OPENAI_API_KEY") {
        let base = env_nonempty("OPENAI_BASE_URL").unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.into());
        let model =
            env_nonempty("OPENAI_VISION_MODEL").unwrap_or_else(|| OPENAI_DEFAULT_VISION_MODEL.into());
        match openai_vision(&client, base.trim_end_matches('/'), &key, &model, &prompt, &b64) {
            Ok(d) => return Ok(d),
            Err(e) => record(&mut errors, "openai", &model, &e),
        }
    }

    // 3) Ollama LOCAL vision (opt-in — cloud models can't see images)
    if let Some(model) = env_nonempty("OLLAMA_VISION_MODEL") {
        let base = env_nonempty("OLLAMA_BASE_URL").unwrap_or_else(|| OLLAMA_DEFAULT_BASE_URL.into());
        match ollama_vision(&client, base.trim_end_matches('/'), &model, &prompt, &b64) {
            Ok(d) => return Ok(d),
            Err(e) => record(&mut errors, "ollama", &model, &e),
        }
    }

    if errors.is_empty() {
        bail!(
            "vision: no provider configured — set GEMINI_API_KEY, or OPENAI_API_KEY with a \
             multimodal OPENAI_VISION_MODEL, or OLLAMA_VISION_MODEL (a local vision model)"
        );
    }
    bail!("vision: all providers failed — {}", errors.join("  |  "))
}

fn record(errors: &mut Vec<String>, provider: &str, model: &str, e: &anyhow::Error) {
    tracing::warn!("vision[{provider}/{model}]: {e}");
    errors.push(format!("{provider}: {}", truncate(&e.to_string(), 120)));
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── Gemini ──────────────────────────────────────────────────────────────────
fn gemini_vision(
    client: &reqwest::blocking::Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    b64: &str,
) -> Result<String> {
    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [
                { "text": prompt },
                { "inline_data": { "mime_type": "image/jpeg", "data": b64 } }
            ]
        }],
        "generationConfig": { "temperature": 0.3, "maxOutputTokens": 800 }
    });
    let url = format!("{GEMINI_ENDPOINT_BASE}/{model}:generateContent");

    // One retry, but only on capacity 5xx — NOT on 429. A 429 is quota
    // exhaustion (often the per-day free-tier cap), which won't clear in a
    // few hundred ms, so we fail fast and let the chain fall to the next
    // provider instead of stalling on backoff.
    let mut last = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
        let resp = client
            .post(&url)
            .header("x-goog-api-key", api_key)
            .json(&body)
            .send()?;
        let status = resp.status();
        last = resp.text()?;
        if status.is_success() {
            let parsed: GeminiResp = serde_json::from_str(&last)
                .map_err(|e| anyhow!("gemini vision parse: {e} — body: {}", truncate(&last, 200)))?;
            let desc = parsed
                .candidates
                .into_iter()
                .next()
                .and_then(|c| c.content)
                .map(|c| {
                    c.parts
                        .into_iter()
                        .filter_map(|p| p.text)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default()
                .trim()
                .to_string();
            if desc.is_empty() {
                bail!("gemini vision: empty description");
            }
            return Ok(desc);
        }
        let code = status.as_u16();
        if !matches!(code, 500 | 502 | 503 | 504) {
            bail!("gemini vision {model} {}: {}", status, truncate(&last, 200));
        }
    }
    bail!("gemini vision {model}: capacity error — {}", truncate(&last, 200))
}

// ── OpenRouter / OpenAI-compatible multimodal ────────────────────────────────
fn openai_vision(
    client: &reqwest::blocking::Client,
    base: &str,
    key: &str,
    model: &str,
    prompt: &str,
    b64: &str,
) -> Result<String> {
    let data_uri = format!("data:image/jpeg;base64,{b64}");
    let body = json!({
        "model": model,
        "max_tokens": 800,
        "temperature": 0.3,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                { "type": "image_url", "image_url": { "url": data_uri } }
            ]
        }]
    });
    let url = format!("{base}/chat/completions");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        bail!("openai vision {model} {}: {}", status, truncate(&text, 200));
    }
    let parsed: OpenAiResp = serde_json::from_str(&text)
        .map_err(|e| anyhow!("openai vision parse: {e} — body: {}", truncate(&text, 200)))?;
    let desc = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default()
        .trim()
        .to_string();
    if desc.is_empty() {
        bail!("openai vision {model}: empty description (is it a multimodal model?)");
    }
    Ok(desc)
}

// ── Ollama LOCAL vision ───────────────────────────────────────────────────────
fn ollama_vision(
    client: &reqwest::blocking::Client,
    base: &str,
    model: &str,
    prompt: &str,
    b64: &str,
) -> Result<String> {
    // Ollama's native /api/chat takes raw base64 in `images` (no data: prefix).
    let body = json!({
        "model": model,
        "stream": false,
        "messages": [{ "role": "user", "content": prompt, "images": [b64] }],
        "options": { "temperature": 0.3, "num_predict": 800 }
    });
    let url = format!("{base}/api/chat");
    let resp = client.post(&url).json(&body).send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        bail!("ollama vision {model} {}: {}", status, truncate(&text, 200));
    }
    let parsed: OllamaChatResp = serde_json::from_str(&text)
        .map_err(|e| anyhow!("ollama vision parse: {e} — body: {}", truncate(&text, 200)))?;
    let desc = parsed
        .message
        .and_then(|m| m.content)
        .unwrap_or_default()
        .trim()
        .to_string();
    if desc.is_empty() {
        bail!("ollama vision {model}: empty description — is '{model}' a LOCAL vision model? (cloud models can't see images)");
    }
    Ok(desc)
}

fn capture_screen() -> Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("jarvis_screen_{ts}.jpg"));
    let status = std::process::Command::new("screencapture")
        .arg("-x") // no shutter sound
        .arg("-t")
        .arg("jpg")
        .arg(&path)
        .status()?;
    if !status.success() {
        open_screen_recording_settings();
        bail!(
            "Sir, I need Screen Recording permission. I've opened System Settings — please toggle Jarvis on under Privacy and Security, then restart me."
        );
    }
    // Empty / black image is the OTHER way macOS signals permission denied
    // for unsigned dev builds — file gets written but is essentially blank.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() < 8_000 {
            let _ = std::fs::remove_file(&path);
            open_screen_recording_settings();
            bail!(
                "Sir, the screenshot came back empty — Screen Recording permission is missing. I've opened System Settings; please grant it to Jarvis, then restart me."
            );
        }
        // Downscale + recompress for faster upload. Resolution kept high
        // enough that small UI text stays legible; quality 85 is the sweet
        // spot. sips is built-in macOS, no extra deps.
        if meta.len() > 200_000 {
            let _ = std::process::Command::new("sips")
                .arg("-Z")
                .arg("1920") // max edge 1920px
                .arg("-s")
                .arg("formatOptions")
                .arg("85")
                .arg(&path)
                .arg("--out")
                .arg(&path)
                .output();
        }
    }
    Ok(path)
}

fn open_screen_recording_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

// ── Response types ────────────────────────────────────────────────────────────
// All fields optional/defaulted so a blocked / truncated / oddly-shaped
// response degrades to "empty description" (a fall-through) instead of a hard
// parse error.
#[derive(Debug, Deserialize)]
struct GeminiResp {
    #[serde(default)]
    candidates: Vec<GCandidate>,
}
#[derive(Debug, Deserialize)]
struct GCandidate {
    #[serde(default)]
    content: Option<GContent>,
}
#[derive(Debug, Deserialize)]
struct GContent {
    #[serde(default)]
    parts: Vec<GPart>,
}
#[derive(Debug, Deserialize)]
struct GPart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiResp {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
}
#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    #[serde(default)]
    message: OpenAiMsg,
}
#[derive(Debug, Default, Deserialize)]
struct OpenAiMsg {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResp {
    #[serde(default)]
    message: Option<OllamaMsg>,
}
#[derive(Debug, Deserialize)]
struct OllamaMsg {
    #[serde(default)]
    content: Option<String>,
}
