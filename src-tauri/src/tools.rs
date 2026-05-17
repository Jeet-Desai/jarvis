//! Tools Jarvis can call. Each tool exposes:
//!   - a JSON-Schema declaration the LLM consumes (so it knows when/how to
//!     invoke it)
//!   - a Rust handler that takes the args, runs locally, returns JSON
//!
//! Tools live here (not in brain.rs) so adding a new one doesn't touch the
//! LLM client. Start tiny; grow as needed.

use anyhow::Result;
use serde_json::{json, Value};

/// One callable tool: Gemini sees the `declaration`, and when it issues a
/// `functionCall` with this name we run `handler` and feed the result back.
pub struct Tool {
    pub name: &'static str,
    pub declaration: Value,
    pub handler: fn(&Value) -> Result<Value>,
}

/// Returns every tool Jarvis has. Order doesn't matter.
pub fn registry() -> Vec<Tool> {
    vec![
        Tool {
            name: "get_current_time",
            declaration: json!({
                "name": "get_current_time",
                "description": "Return the user's current local date and time. Use whenever the user asks what time it is, what day it is, or anything involving the current moment.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }),
            handler: get_current_time,
        },
        Tool {
            name: "get_focused_app",
            declaration: json!({
                "name": "get_focused_app",
                "description": "Get the name of the macOS app the user currently has in focus. Use before answering questions that depend on what the user is currently working with (e.g. \"what am I doing right now?\" or to choose which app to read context from).",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            handler: get_focused_app,
        },
        Tool {
            name: "get_browser_context",
            declaration: json!({
                "name": "get_browser_context",
                "description": "Read the user's currently active browser tab — returns URL, page title, and the first ~4000 chars of visible page text. Call this whenever the user asks about a webpage they're viewing, references 'this page' / 'this article' / 'what I'm reading' / 'this site', wants a summary, wants you to answer something based on what's on screen, or any context-dependent question about web content. Supports Chrome, Safari, Arc, Brave, Edge, Vivaldi. Returns an error for Firefox or when no browser is focused.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            handler: get_browser_context,
        },
        Tool {
            name: "get_clipboard",
            declaration: json!({
                "name": "get_clipboard",
                "description": "Read whatever text is currently on the user's macOS clipboard. THIS IS THE PRIMARY WAY to read text from non-browser apps (WhatsApp, Slack, Discord, VS Code, Mail, Notes, Messages, terminal, anywhere). Workflow: tell the user 'select the text with Cmd+A and copy with Cmd+C, then I'll read it' — they do it — then YOU call this tool. Returns the clipboard contents (up to ~4000 chars). Requires NO permission — works on every Mac immediately.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            handler: get_clipboard,
        },
        Tool {
            name: "see_screen",
            declaration: json!({
                "name": "see_screen",
                "description": "OPTIONAL screenshot+vision tool. Captures the screen and asks a vision model to describe it. Only works if the user has granted Screen Recording permission (requires admin on some Macs — many users CAN'T grant this). Prefer get_clipboard for text content. Use see_screen only if the user explicitly wants visual analysis (e.g. 'describe this image', 'what's in this video frame') AND the clipboard wouldn't help.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "What the user wants to know about what's on screen." }
                    },
                    "required": ["query"]
                }
            }),
            handler: see_screen,
        },
        Tool {
            name: "list_calendar_events",
            declaration: json!({
                "name": "list_calendar_events",
                "description": "Read events from the user's Google Calendar (via MCP server). Use for 'what's on my calendar', 'what meetings do I have today/tomorrow/this week', 'when's my next meeting'. Pass timeMin and timeMax as ISO-8601 timestamps that bracket the window the user is asking about. Returns event summaries with start/end times. Requires the user to have configured GOOGLE_OAUTH_CREDENTIALS in .env (see README).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "timeMin": { "type": "string", "description": "ISO-8601 start of the time window, e.g. '2026-05-17T00:00:00+05:30' for the start of today in the user's local time." },
                        "timeMax": { "type": "string", "description": "ISO-8601 end of the time window, e.g. '2026-05-17T23:59:59+05:30' for end of today." },
                        "calendarId": { "type": "string", "description": "Optional. Defaults to 'primary' (the user's main calendar)." }
                    },
                    "required": ["timeMin", "timeMax"]
                }
            }),
            handler: list_calendar_events,
        },
        Tool {
            name: "create_calendar_event",
            declaration: json!({
                "name": "create_calendar_event",
                "description": "Create a new event on the user's Google Calendar. Use when the user says 'schedule X for tomorrow at 3pm', 'add a meeting with Y on Friday', etc. ALWAYS confirm the title, date, and time with the user BEFORE calling unless they were unambiguously specific.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "summary":     { "type": "string", "description": "Event title." },
                        "start":       { "type": "string", "description": "ISO-8601 start time, e.g. '2026-05-18T15:00:00+05:30'." },
                        "end":         { "type": "string", "description": "ISO-8601 end time. Required by Google Calendar API." },
                        "description": { "type": "string", "description": "Optional details / notes." },
                        "location":    { "type": "string", "description": "Optional location string." },
                        "calendarId":  { "type": "string", "description": "Optional. Defaults to 'primary'." }
                    },
                    "required": ["summary", "start", "end"]
                }
            }),
            handler: create_calendar_event,
        },
        // Future: send_message, set_clipboard,
        //         read_screen_via_accessibility, click_at_xy, …
    ]
}

fn get_current_time(_args: &Value) -> Result<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    // Use chrono-less formatting — keep deps minimal. The LLM will format
    // it nicely for the user; we just need to give it the raw facts.
    let (date_str, time_str, tz) = format_local(secs);
    Ok(json!({
        "iso": iso8601_local(secs),
        "date": date_str,
        "time_24h": time_str,
        "timezone": tz,
    }))
}

// ──────────────────────────────────────────────────────────────────────────────
// Screen / browser context tools — via macOS AppleScript (osascript)
//
// Why AppleScript instead of CDP / accessibility:
//   - Zero new daemons or extensions to install on the user's machine
//   - Built into every Mac, no setup
//   - One predictable TCC permission prompt per (Jarvis ↔ browser) pair
// ──────────────────────────────────────────────────────────────────────────────

fn get_focused_app(_args: &Value) -> Result<Value> {
    let app = osascript(
        r#"tell application "System Events" to get name of first process whose frontmost is true"#,
    )?;
    Ok(json!({ "front_app": app.trim() }))
}

fn get_browser_context(_args: &Value) -> Result<Value> {
    let app = osascript(
        r#"tell application "System Events" to get name of first process whose frontmost is true"#,
    )?;
    let app = app.trim().to_string();

    let (url_cmd, title_cmd, text_cmd): (String, String, Option<String>) = match app.as_str() {
        // Chromium-based browsers share an identical AppleScript dictionary
        | "Google Chrome"
        | "Google Chrome Canary"
        | "Brave Browser"
        | "Microsoft Edge"
        | "Vivaldi"
        | "Arc"
        | "Comet"             // Perplexity Comet
        | "Chromium"          // the open-source build
        | "Opera"
        | "Opera GX"
        | "Sidekick"
        | "Dia"               // The Browser Company's other browser
        | "Island"            // enterprise browser, also chromium
            => (
            format!(r#"tell application "{app}" to get URL of active tab of front window"#),
            format!(r#"tell application "{app}" to get title of active tab of front window"#),
            Some(format!(
                r#"tell application "{app}" to tell active tab of front window to execute javascript "document.body.innerText.substring(0,4000)""#
            )),
        ),
        "Safari" => (
            r#"tell application "Safari" to get URL of front document"#.to_string(),
            r#"tell application "Safari" to get name of front document"#.to_string(),
            Some(
                r#"tell application "Safari" to do JavaScript "document.body.innerText.substring(0,4000)" in front document"#
                    .to_string(),
            ),
        ),
        "Firefox" | "Zen Browser" => {
            return Ok(json!({
                "error": format!("{app} doesn't support AppleScript automation, so I can't read the page directly. Ask the user to copy the text or switch to a Chromium-based browser."),
                "front_app": app,
            }));
        }
        _ => {
            return Ok(json!({
                "error": format!("No browser is currently focused — the front app is '{app}'."),
                "front_app": app,
            }));
        }
    };

    let url = osascript(&url_cmd).unwrap_or_else(|e| format!("[error: {e}]"));
    let title = osascript(&title_cmd).unwrap_or_else(|e| format!("[error: {e}]"));
    let text = match text_cmd {
        Some(cmd) => osascript(&cmd).unwrap_or_else(|e| {
            format!(
                "[page text unavailable — you may need to enable 'Allow JavaScript from Apple Events' in {app}: {e}]"
            )
        }),
        None => String::new(),
    };

    Ok(json!({
        "browser":      app,
        "url":          url.trim(),
        "title":        title.trim(),
        "text_preview": text.trim(),
    }))
}

fn osascript(script: &str) -> Result<String> {
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("osascript: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ──────────────────────────────────────────────────────────────────────────────
// Screenshot + vision — uses Gemini multimodal regardless of which LLM the
// main brain is using. Vision is encapsulated inside the tool so the
// orchestrator stays text-only.
// ──────────────────────────────────────────────────────────────────────────────

fn see_screen(args: &Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Describe what's visible on the screen.");

    let description = crate::brain::vision::describe_screen(query)?;
    Ok(json!({
        "description": description,
    }))
}

// ──────────────────────────────────────────────────────────────────────────────
// Clipboard tool — the no-permission fallback for "what's on screen" queries.
// Works on every Mac without admin / Screen Recording / Accessibility.
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Calendar tools — dispatched to the Google Calendar MCP server via the
// global mcp::McpManager. Returns a friendly setup hint when not configured.
// ──────────────────────────────────────────────────────────────────────────────

fn list_calendar_events(args: &Value) -> Result<Value> {
    let mcp = crate::mcp::global()
        .ok_or_else(|| anyhow::anyhow!("MCP manager not running"))?;
    // The cocal/google-calendar-mcp tool is named "list-events"
    mcp.calendar_call("list-events", normalize_calendar_args(args))
}

fn create_calendar_event(args: &Value) -> Result<Value> {
    let mcp = crate::mcp::global()
        .ok_or_else(|| anyhow::anyhow!("MCP manager not running"))?;
    // The cocal/google-calendar-mcp tool is named "create-event"
    mcp.calendar_call("create-event", normalize_calendar_args(args))
}

/// Inject a default calendarId of "primary" if the LLM omitted it.
fn normalize_calendar_args(args: &Value) -> Value {
    let mut obj = args.as_object().cloned().unwrap_or_default();
    if !obj.contains_key("calendarId") {
        obj.insert("calendarId".into(), Value::String("primary".into()));
    }
    Value::Object(obj)
}

fn get_clipboard(_args: &Value) -> Result<Value> {
    let output = std::process::Command::new("pbpaste").output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!("pbpaste failed"));
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(json!({
            "error": "Clipboard is empty. Ask the user to copy some text first (Cmd+A then Cmd+C), then call this tool again."
        }));
    }
    // Cap at ~8000 bytes (char-safe) to keep LLM payload sane
    let capped = if trimmed.len() > 8000 {
        let mut i = 8000;
        while i > 0 && !trimmed.is_char_boundary(i) { i -= 1; }
        format!("{}…", &trimmed[..i])
    } else {
        trimmed.to_string()
    };
    Ok(json!({ "clipboard_text": capped, "length": trimmed.len() }))
}

// ──────────────────────────────────────────────────────────────────────────────
// Time helpers — no chrono dep
// ──────────────────────────────────────────────────────────────────────────────

fn format_local(unix_secs: u64) -> (String, String, String) {
    let tz_offset_secs = local_tz_offset_seconds();
    let local = unix_secs as i64 + tz_offset_secs as i64;
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(local);
    (
        format!("{y:04}-{mo:02}-{d:02}"),
        format!("{h:02}:{mi:02}:{s:02}"),
        format_tz(tz_offset_secs),
    )
}

fn iso8601_local(unix_secs: u64) -> String {
    let tz_offset_secs = local_tz_offset_seconds();
    let local = unix_secs as i64 + tz_offset_secs as i64;
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(local);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{}",
        format_tz(tz_offset_secs)
    )
}

fn format_tz(offset_secs: i32) -> String {
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs = offset_secs.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    format!("{sign}{h:02}:{m:02}")
}

fn local_tz_offset_seconds() -> i32 {
    // Cheap & correct on macOS: ask libc for the difference between
    // gmtime and localtime of the same instant.
    use std::mem::MaybeUninit;
    unsafe {
        let now = libc_time();
        let mut local: MaybeUninit<libc::tm> = MaybeUninit::uninit();
        libc::localtime_r(&now, local.as_mut_ptr());
        let local = local.assume_init();
        local.tm_gmtoff as i32
    }
}

fn libc_time() -> libc::time_t {
    unsafe { libc::time(std::ptr::null_mut()) }
}

// Convert unix seconds (already shifted to local) to (Y, M, D, h, m, s).
// Algorithm from Howard Hinnant; works for any year.
fn unix_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem  = secs.rem_euclid(86400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    // days since 1970-01-01 → date
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365*yoe + yoe/4 - yoe/100);
    let mp = (5*doy + 2) / 153;
    let d = (doy - (153*mp + 2)/5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y as i32, m, d, h, mi, s)
}
