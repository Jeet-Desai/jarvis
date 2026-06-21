//! Tools Jarvis can call. Each tool exposes:
//!   - a JSON-Schema declaration the LLM consumes (so it knows when/how to
//!     invoke it)
//!   - a Rust handler that takes the args, runs locally, returns JSON
//!
//! Tools live here (not in brain.rs) so adding a new one doesn't touch the
//! LLM client. Start tiny; grow as needed.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::brain;
use crate::control;
use crate::policy::{self, DangerLevel, Decision};

/// One callable tool: the LLM sees the `declaration`, and when it issues a tool
/// call with this name we run `handler` (through `dispatch`, which enforces the
/// safety policy) and feed the result back. `danger` drives that policy gate.
pub struct Tool {
    pub name: &'static str,
    pub declaration: Value,
    pub handler: fn(&Value) -> Result<Value>,
    pub danger: DangerLevel,
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
            danger: DangerLevel::ReadOnly,
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
            danger: DangerLevel::ReadOnly,
            handler: get_current_time,
        },
        Tool {
            name: "get_focused_app",
            declaration: json!({
                "name": "get_focused_app",
                "description": "Get the name of the macOS app the user currently has in focus. Use before answering questions that depend on what the user is currently working with (e.g. \"what am I doing right now?\" or to choose which app to read context from).",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            danger: DangerLevel::ReadOnly,
            handler: get_focused_app,
        },
        Tool {
            name: "get_browser_context",
            declaration: json!({
                "name": "get_browser_context",
                "description": "Read the user's currently active browser tab — returns URL, page title, and the first ~4000 chars of visible page text. Call this whenever the user asks about a webpage they're viewing, references 'this page' / 'this article' / 'what I'm reading' / 'this site', wants a summary, wants you to answer something based on what's on screen, or any context-dependent question about web content. Supports Chrome, Safari, Arc, Brave, Edge, Vivaldi. Returns an error for Firefox or when no browser is focused.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            danger: DangerLevel::ReadOnly,
            handler: get_browser_context,
        },
        Tool {
            name: "get_clipboard",
            declaration: json!({
                "name": "get_clipboard",
                "description": "Read whatever text is currently on the user's macOS clipboard. THIS IS THE PRIMARY WAY to read text from non-browser apps (WhatsApp, Slack, Discord, VS Code, Mail, Notes, Messages, terminal, anywhere). Workflow: tell the user 'select the text with Cmd+A and copy with Cmd+C, then I'll read it' — they do it — then YOU call this tool. Returns the clipboard contents (up to ~4000 chars). Requires NO permission — works on every Mac immediately.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            danger: DangerLevel::ReadOnly,
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
            danger: DangerLevel::ReadOnly,
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
            danger: DangerLevel::ReadOnly,
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
            danger: DangerLevel::Mutating,
            handler: create_calendar_event,
        },
        // ── dev-agent orchestrator (Python sidecar over MCP) ──────────────────
        Tool {
            name: "spawn_dev_agent",
            declaration: json!({
                "name": "spawn_dev_agent",
                "description": "Spawn an autonomous Claude Code dev agent to work on a coding task in an isolated git worktree (its own branch). Use when the user says 'have an agent build X', 'spin up a coder to fix Y', 'build me a website', 'start working on Z in the background'. The agent runs autonomously (auto-applies edits, runs whitelisted shell) and works in the BACKGROUND — this call returns immediately. DESTRUCTIVE: writes code and runs commands — gated by the safety policy. After spawning, just tell the user it's started; do NOT poll its status in a loop (that blocks your turn). Returns the agent's id, branch, and worktree path.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task": { "type": "string", "description": "The coding task for the agent, in plain language (e.g. 'build an e-commerce storefront in React' or 'add a /health endpoint and a test')." },
                        "project_dir": { "type": "string", "description": "OPTIONAL. An existing git repo's absolute path to work inside, OR a short name for a NEW project (created under ~/claude/jarvis-workspace/projects). Omit entirely for a brand-new project — a name is derived from the task." },
                        "name": { "type": "string", "description": "Optional friendly name for the agent. Defaults to agent-<id>." }
                    },
                    "required": ["task"]
                }
            }),
            danger: DangerLevel::Destructive,
            handler: spawn_dev_agent,
        },
        Tool {
            name: "instruct_dev_agent",
            declaration: json!({
                "name": "instruct_dev_agent",
                "description": "Send a follow-up instruction to a running dev agent (its session is persistent — it remembers everything it has done). Use to steer, correct, or add to an agent's work, e.g. 'tell agent abc123 to also add tests', 'have it use TypeScript instead'. Pass the agent_id from spawn_dev_agent / list_dev_agents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "agent_id":    { "type": "string", "description": "The id of the agent to instruct." },
                        "instruction": { "type": "string", "description": "The follow-up instruction in plain language." }
                    },
                    "required": ["agent_id", "instruction"]
                }
            }),
            danger: DangerLevel::Mutating,
            handler: instruct_dev_agent,
        },
        Tool {
            name: "list_dev_agents",
            declaration: json!({
                "name": "list_dev_agents",
                "description": "List all dev agents with their status (spawning/working/idle/stopped/error), branch, cost so far, turn count, and last activity. Use for 'what are my agents doing', 'how's the agent going', 'list the agents'.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            danger: DangerLevel::ReadOnly,
            handler: list_dev_agents,
        },
        Tool {
            name: "get_agent_status",
            declaration: json!({
                "name": "get_agent_status",
                "description": "Get detailed status for ONE dev agent: state, cost, turn count, and its recent events (what it has been saying and which tools it used). Use when the user asks about a specific agent's progress. Pass the agent_id.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "The id of the agent to inspect." }
                    },
                    "required": ["agent_id"]
                }
            }),
            danger: DangerLevel::ReadOnly,
            handler: get_agent_status,
        },
        Tool {
            name: "stop_dev_agent",
            declaration: json!({
                "name": "stop_dev_agent",
                "description": "Interrupt and stop a dev agent. Use for 'stop the agent', 'kill agent abc123', 'cancel that'. Set remove_worktree=true to ALSO delete its git worktree — only do this once its work has been reviewed/merged or is being abandoned, since it is irreversible.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "agent_id":       { "type": "string", "description": "The id of the agent to stop." },
                        "remove_worktree": { "type": "boolean", "description": "If true, also delete the agent's worktree. Default false (keeps the work for review)." }
                    },
                    "required": ["agent_id"]
                }
            }),
            danger: DangerLevel::Mutating,
            handler: stop_dev_agent,
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
            danger: DangerLevel::Mutating,
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
            danger: DangerLevel::Mutating,
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
            danger: DangerLevel::Mutating,
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
            danger: DangerLevel::Mutating,
            handler: send_shortcut,
        },
        Tool {
            name: "read_screen_elements",
            declaration: json!({
                "name": "read_screen_elements",
                "description": "Read the interactive UI elements (buttons, text fields, links, menu items, checkboxes…) of the CURRENTLY FOCUSED app via macOS Accessibility, each with its on-screen label and center coordinates. Use this to 'understand the screen' before acting — e.g. to find the button or field to click. Returns up to ~120 elements. After reading, click one with `click_element` (by label) or `click` (by coordinates). Requires Accessibility permission (already granted to Jarvis).",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            danger: DangerLevel::ReadOnly,
            handler: read_screen_elements,
        },
        Tool {
            name: "locate_on_screen",
            declaration: json!({
                "name": "locate_on_screen",
                "description": "Visually find WHERE something is on screen and get its click coordinates, by analyzing a screenshot with a vision model. Use this to click something that read_screen_elements / click_element can't find by label — an icon (the '+' new-tab button), a custom web button, a search result, a canvas/image element. Describe the target in plain words (e.g. 'the + new tab icon at the top of Chrome', 'the New meeting button', 'the first search result link'). Returns screen coordinates {x, y}; then call `click` with them. found=false means it isn't visible (scroll or rethink). Requires Screen Recording permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": { "type": "string", "description": "Plain-language description of the on-screen element to locate." }
                    },
                    "required": ["description"]
                }
            }),
            danger: DangerLevel::ReadOnly,
            handler: locate_on_screen,
        },
        Tool {
            name: "click_element",
            declaration: json!({
                "name": "click_element",
                "description": "Click an on-screen UI element of the focused app by its visible label (e.g. 'Send', 'New Chat', 'Submit', 'OK'). Finds the best-matching element from the accessibility tree and clicks its center. Optionally pass `role` (e.g. 'AXButton', 'AXLink', 'AXCheckBox') to disambiguate. Prefer this over raw `click` when you know the element's text — it's robust to layout changes. Call `read_screen_elements` first if unsure of the exact label.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string", "description": "Visible text/label of the element to click." },
                        "role":  { "type": "string", "description": "Optional accessibility role filter, e.g. 'AXButton', 'AXLink', 'AXTextField'." }
                    },
                    "required": ["label"]
                }
            }),
            danger: DangerLevel::Mutating,
            handler: click_element,
        },
        Tool {
            name: "click",
            declaration: json!({
                "name": "click",
                "description": "Click the mouse at absolute screen coordinates (x, y). Use when you have coordinates from `read_screen_elements` (center_x/center_y) or `get_screen_info`, or for canvas/web/Electron UIs where element labels aren't available. Omit x/y to click at the current cursor position. Prefer `click_element` when you know the element's label.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "x": { "type": "integer", "description": "Absolute screen X (points). Omit to click at current cursor." },
                        "y": { "type": "integer", "description": "Absolute screen Y (points). Omit to click at current cursor." },
                        "button": { "type": "string", "description": "'left' (default), 'right', or 'middle'." },
                        "double": { "type": "boolean", "description": "If true, double-click. Default false." }
                    },
                    "required": []
                }
            }),
            danger: DangerLevel::Mutating,
            handler: click,
        },
        Tool {
            name: "move_mouse",
            declaration: json!({
                "name": "move_mouse",
                "description": "Move the mouse cursor to absolute screen coordinates (x, y) without clicking. Useful to hover, or to position before a separate click/drag.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "x": { "type": "integer" },
                        "y": { "type": "integer" }
                    },
                    "required": ["x", "y"]
                }
            }),
            danger: DangerLevel::Mutating,
            handler: move_mouse,
        },
        Tool {
            name: "scroll",
            declaration: json!({
                "name": "scroll",
                "description": "Scroll the focused window/area. Positive amount scrolls down (or right if horizontal); negative scrolls up/left. Amount is in scroll 'lines' (a few lines per call is typical).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "amount": { "type": "integer", "description": "Lines to scroll; positive=down/right, negative=up/left. Default 3." },
                        "horizontal": { "type": "boolean", "description": "Scroll horizontally instead of vertically. Default false." }
                    },
                    "required": []
                }
            }),
            danger: DangerLevel::Mutating,
            handler: scroll,
        },
        Tool {
            name: "get_screen_info",
            declaration: json!({
                "name": "get_screen_info",
                "description": "Return the main display size (width/height in points) and the current mouse cursor position. Use to understand the coordinate space before moving/clicking.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            danger: DangerLevel::ReadOnly,
            handler: get_screen_info,
        },
        Tool {
            name: "list_apps",
            declaration: json!({
                "name": "list_apps",
                "description": "List the names of the apps currently running with a visible UI (foreground apps). Use to discover the exact app name before focus_app / quit_app, or to answer 'what apps are open'.",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }),
            danger: DangerLevel::ReadOnly,
            handler: list_apps,
        },
        Tool {
            name: "quit_app",
            declaration: json!({
                "name": "quit_app",
                "description": "Quit a running macOS app by name (graceful quit; apps may prompt to save). Use when the user says 'close X', 'quit Y'. Confirm with the user if the app may have unsaved work.",
                "parameters": {
                    "type": "object",
                    "properties": { "app_name": { "type": "string" } },
                    "required": ["app_name"]
                }
            }),
            danger: DangerLevel::Destructive,
            handler: quit_app,
        },
        Tool {
            name: "wait",
            declaration: json!({
                "name": "wait",
                "description": "Pause briefly (milliseconds) to let the UI settle between actions — e.g. after focus_app or click before reading the screen or typing. Max 5000 ms.",
                "parameters": {
                    "type": "object",
                    "properties": { "ms": { "type": "integer", "description": "Milliseconds to wait (≤5000). Default 500." } },
                    "required": []
                }
            }),
            danger: DangerLevel::ReadOnly,
            handler: wait_tool,
        },
        Tool {
            name: "run_shell",
            declaration: json!({
                "name": "run_shell",
                "description": "Run a shell command on the user's Mac and return its stdout/stderr/exit code. This is often the MOST RELIABLE way to do real work — manage files, run git, build/test code, query the system — instead of clicking through GUIs. Runs via `sh -c`. Times out (default 20s, max 120). DESTRUCTIVE: always gated by the safety policy and (outside dev mode) confirmed with the user. Do NOT use for interactive programs that need a TTY.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command line to run." },
                        "workdir": { "type": "string", "description": "Optional working directory (defaults to Jarvis's cwd)." },
                        "timeout_secs": { "type": "integer", "description": "Optional timeout in seconds (1–120, default 20)." }
                    },
                    "required": ["command"]
                }
            }),
            danger: DangerLevel::Destructive,
            handler: run_shell,
        },
        Tool {
            name: "read_file",
            declaration: json!({
                "name": "read_file",
                "description": "Read a text file's contents (up to ~12k chars). Use to read code, configs, notes, logs. Pass an absolute path or one starting with ~/.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }),
            danger: DangerLevel::ReadOnly,
            handler: read_file,
        },
        Tool {
            name: "write_file",
            declaration: json!({
                "name": "write_file",
                "description": "Create or OVERWRITE a text file with the given content (creates parent folders). Use to write code, save notes, edit configs. DESTRUCTIVE: overwrites without merge; gated by the safety policy and confirmed outside dev mode. For surgical edits, read_file first then write the full new content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":    { "type": "string" },
                        "content": { "type": "string", "description": "Full new file contents." }
                    },
                    "required": ["path", "content"]
                }
            }),
            danger: DangerLevel::Destructive,
            handler: write_file,
        },
        Tool {
            name: "list_dir",
            declaration: json!({
                "name": "list_dir",
                "description": "List the entries (files and folders) in a directory. Defaults to the home folder. Pass an absolute path or one starting with ~/.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": []
                }
            }),
            danger: DangerLevel::ReadOnly,
            handler: list_dir,
        },
        Tool {
            name: "search_files",
            declaration: json!({
                "name": "search_files",
                "description": "Find files whose NAME contains a query string, under a directory (defaults to home). Returns up to 50 matching paths. Use to locate a file before reading or opening it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Substring to match in file/folder names." },
                        "dir":   { "type": "string", "description": "Optional root directory to search under (default: home)." }
                    },
                    "required": ["query"]
                }
            }),
            danger: DangerLevel::ReadOnly,
            handler: search_files,
        },
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
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Drop any other control character (0x00–0x1F, 0x7F). These can't
            // appear in a legitimate app name and would otherwise be a vector
            // for breaking out of the AppleScript string literal.
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {}
            c => out.push(c),
        }
    }
    out
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
// Dev-agent orchestrator tools — dispatched to the Python sidecar (orchestrator/)
// via the global mcp::McpManager. The MCP tool names match these tool names, so
// each handler just forwards its args. Spawning is gated as Destructive; the
// worktree isolation keeps follow-up instructions recoverable (Mutating).
// ──────────────────────────────────────────────────────────────────────────────

