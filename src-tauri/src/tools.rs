//! Tools Jarvis can call. Each tool exposes:
//!   - a JSON-Schema declaration the LLM consumes (so it knows when/how to
//!     invoke it)
//!   - a Rust handler that takes the args, runs locally, returns JSON
//!
//! Tools live here (not in brain.rs) so adding a new one doesn't touch the
//! LLM client. Start tiny; grow as needed.

use anyhow::Result;
use serde_json::{json, Value};

use crate::brain;

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
            name: "get_brain_info",
            declaration: json!({
                "name": "get_brain_info",
                "description": "Return the ground-truth identity of the LLM provider(s) and model(s) currently running this assistant. Use whenever the user asks 'what model are you', 'which LLM', 'which provider', 'are you GPT / Gemini / Claude / Ollama', 'what brain am I talking to'. Returns the configured fallback order with concrete model names — the first entry is what's serving this turn unless a fallback was triggered. Do NOT guess model identity from training data; always call this tool.",
                "parameters": { "type": "object", "properties": {} }
            }),
            handler: get_brain_info,
        },
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
        Tool {
            name: "type_text",
            declaration: json!({
                "name": "type_text",
                "description": "Type / write / paste text into another app on the user's Mac. Use whenever the user says 'type X into Y', 'write this in Z', 'paste my answer into [app]', 'enter this query in [app]'. Default target is whatever app is currently focused; pass `app_name` to bring a different app to front first (e.g. 'Codex', 'ChatGPT', 'Google Chrome', 'Slack', 'iMessage'). Pass `submit=true` to press Return after typing (use for chat boxes / search bars where the user wants the text sent, not just entered). Uses clipboard-paste under the hood so all characters (emoji, code, multi-line) work. Restores the user's original clipboard afterward. Requires Accessibility + Automation, both granted to Jarvis.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text":     { "type": "string",  "description": "The text to type." },
                        "app_name": { "type": "string",  "description": "Optional. Exact name of the macOS app to focus before typing (e.g. 'Codex'). If omitted, types into whichever app is currently focused." },
                        "submit":   { "type": "boolean", "description": "Optional. If true, press Return/Enter after typing. Default false." }
                    },
                    "required": ["text"]
                }
            }),
            handler: type_text,
        },
        Tool {
            name: "set_clipboard",
            declaration: json!({
                "name": "set_clipboard",
                "description": "Write text to the user's clipboard so they can paste it themselves with Cmd+V. Use when the user asks you to 'copy X to clipboard', 'put X on my clipboard', or when text is long enough that asking them to paste is friendlier than auto-typing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Text to place on the clipboard." }
                    },
                    "required": ["text"]
                }
            }),
            handler: set_clipboard,
        },
        Tool {
            name: "focus_app",
            declaration: json!({
                "name": "focus_app",
                "description": "Bring a macOS app to the front (focus it). Use when the user says 'open X', 'switch to Y', 'go to Z', or before any other tool that needs that app focused. Activates the app if running; launches it if installed but not running.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "app_name": { "type": "string", "description": "Exact macOS app name (e.g. 'Codex', 'Google Chrome', 'Slack')." }
                    },
                    "required": ["app_name"]
                }
            }),
            handler: focus_app,
        },
        Tool {
            name: "send_shortcut",
            declaration: json!({
                "name": "send_shortcut",
                "description": "Send a macOS keyboard shortcut to the focused app. Use for ⌘F (find/search), ⌘N (new chat/document), ⌘S (save), ⌘W (close), ⌘T (new tab), Escape, arrow keys, Tab, etc. — anything that ISN'T literal text. CRITICAL use case: when opening a chat in messaging apps (WhatsApp, Slack, iMessage, Discord, Telegram), the focused state is usually NOT the search field. First call send_shortcut with key='f' modifiers=['cmd'] (⌘F) or key='n' modifiers=['cmd'] (⌘N) to focus the search/new-chat field, THEN call type_text with the name and submit=true. Requires Accessibility permission, already granted to Jarvis.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "The key to press. Either a single literal character ('f', 'n', '/', '?', '1'), or a named special key: 'return', 'escape', 'space', 'tab', 'delete', 'left', 'right', 'up', 'down', 'home', 'end', 'pageup', 'pagedown'." },
                        "modifiers": { "type": "array", "items": { "type": "string" }, "description": "Optional modifier keys held while pressing. Allowed values: 'cmd' (or 'command'), 'shift', 'option' (or 'alt'), 'control' (or 'ctrl'). Use ['cmd'] for ⌘F, ['cmd','shift'] for ⌘⇧P, etc. Omit for unmodified keys like plain Escape." }
                    },
                    "required": ["key"]
                }
            }),
            handler: send_shortcut,
        },
        // Future: read_screen_via_accessibility, click_at_xy, …
    ]
}

