//! Screenshot + vision-LLM bridge.
//!
//! Used by the `see_screen` tool. Captures the user's screen via macOS
//! `screencapture`, base64-encodes the JPEG, and calls Gemini 2.5 Flash
//! multimodal with the user's question as context. Returns a short
//! description focused on what's relevant to that question.
//!
//! Why a dedicated vision call instead of feeding the image directly to
//! the main LLM:
//!   - The main LLM might be Ollama (text-only), or might be a model that
//!     doesn't natively support inline images. This bridge works regardless.
//!   - The orchestrator in `brain/mod.rs` doesn't need to know about images
//!     at all — they stay inside the tool boundary.
//!
//! Requirements:
//!   - `GEMINI_API_KEY` must be set (separate from the user's main brain
//!     provider; vision always uses Gemini).
//!   - First call triggers macOS Screen Recording permission prompt.

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

const VISION_MODEL: &str = "gemini-2.5-flash";
const ENDPOINT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const VISION_TIMEOUT_SECS: u64 = 45;

/// Capture the screen and ask Gemini what's on it, in the context of the
/// user's question. Returns a short text description focused on the answer.
pub fn describe_screen(query: &str) -> Result<String> {
    let api_key = std::env::var("GEMINI_API_KEY")
        .map_err(|_| anyhow!("vision requires GEMINI_API_KEY in .env"))?;

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

    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [
                { "text": prompt },
                { "inline_data": { "mime_type": "image/jpeg", "data": b64 } }
            ]
        }],
        "generationConfig": {
            "temperature": 0.3,
            "maxOutputTokens": 800,
        }
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(VISION_TIMEOUT_SECS))
        .build()?;

    let url = format!("{ENDPOINT_BASE}/{VISION_MODEL}:generateContent");
    let resp = client
        .post(&url)
        .header("x-goog-api-key", &api_key)
        .json(&body)
        .send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        bail!("gemini vision {}: {}", status, truncate(&text, 240));
    }

    let parsed: VisionResp = serde_json::from_str(&text)
        .map_err(|e| anyhow!("vision parse: {e} — body: {}", truncate(&text, 240)))?;
    let candidate = parsed
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("vision: no candidates"))?;
    let description = candidate
        .content
        .parts
        .into_iter()
        .filter_map(|p| p.text)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    if description.is_empty() {
        bail!("vision: empty description");
    }
    tracing::info!("vision: {} chars", description.len());
    Ok(description)
}

fn capture_screen() -> Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("jarvis_screen_{ts}.jpg"));
    let status = std::process::Command::new("screencapture")
        .arg("-x")              // no shutter sound
        .arg("-t").arg("jpg")
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
        // Downscale + recompress to ~200KB for faster upload to Gemini.
        // sips is built-in macOS, no extra deps.
        if meta.len() > 200_000 {
            let _ = std::process::Command::new("sips")
                .arg("-Z").arg("1280")                // max edge 1280px
                .arg("-s").arg("formatOptions").arg("70")
                .arg(&path)
                .arg("--out").arg(&path)
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
    if s.len() <= max_bytes { return s.to_string(); }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    format!("{}…", &s[..i])
}

// ── Response types ────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct VisionResp { candidates: Vec<VCandidate> }
#[derive(Debug, Deserialize)]
struct VCandidate { content: VContent }
#[derive(Debug, Deserialize)]
struct VContent { parts: Vec<VPart> }
#[derive(Debug, Deserialize)]
struct VPart { #[serde(default)] text: Option<String> }