fn orchestrator_call(tool: &str, args: &Value) -> Result<Value> {
    let mcp = crate::mcp::global().ok_or_else(|| anyhow::anyhow!("MCP manager not running"))?;
    mcp.orchestrator_call(tool, args.clone())
}

fn spawn_dev_agent(args: &Value) -> Result<Value> {
    orchestrator_call("spawn_dev_agent", args)
}

fn instruct_dev_agent(args: &Value) -> Result<Value> {
    orchestrator_call("instruct_dev_agent", args)
}

fn list_dev_agents(args: &Value) -> Result<Value> {
    orchestrator_call("list_dev_agents", args)
}

fn get_agent_status(args: &Value) -> Result<Value> {
    orchestrator_call("get_agent_status", args)
}

fn stop_dev_agent(args: &Value) -> Result<Value> {
    orchestrator_call("stop_dev_agent", args)
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

    // Native synthetic keystroke via enigo (cross-platform; more reliable than
    // System Events `keystroke` and works in apps that ignore Apple Events).
    control::press_keys(key, &modifiers)?;
    Ok(json!({ "ok": true, "key": key, "modifiers": modifiers }))
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

// ──────────────────────────────────────────────────────────────────────────────
// JARVIS 2.0 computer-control tools — native input (enigo, cross-platform),
// accessibility-grounded clicking (macOS AX), shell, and file operations. All
// flow through `dispatch` below, so each is gated by the safety policy.
// ──────────────────────────────────────────────────────────────────────────────

fn read_screen_elements(_args: &Value) -> Result<Value> {
    let els = control::read_ui_elements(120)?;
    Ok(json!({
        "count": els.len(),
        "elements": els,
        "note": "Each element has role, label, and screen coordinates (center_x/center_y). Click one with click_element (by label) or click (by coordinates).",
    }))
}

fn locate_on_screen(args: &Value) -> Result<Value> {
    let desc = args
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: description"))?;
    let (xf, yf, found) = crate::brain::vision::locate_on_screen(desc)?;
    if !found {
        return Ok(json!({
            "found": false,
            "note": "Not visible on screen — try scrolling, or use read_screen_elements to list elements."
        }));
    }
    let (sw, sh) = control::screen_size().unwrap_or((0, 0));
    if sw == 0 || sh == 0 {
        return Ok(json!({ "error": "couldn't determine screen size to convert coordinates" }));
    }
    let x = (xf * sw as f64).round() as i32;
    let y = (yf * sh as f64).round() as i32;
    Ok(json!({
        "found": true, "x": x, "y": y,
        "note": format!("'{desc}' is at screen point ({x}, {y}). Click it with click(x={x}, y={y})."),
    }))
}

fn click_element(args: &Value) -> Result<Value> {
    let label = args
        .get("label")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: label"))?;
    let role = args
        .get("role")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let el = control::click_ui_element(label, role)?;
    Ok(json!({ "ok": true, "clicked": el }))
}

fn move_mouse(args: &Value) -> Result<Value> {
    let x = int_arg(args, "x")?;
    let y = int_arg(args, "y")?;
    control::move_mouse(x, y)?;
    Ok(json!({ "ok": true, "x": x, "y": y }))
}

fn click(args: &Value) -> Result<Value> {
    let x = args.get("x").and_then(|v| v.as_i64()).map(|n| n as i32);
    let y = args.get("y").and_then(|v| v.as_i64()).map(|n| n as i32);
    let button = args.get("button").and_then(|v| v.as_str()).unwrap_or("left");
    let double = args.get("double").and_then(|v| v.as_bool()).unwrap_or(false);
    control::click(x, y, button, double)?;
    Ok(json!({ "ok": true, "x": x, "y": y, "button": button, "double": double }))
}

fn scroll(args: &Value) -> Result<Value> {
    let amount = args.get("amount").and_then(|v| v.as_i64()).unwrap_or(3) as i32;
    let horizontal = args.get("horizontal").and_then(|v| v.as_bool()).unwrap_or(false);
    control::scroll(amount, horizontal)?;
    Ok(json!({ "ok": true, "amount": amount, "horizontal": horizontal }))
}

fn get_screen_info(_args: &Value) -> Result<Value> {
    let (w, h) = control::screen_size().unwrap_or((0, 0));
    let (cx, cy) = control::cursor_position().unwrap_or((0, 0));
    Ok(json!({ "screen_width": w, "screen_height": h, "cursor_x": cx, "cursor_y": cy }))
}

fn wait_tool(args: &Value) -> Result<Value> {
    let ms = args.get("ms").and_then(|v| v.as_u64()).unwrap_or(500).min(5000);
    std::thread::sleep(std::time::Duration::from_millis(ms));
    Ok(json!({ "ok": true, "waited_ms": ms }))
}

fn list_apps(_args: &Value) -> Result<Value> {
    let out = osascript(
        r#"tell application "System Events" to get name of every process whose background only is false"#,
    )?;
    let apps: Vec<String> = out
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Ok(json!({ "apps": apps }))
}

fn quit_app(args: &Value) -> Result<Value> {
    let app = args
        .get("app_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: app_name"))?;
    osascript(&format!(
        r#"tell application "{}" to quit"#,
        applescript_escape(app)
    ))?;
    Ok(json!({ "ok": true, "quit": app }))
}

