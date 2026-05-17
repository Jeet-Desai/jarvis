# Jarvis

A Tony Stark-style voice assistant for macOS. Always-on wake word, a holographic orb that overlays every screen (even fullscreen apps), pluggable LLM brain with automatic fallback, conversational memory, browser-context awareness, and a HUD reminiscent of Iron Man's helmet.

> **v1 status: macOS only.** Apple Silicon recommended. Built in Tauri 2 + Rust + SolidJS + Three.js.

---

## What it does

- **Wake on "Jarvis"** — say the word, the orb wakes up and starts listening. (Hotkey `⇧⌘J` also works.)
- **Voice → voice loop** — your speech transcribed locally (whisper.cpp on Metal), sent to an LLM, response read aloud (macOS `say`).
- **Reads webpages** — *"summarize this page"* in Chrome / Safari / Comet / Arc / Brave / Edge / Vivaldi → Jarvis pulls the URL, title, and visible text via AppleScript and answers.
- **Reads anywhere via clipboard** — for non-browser apps (Slack, WhatsApp, VS Code, …): you select+copy, Jarvis reads what you copied.
- **Always-on-top overlay** — orb appears over any window, including fullscreen apps, on every macOS Space. Powered by NSPanel.
- **Pluggable brain** — drop in one or more of Gemini, OpenAI, OpenRouter, Groq, Together, Mistral, or local Ollama. Automatic fallback on rate-limit / network errors.
- **Conversation memory** — Jarvis remembers prior turns within a session. `⇧⌘R` to wipe.
- **Interruptible** — `⇧⌘S` hard-stops Jarvis mid-response.

---

## Requirements

| | |
|---|---|
| **OS** | macOS 12 Monterey or later |
| **CPU** | Apple Silicon strongly preferred (M-series). Whisper uses Metal — Intel will fall back to CPU and be significantly slower. |
| **RAM** | 8 GB minimum, 16 GB+ if you want to run a local Ollama model |
| **Disk** | ~500 MB for the app + whisper model + node_modules |
| **Internet** | Needed only for cloud LLM providers (Gemini / OpenAI / OpenRouter). Pure local stack works fully offline if you use Ollama + macOS `say`. |
| **Mic** | Built-in or USB microphone |
| **Permissions** | Microphone (required), Input Monitoring (for global hotkeys, auto-prompted) |

### Tooling installed automatically by `setup.sh`