fn get_brain_info(_args: &Value) -> Result<Value> {
    match brain::brain_info() {
        Some(info) => Ok(json!({
            "providers_in_order": info.providers_in_order,
            "primary":            info.providers_in_order.first(),
            "vision_model":       info.vision_model,
            "note":               "The first provider serves each turn; on retryable errors (rate limit / timeout) the brain falls back to the next. Vision (see_screen) always uses the listed vision_model regardless of brain provider.",
        })),
        None => Ok(json!({
            "error": "brain not initialized yet — get_brain_info called before Brain::from_env",
        })),
    }
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

/// Escape a string so it can be safely embedded inside an AppleScript
/// double-quoted literal. Without this, any value interpolated into a
/// `tell application "..."` template can break out of the string and inject
/// arbitrary AppleScript — which has `do shell script`, i.e. command
/// execution. The values we interpolate (tool `app_name`, keystroke text)
/// are LLM-controlled, and the LLM ingests untrusted web-page text via
/// `get_browser_context`, so this is a real prompt-injection → RCE path.
/// Escaping keeps the value confined to the string literal; a hostile name
/// at worst yields "app not found", never code execution.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
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

// ──────────────────────────────────────────────────────────────────────────────
// Action tools — type text, focus apps, set clipboard. These are the "do
// things on the user's Mac" tools (vs the read-only ones above). They rely
// on Accessibility (System Events keystroke) + AppleEvents (tell app to
// activate) — both granted to Jarvis at first launch.
// ──────────────────────────────────────────────────────────────────────────────

fn type_text(args: &Value) -> Result<Value> {
    use std::io::Write;

    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: text"))?;
    let app_name = args
        .get("app_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::trim);
    let submit = args.get("submit").and_then(|v| v.as_bool()).unwrap_or(false);

    // Stash old clipboard so we can restore it after the paste.
    let old_clip = std::process::Command::new("pbpaste")
        .output()
        .ok()
        .map(|o| o.stdout);

    // Put our text on the clipboard.
    write_clipboard(text)?;

    // Focus the target app if specified. Use Launch Services (open -a)
    // so Catalyst / sandboxed apps work; AppleScript activate fails on them.
    if let Some(app) = app_name {
        let _ = std::process::Command::new("open").arg("-a").arg(app).status();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    // Paste (⌘V).
    osascript(r#"tell application "System Events" to keystroke "v" using command down"#)?;

    if submit {
        std::thread::sleep(std::time::Duration::from_millis(150));
        // key code 36 = Return
        osascript(r#"tell application "System Events" to key code 36"#)?;
    }

    // Restore prior clipboard contents after the paste has had time to land.
    if let Some(old) = old_clip {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(&old);
        }
        let _ = child.wait();
    }

    Ok(json!({
        "ok": true,
        "typed_into": app_name.unwrap_or("focused app"),
        "submitted": submit,
        "chars": text.chars().count(),
    }))
}

fn send_shortcut(args: &Value) -> Result<Value> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: key"))?;
    let modifiers: Vec<String> = args
        .get("modifiers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.as_str().map(|s| s.trim().to_lowercase()))
                .collect()
        })
        .unwrap_or_default();

    let mod_strs: Vec<&str> = modifiers
        .iter()
        .filter_map(|m| match m.as_str() {
            "cmd" | "command" | "meta" | "super" => Some("command down"),
            "shift" => Some("shift down"),
            "option" | "opt" | "alt" => Some("option down"),
            "control" | "ctrl" => Some("control down"),
            _ => None,
        })
        .collect();
    let using_clause = if mod_strs.is_empty() {
        String::new()
    } else {
        format!(" using {{{}}}", mod_strs.join(", "))
    };

    let key_lc = key.to_lowercase();
    let key_code: Option<u16> = match key_lc.as_str() {
        "return" | "enter" => Some(36),
        "escape" | "esc" => Some(53),
        "space" => Some(49),
        "tab" => Some(48),
        "delete" | "backspace" => Some(51),
        "forwarddelete" | "fwddelete" => Some(117),
        "left" | "leftarrow" => Some(123),
        "right" | "rightarrow" => Some(124),
        "down" | "downarrow" => Some(125),
        "up" | "uparrow" => Some(126),
        "home" => Some(115),
        "end" => Some(119),
        "pageup" => Some(116),
        "pagedown" => Some(121),
        _ => None,
    };

    let script = if let Some(code) = key_code {
        format!(r#"tell application "System Events" to key code {code}{using_clause}"#)
    } else {
        let escaped = applescript_escape(key);
        format!(r#"tell application "System Events" to keystroke "{escaped}"{using_clause}"#)
    };

    osascript(&script)?;
    Ok(json!({
        "ok": true,
        "key": key,
        "modifiers": modifiers,
    }))
}

fn set_clipboard(args: &Value) -> Result<Value> {
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: text"))?;
    write_clipboard(text)?;
    Ok(json!({ "ok": true, "chars": text.chars().count() }))
}

fn focus_app(args: &Value) -> Result<Value> {
    let app = args
        .get("app_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: app_name"))?;

    // `open -a` goes through Launch Services and works for Catalyst,
    // Electron, native, and sandboxed apps. AppleScript activate fails
    // with -1728 on sandboxed Catalyst apps like WhatsApp.
    let ls = std::process::Command::new("open").arg("-a").arg(app).output();
    let launched_via_ls = matches!(&ls, Ok(o) if o.status.success());
    if launched_via_ls {
        std::thread::sleep(std::time::Duration::from_millis(200));
        return Ok(json!({ "ok": true, "focused": app, "method": "launch_services" }));
    }

    // Fallback: try AppleScript activate (works for some scriptable apps
    // that Launch Services can't find by display name). Escape the name —
    // it's LLM-supplied and would otherwise allow AppleScript injection.
    if osascript(&format!(r#"tell application "{}" to activate"#, applescript_escape(app))).is_ok() {
        std::thread::sleep(std::time::Duration::from_millis(200));
        return Ok(json!({ "ok": true, "focused": app, "method": "applescript" }));
    }

    let stderr = ls.ok()
        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
        .unwrap_or_default();
    Ok(json!({
        "error": format!(
            "Couldn't focus '{app}'. Both Launch Services and AppleScript failed. \
             Verify the app's exact name (in Dock or /Applications); common gotcha: \
             '{app}' might actually be 'WhatsApp Web', 'Slack', 'Visual Studio Code', etc. \
             open-a stderr: {stderr}"
        )
    }))
}

fn write_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
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