fn read_file(args: &Value) -> Result<Value> {
    let path = path_arg(args)?;
    let meta = std::fs::metadata(&path).map_err(|e| anyhow::anyhow!("stat {path}: {e}"))?;
    if meta.len() > 2_000_000 {
        return Ok(json!({
            "error": format!("{path} is {} bytes — too large to read inline. Use run_shell with head/grep/sed instead.", meta.len())
        }));
    }
    let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(json!({ "path": path, "bytes": meta.len(), "content": cap_chars(&text, 12_000) }))
}

fn write_file(args: &Value) -> Result<Value> {
    let path = path_arg(args)?;
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, content).map_err(|e| anyhow::anyhow!("write {path}: {e}"))?;
    Ok(json!({ "ok": true, "path": path, "bytes": content.len() }))
}

fn list_dir(args: &Value) -> Result<Value> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(expand_tilde)
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let mut entries = Vec::new();
    for e in std::fs::read_dir(&path)
        .map_err(|e| anyhow::anyhow!("read_dir {path}: {e}"))?
        .flatten()
    {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(json!({ "name": e.file_name().to_string_lossy(), "dir": is_dir }));
        if entries.len() >= 200 {
            break;
        }
    }
    Ok(json!({ "path": path, "entries": entries }))
}

fn search_files(args: &Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: query"))?
        .to_lowercase();
    let root = args
        .get("dir")
        .and_then(|v| v.as_str())
        .map(expand_tilde)
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let mut matches = Vec::new();
    search_walk(std::path::Path::new(&root), &query, 0, &mut matches);
    Ok(json!({ "query": query, "root": root, "matches": matches }))
}