- Xcode Command Line Tools (you'll be prompted by macOS if missing)
- Homebrew (you install yourself if missing — https://brew.sh)
- `cmake` (via brew, needed by whisper.cpp's build)
- Rust toolchain (via rustup)
- Node + pnpm (via brew / npm)
- Frontend dependencies (`pnpm install`)
- Whisper model `ggml-base.en.bin` (~140 MB, from huggingface.co)

### What you need at least one of (LLM provider)

| Provider | Cost | Setup time |
|---|---|---|
| **Google Gemini** *(recommended)* | Free tier (250 req/day) | 1 min — sign up at https://aistudio.google.com/apikey |
| **OpenAI** | Pay-as-you-go ($5 minimum) | https://platform.openai.com — `gpt-4o-mini` is ~$0.15/M tokens |
| **OpenRouter** (free Llama/Qwen) | Free | https://openrouter.ai/keys |
| **Local Ollama** | Free, runs on your Mac | `brew install ollama && ollama pull qwen2.5:7b && ollama serve` |

You only need **one**. Adding more enables automatic failover.

---

## Setup (5 minutes)

```bash
git clone https://github.com/jeet-desai/jarvis.git
cd jarvis
./setup.sh           # checks prereqs, installs missing ones, downloads model
$EDITOR .env         # add at least one LLM key (see .env.example)
pnpm tauri dev       # first build is 5-10 min (whisper.cpp compile is heavy)
```

When you run it for the first time:

1. macOS will ask for **Microphone access** → click Allow
2. (Optionally) macOS will ask for **Input Monitoring** for global hotkeys → click Open Settings → enable Jarvis
3. The orb appears in your top-right corner
4. Say *"Jarvis, what time is it?"*

---

## Hotkeys

| | |
|---|---|
| `⇧⌘J` | Jarvis on/off (or interrupt-then-listen during speaking) |
| `⇧⌘S` | Hard stop (kills TTS, returns to Idle) |
| `⇧⌘H` | Toggle HUD overlay |
| `⇧⌘C` | Close everything (orb + HUD) / bring back |
| `⇧⌘R` | Reset conversation memory |
| `⇧⌘+` / `⇧⌘-` | Orb brighter / dimmer |
| `⇧⌘P` | Cycle through states for previewing animations (dev) |

You can also say *"Jarvis"* alone — wake word is whisper-based, no extra dependencies.

---

## Tools the LLM can call

- `get_current_time` — date/time queries
- `get_focused_app` — what app the user is in
- `get_browser_context` — URL + title + ~4 KB of visible page text (Chromium browsers + Safari)
- `get_clipboard` — what the user just copied (works in any app, no permission)
- `see_screen` — capture + Gemini vision describe (requires Screen Recording permission)
- `list_calendar_events` — read events from Google Calendar (requires one-time setup below)
- `create_calendar_event` — add events to Google Calendar (same setup)

Add your own in `src-tauri/src/tools.rs`. Plug in any [MCP server](https://modelcontextprotocol.io/) (Slack, GitHub, Notion, Postgres, ...) by adding a few lines in `src-tauri/src/mcp.rs`.

---

## Calendar setup (optional, one-time)

Jarvis talks to Google Calendar through the [`@cocal/google-calendar-mcp`](https://github.com/nspady/google-calendar-mcp) MCP server, spawned on demand via `npx`. You need to grant OAuth access to your own calendar once.

1. **Create a Google Cloud project** at https://console.cloud.google.com/ (use your **personal** Google account, not work).
2. **APIs & Services → Library** → enable **Google Calendar API**.
3. **APIs & Services → OAuth consent screen** → External → fill in app name "Jarvis" → add yourself as a Test User under "Test users" (keeps the app in Testing mode, no Google verification needed).
4. **APIs & Services → Credentials → Create Credentials → OAuth client ID** → **Desktop app** → download the JSON.
5. Save it somewhere private:
   ```bash
   mkdir -p ~/.config/jarvis
   mv ~/Downloads/client_secret_*.json ~/.config/jarvis/gcp-oauth.keys.json
   chmod 600 ~/.config/jarvis/gcp-oauth.keys.json
   ```
6. Add to `.env`:
   ```
   GOOGLE_OAUTH_CREDENTIALS=/Users/you/.config/jarvis/gcp-oauth.keys.json
   ```
7. Restart Jarvis (`pnpm tauri dev`). On first calendar query, a browser tab opens for consent. Token is then cached on disk; subsequent queries are silent.

If the file isn't there, calendar tools simply respond with a setup hint — Jarvis still works for everything else.

---

## Architecture

```
   mic ──▶ cpal (audio thread) ──▶ rolling buffer
                                       │
              ┌────────────────────────┼────────────────────────┐
              ▼                        ▼                        ▼
       wake-word watcher       active recording           VAD detector
       (whisper.cpp every       (when Listening)         (auto end on silence)
        ~600 ms on idle)
              │                        │
              └──── "Jarvis"     end-of-utterance
                       │                │
                       ▼                ▼
                 set Listening    transcribe → text
                                       │
                                       ▼
                              Brain (pluggable provider)
                              Gemini → OpenAI-compat → Ollama
                                       │   fallback on rate-limit
                                       ▼
                        tools (browser, clipboard, time, …)
                                       │
                                       ▼
                               final text reply
                                       │
                                       ▼
                              macOS `say` (TTS)
                                       │
                                       ▼
                          orb animates "speaking"
```

| Layer | Tech |
|---|---|
| Window shell | Tauri 2 (Rust core + WebKit webview) |
| Overlay | `tauri-nspanel` (converts NSWindow → NSPanel, level=1000, canJoinAllSpaces + fullScreenAuxiliary) |
| Mic capture | `cpal` (Core Audio backend) |
| STT | `whisper-rs` (whisper.cpp + Metal) |
| Resampling | `rubato` (48k → 16k for whisper) |
| Brain orchestrator | Custom (`src-tauri/src/brain/`) |
| TTS | macOS `say` subprocess |
| UI | SolidJS + Three.js + custom GLSL shader |
| Hotkeys | `tauri-plugin-global-shortcut` |
| AppleScript bridges | `osascript` subprocess (browser context) |
| Clipboard | `pbpaste` subprocess |
| Vision | Gemini 2.5 Flash multimodal (screenshot via `screencapture` + `sips`) |

---

## Roadmap

- [ ] Cross-platform (Linux + Windows) — needs alternate TTS, screenshot, clipboard backends and a non-NSPanel overlay strategy
- [ ] Acoustic echo cancellation so auto-barge-in (interrupt by speaking) becomes reliable without false self-triggers
- [ ] Kokoro-82M ONNX local TTS (replace `say` with character-driven Jarvis voice)
- [ ] Accessibility API integration (read text from any focused app without copy/paste)
- [ ] Click + type tools (Jarvis can drive the screen, not just read it)
- [ ] MCP server tools (calendar, email, Slack, GitHub via the user's existing MCP ecosystem)
- [ ] Configurable orb position (drag-anywhere persists; corner presets)
- [ ] Per-conversation persona / system prompt overrides

---

## Troubleshooting

**No reply, just silence after speaking**
Check the dev console — likely your LLM provider hit rate-limit and there's no fallback configured. Add another provider key in `.env`.

**"Rate limit reached" spoken aloud frequently**
Gemini free tier is 10 RPM. Add OpenRouter (free Llama) or local Ollama as fallback in `.env`.

**Wake word doesn't fire reliably**
Whisper.cpp isn't optimized for wake-word detection. For sub-100ms wake response, swap to Picovoice Porcupine (requires free account at picovoice.ai with work-domain email).

**Mic permission keeps re-prompting**
Each rebuild of the dev binary may re-trigger TCC. After grant, build once with `pnpm tauri build` and run the bundled `.app` to get a stable identity.

**`see_screen` doesn't work, opens Settings demanding admin password**
If Screen Recording can't be granted, use `get_clipboard` instead — works on every Mac without permission.

**Bloom halo creates a visible square around the orb**
Confirm you're on the latest build — the bloom was tuned to fit inside the orb-window bounds. If still visible, lower `BLOOM_BASE_INTENSITY` in `src/shared/OrbScene.tsx`.

---

## License

All rights reserved. Source visible for reference and personal use only. Do not redistribute, sublicense, or use in derivative work without explicit permission.

