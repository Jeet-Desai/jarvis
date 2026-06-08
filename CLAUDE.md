# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
./setup.sh             # one-time: installs cmake/rust/pnpm, downloads ggml-base.en.bin
pnpm tauri dev         # run the app (first build is 5–10 min — whisper.cpp + Metal compile)
pnpm tauri build       # produce a signed .app bundle (stable TCC identity → mic permission survives rebuilds)
pnpm build             # frontend-only Vite build (outputs to dist/, consumed by Tauri)
cargo check --manifest-path src-tauri/Cargo.toml   # fast Rust type-check without linking whisper
cargo build --manifest-path src-tauri/Cargo.toml   # full Rust build
```

There is no test suite and no linter configured. `pnpm` is required (workspace is pinned via `pnpm-workspace.yaml`); do not substitute npm/yarn.

The frontend `pnpm dev` is automatically invoked by `tauri dev` via `beforeDevCommand` in `tauri.conf.json` on port 1420 — do not start Vite separately.

macOS only. The build assumes Apple Silicon + Metal; `whisper-rs` is compiled with the `metal` feature flag in `src-tauri/Cargo.toml`.

## Architecture

Jarvis is a Tauri 2 desktop app: Rust core in `src-tauri/`, SolidJS + Three.js frontend in `src/`. The Rust side owns the always-on mic, STT, the LLM brain, tool dispatch, and TTS; the frontend is a thin presentation layer (an orb window + a HUD window) driven entirely by Tauri events.

### Two windows, both NSPanels

`tauri.conf.json` declares regular Tauri webviews `orb` and `hud`. In `src-tauri/src/lib.rs` they are converted to NSPanels via the `tauri_panel!` macro and `tauri-nspanel` (panel level 1000, `canJoinAllSpaces`, `fullScreenAuxiliary`). This NSPanel conversion — not standard `alwaysOnTop` — is what makes the overlay appear above fullscreen apps and across every Space. **Both panels must be defined in one `tauri_panel!` invocation**; the macro emits duplicate imports if called twice.

`src/index.tsx` is a single entry point that branches on `window.location.pathname`: `/hud` renders the HUD, anything else renders the orb. There is no router.

### Audio pipeline (always-on, single cpal stream)

`src-tauri/src/audio.rs` opens **one** cpal stream at app launch that lives until exit. This is deliberate — a wake-word detector needs continuous audio, so per-utterance start/stop is not an option. State is flipped via an `AtomicBool` (`is_recording`); when false, samples still flow through VAD and a 2-second rolling buffer but are not retained. The cpal stream is `!Send` on macOS (CoreAudio callback runs on a CFRunLoop) so it lives on a dedicated thread with RAII; sharing happens through `Arc<PipeInner>`.

VAD is `webrtc-vad` (the Chrome/Firefox WebRTC VAD), **not** an RMS threshold. The old RMS scheme caused stuck-in-Listening 30s timeouts in quiet rooms; do not regress to it. Tuning constants (`MIN_SPEECH_MS`, `SILENCE_TO_STOP_MS=700`, `MAX_UTTERANCE_MS=30_000`) match ChatGPT Voice / Gemini Live defaults.

Barge-in uses a lightweight "AEC lite" — during the first `GRACE_MS` of Jarvis's own TTS, the trigger is suppressed and ambient RMS is sampled; afterward the user must speak above `baseline + MARGIN` to interrupt. This prevents Jarvis from interrupting itself without true echo cancellation.

Wake-word detection (`wake.rs`) re-runs whisper.cpp on the rolling buffer every ~600 ms while Idle. Slow but zero extra dependencies; swapping to Picovoice Porcupine is noted as the path to sub-100ms wake.

Resampling 48k → 16k for whisper goes through `rubato`.

### Brain — pluggable providers with fallback

`src-tauri/src/brain/mod.rs` defines `LlmProvider` (`name()` + `generate(history, tools) -> String`). `Brain::from_env()` detects which providers have credentials in `.env` and orders them by `LLM_PROVIDERS=` (default: gemini → openai → ollama). Implementations: `gemini.rs`, `openai.rs` (OpenAI-compatible, also covers OpenRouter / Groq / Together / Mistral / DeepSeek via `OPENAI_BASE_URL`), `ollama.rs`. Vision goes through `brain/vision.rs` (Gemini 2.5 Flash multimodal).

Key design constraints:

- **Conversation history is normalized** to `Message { role, content }` plain text (no provider-specific tool-use blobs). This lets a request fall over to the next provider mid-conversation without translation; per-turn tool calls stay private to the provider.
- **Each provider owns its own tool-use loop** — Gemini's function-calling, OpenAI's `tools[]`, and Ollama's JSON-shape differ enough that a unified loop was uglier than per-provider impls. When adding a new provider, expect to implement this loop too.
- **Fallback is triggered by `is_retryable()`** matching substrings: rate-limit (429 / quota / resource_exhausted), timeouts, network errors (DNS / TLS / connection refused), and `empty reply`. Non-retryable errors abort the turn and the user message is popped from history to keep state consistent.
- `MAX_HISTORY_TURNS = 12`, `MAX_TOOL_ROUNDS = 6`, `REQUEST_TIMEOUT_SECS = 30`.

The system prompt is a long block in `brain/mod.rs` enforcing **mandatory tool-call rules** (always call `get_focused_app` for screen/page questions; never reuse stale clipboard or stale time; never introspect about infrastructure health). When editing the system prompt, preserve those numbered rules — they exist to suppress specific past hallucinations.

### Tools and MCP

Built-in tools live in `src-tauri/src/tools.rs` (registry exposed to the brain). They shell out via subprocess to macOS-native bindings: `osascript` for browser context, `pbpaste` for clipboard, `screencapture`+`sips` for vision, plus pure-Rust ones like `get_current_time`. Adding a new built-in tool = extend the registry there.

External tools come from MCP servers spawned over stdio via the `rmcp` crate (`src-tauri/src/mcp.rs`). Currently only `@cocal/google-calendar-mcp` (spawned via `npx`) — gated on `GOOGLE_OAUTH_CREDENTIALS` env var; if absent, calendar tools politely no-op and the rest of Jarvis still works.

### State machine and conversation mode

`src-tauri/src/state.rs` holds the single `AppState`. Five-state machine: `Idle → Listening → Thinking → Speaking → Idle` (with `Error`). The `session: AtomicBool` flag is **conversation mode**: when true, after a turn finishes Jarvis re-arms the mic automatically — no need to re-say "Jarvis". Set by wake word or first `⇧⌘J`, cleared by `⇧⌘S` / `⇧⌘C` / toggling off mid-Listening.

State transitions emit Tauri events that the frontend listens to in `src/shared/state.ts`; the orb's animation (`src/shared/OrbScene.tsx` — custom GLSL via Three.js + postprocessing) responds to those.

### macOS-specific permissions

Mic is requested via standard Tauri/AVAudio. Input Monitoring (for global hotkeys) and Screen Recording (for `see_screen`) are TCC-gated; `src-tauri/src/permissions.rs` and `grant-permissions.sh` handle prompting / re-prompting. Each rebuild of the dev binary may re-trigger TCC because the code-signed identity changes — for stable permission state, run `pnpm tauri build` once and use the bundled `.app`.

`signingIdentity: "Jarvis Code Signing"` in `tauri.conf.json` refers to a local self-signed keychain identity, not an Apple Developer cert. Don't change it without coordinating — TCC keys off the signing identity.

## Conventions worth knowing

- Tracing via the `tracing` crate (no `println!` in Rust paths) — `tracing_subscriber` is wired up at startup.
- Errors propagated with `anyhow::Result`; user-visible error text is curated separately (see lib.rs "non-technical error messages" — logs get the gory detail, the user gets short prose).
- No `unwrap()` in hot paths — TTS / mic / brain failures must degrade gracefully because they happen on a user-facing voice loop.
- The frontend has no build-time type checking step wired into `tauri dev`; `tsc` is only consulted by editor tooling. If you change shared types, build will not catch drift — exercise both windows manually.