fn search_walk(dir: &std::path::Path, query: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 6 || out.len() >= 50 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if out.len() >= 50 {
            return;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // skip dotfiles / hidden dirs
        }
        let p = e.path();
        if name.to_lowercase().contains(query) {
            out.push(p.to_string_lossy().to_string());
        }
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            search_walk(&p, query, depth + 1, out);
        }
    }
}

fn run_shell(args: &Value) -> Result<Value> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: command"))?
        .to_string();
    let workdir = args.get("workdir").and_then(|v| v.as_str()).map(expand_tilde);
    let timeout = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .clamp(1, 120);

    use std::process::Stdio;
    let mut cmd = shell_command();
    cmd.arg(&command).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = &workdir {
        cmd.current_dir(d);
    }
    let child = cmd.spawn().map_err(|e| anyhow::anyhow!("spawn: {e}"))?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(timeout)) {
        Ok(Ok(out)) => Ok(json!({
            "exit_code": out.status.code(),
            "stdout": cap_chars(&String::from_utf8_lossy(&out.stdout), 10_000),
            "stderr": cap_chars(&String::from_utf8_lossy(&out.stderr), 4_000),
        })),
        Ok(Err(e)) => Err(anyhow::anyhow!("run: {e}")),
        Err(_) => {
            kill_pid(pid);
            Ok(json!({ "error": format!("command timed out after {timeout}s and was killed") }))
        }
    }
}

#[cfg(unix)]
fn shell_command() -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.arg("-c");
    c
}
#[cfg(windows)]
fn shell_command() -> std::process::Command {
    let mut c = std::process::Command::new("cmd");
    c.arg("/C");
    c
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}
#[cfg(not(unix))]
fn kill_pid(_pid: u32) {}

// ── small arg / text helpers ────────────────────────────────────────────────

fn int_arg(args: &Value, key: &str) -> Result<i32> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .map(|n| n as i32)
        .ok_or_else(|| anyhow::anyhow!("missing/invalid integer field: {key}"))
}

fn path_arg(args: &Value) -> Result<String> {
    args.get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(expand_tilde)
        .ok_or_else(|| anyhow::anyhow!("missing required field: path"))
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(h) = std::env::var("HOME") {
            return format!("{h}/{rest}");
        }
    }
    p.to_string()
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let t: String = s.chars().take(max).collect();
    format!("{t}…[truncated]")
}

// ──────────────────────────────────────────────────────────────────────────────
// Dispatch — the single choke point every tool call flows through. Enforces the
// safety policy, surfaces a confirmation when required, runs the handler, and
// records the action in the activity feed + audit log. All three LLM providers
// call this instead of invoking handlers directly.
// ──────────────────────────────────────────────────────────────────────────────

/// True once this turn has ingested untrusted content (web page / clipboard /
/// screen / file). A subsequent mutation could be prompt-injection-driven, so
/// the policy force-confirms it. Reset each turn by `begin_turn`.
static TURN_TAINTED: AtomicBool = AtomicBool::new(false);

/// Called at the start of every brain turn (from `brain::Brain::ask`).
pub fn begin_turn() {
    TURN_TAINTED.store(false, Ordering::Release);
}

pub fn turn_is_tainted() -> bool {
    TURN_TAINTED.load(Ordering::Acquire)
}

fn is_untrusted_reader(name: &str) -> bool {
    matches!(
        name,
        "get_browser_context"
            | "get_clipboard"
            | "see_screen"
            | "read_screen_elements"
            | "locate_on_screen"
            | "read_file"
    )
}

/// Run a tool by name with the safety policy applied. Returns a JSON value the
/// provider feeds back to the model (errors are returned as `{ "error": … }`,
/// never panics).
pub fn dispatch(tools_reg: &[Tool], name: &str, args: &Value) -> Value {
    let Some(tool) = tools_reg.iter().find(|t| t.name == name) else {
        return json!({ "error": format!("unknown tool: {name}") });
    };
    let id = crate::gate::next_id();
    let tainted = turn_is_tainted();
    let pol = policy::global();
    let summary = policy::describe(name, args);

    match pol.decide(name, tool.danger, args, tainted) {
        Decision::Deny { reason } => {
            tracing::warn!("policy: DENY {name} — {reason}");
            // Activity feed shows the canonical action summary (consistent with
            // every other state); the raw reason goes to the audit log + the
            // error result the brain relays to the user.
            crate::gate::emit_activity(id, name, &summary, tool.danger, "denied");
            audit_record(id, name, args, tool.danger, "denied", Some(&reason));
            return json!({
                "error": format!("Refused by the safety policy: {reason}. Tell the user why, and stop — do not retry.")
            });
        }
        Decision::Confirm { summary: prompt, danger: prompt_danger } => {
            tracing::info!("policy: CONFIRM {name} — {prompt}");
            // Activity feed + audit use the canonical summary + tool.danger so a
            // single action reads consistently across awaiting/running/ok states;
            // the spoken/HUD PROMPT uses the decision's (possibly taint-annotated,
            // possibly escalated) wording, which is the right UX for the ask.
            crate::gate::emit_activity(id, name, &summary, tool.danger, "awaiting");
            let approved =
                crate::gate::request_confirmation(id, &prompt, prompt_danger, pol.confirm_timeout_secs);
            if !approved {
                crate::gate::emit_activity(id, name, &summary, tool.danger, "denied");
                audit_record(id, name, args, tool.danger, "denied_by_user", None);
                return json!({
                    "error": "The user declined this action (or it timed out). Do NOT retry it; ask what they'd like instead."
                });
            }
        }
        Decision::Allow => {}
    }

    crate::gate::emit_activity(id, name, &summary, tool.danger, "running");
    let result = match (tool.handler)(args) {
        Ok(v) => v,
        Err(e) => json!({ "error": e.to_string() }),
    };
    if is_untrusted_reader(name) {
        TURN_TAINTED.store(true, Ordering::Release);
    }
    let status = if result.get("error").is_some() { "error" } else { "ok" };
    crate::gate::emit_activity(id, name, &summary, tool.danger, status);
    audit_record(id, name, args, tool.danger, status, None);
    result
}

fn audit_record(id: u64, name: &str, args: &Value, danger: DangerLevel, status: &str, reason: Option<&str>) {
    crate::gate::audit(json!({
        "id": id,
        "tool": name,
        "danger": danger.as_str(),
        "status": status,
        "reason": reason,
        "args": redact_args(args),
        "ts": now_unix_secs(),
    }));
}

/// Truncate long string args before they reach the audit log (so we don't dump
/// pasted documents or secrets verbatim). Coarse but safe.
fn redact_args(args: &Value) -> Value {
    match args {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let rv = match v {
                    Value::String(s) if s.len() > 200 => {
                        Value::String(format!("{}…[{}B]", &s[..safe_cut(s, 200)], s.len()))
                    }
                    other => other.clone(),
                };
                out.insert(k.clone(), rv);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn safe_cut(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
