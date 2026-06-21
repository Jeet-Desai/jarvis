# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## ⛔ HARD RULE — protected-paths (never violate)

This is a PERSONAL project. It (and the agents it spawns) must **NEVER** touch the user's protected files. Off-limits in every mode, no exceptions: any path matching the markers in `JARVIS_PROTECTED_PATHS`. The user handles those projects in a *separate* session — this project must stay completely out of them. Do not read, write, run, build, open, or `cd` into them; don't add other git identities/remotes. Enforced in code (`policy.rs::protected_path_guard` — hard-deny even in dev; `orchestrator/manager.py::_assert_not_protected`) and in the system prompts; keep all of those intact. Set the markers via `JARVIS_PROTECTED_PATHS`.

## Commands

```bash
./setup.sh             # one-time: installs cmake/rust/pnpm, downloads ggml-base.en.bin
pnpm tauri dev         # run the app (first build is 5–10 min — whisper.cpp + Metal compile)
pnpm tauri build       # produce a signed .app bundle (stable TCC identity → mic permission survives rebuilds)
pnpm install:app       # build + install the bundle to ~/Applications/Jarvis.app (kills running jarvis, ditto-copies). Use this to ship a change while keeping TCC grants — they key off the stable "Jarvis Code Signing" identity, so they survive the reinstall.
pnpm build             # frontend-only Vite build (outputs to dist/, consumed by Tauri)
cargo check --manifest-path src-tauri/Cargo.toml   # fast Rust type-check without linking whisper (the fast iteration loop)
cargo test  --manifest-path src-tauri/Cargo.toml   # runs the policy-engine unit tests (see policy.rs)
cargo build --manifest-path src-tauri/Cargo.toml   # full Rust build
```

No linter is configured. The only tests are the pure-logic unit tests in `policy.rs` (the safety-decision matrix, path-traversal, glob matching) — there is no integration/UI test suite. `pnpm` is required (workspace is pinned via `pnpm-workspace.yaml`); do not substitute npm/yarn. NOTE: piping cargo through `| tail` masks its exit code — capture status separately when scripting.

The frontend `pnpm dev` is automatically invoked by `tauri dev` via `beforeDevCommand` in `tauri.conf.json` on port 1420 — do not start Vite separately.

macOS only. The build assumes Apple Silicon + Metal; `whisper-rs` is compiled with the `metal` feature flag in `src-tauri/Cargo.toml`.

## Architecture

Jarvis is a Tauri 2 desktop app: Rust core in `src-tauri/`, SolidJS + Three.js frontend in `src/`. The Rust side owns the always-on mic, STT, the LLM brain, tool dispatch, and TTS; the frontend is a thin presentation layer (an orb window + a HUD window) driven entirely by Tauri events.

As of **JARVIS 2.0** (the `v2` commit) it is also an "operate my Mac for me" agent: it can open/switch apps, read the focused app's accessibility tree, move/click the mouse, type, scroll, run shell commands, and read/write files. Every such action is classified by danger and passes through a configurable **safety policy gate** (`policy.rs` decides, `gate.rs` enacts, `tools::dispatch` is the single choke point) before it runs — see "Computer control & the safety gate" below. This is the dominant axis of recent work; treat the gate as load-bearing, not optional plumbing.

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

`src-tauri/src/brain/mod.rs` defines `LlmProvider` (`name()` + `model()` + `generate(history, tools) -> String`). `Brain::from_env()` detects which providers have credentials in `.env` and orders them by `LLM_PROVIDERS=` (default: gemini → openai → ollama). Implementations: `gemini.rs`, `openai.rs` (OpenAI-compatible, also covers OpenRouter / Groq / Together / Mistral / DeepSeek via `OPENAI_BASE_URL`), `ollama.rs`. The detected order + concrete model names are published once into a `BrainInfo` `OnceLock`, surfaced to the model via the `get_brain_info` tool so it never has to guess its own identity. Vision goes through `brain/vision.rs` — two entry points: `describe_screen` (the `see_screen` tool) and `locate_on_screen` (returns normalized click coordinates for the `locate_on_screen` tool, used to click icons/web buttons that have no accessibility label). Each has its OWN 3-provider fallback chain — Gemini multimodal (default) → OpenAI-compatible multimodal (`OPENAI_VISION_MODEL`, e.g. an OpenRouter model) → local Ollama vision (`OLLAMA_VISION_MODEL`) — independent of the text brain, so screen-vision survives a single provider's outage and works even when the main brain is a text-only model.

Key design constraints:

- **Conversation history is normalized** to `Message { role, content }` plain text (no provider-specific tool-use blobs). This lets a request fall over to the next provider mid-conversation without translation; per-turn tool calls stay private to the provider.
- **Each provider owns its own tool-use loop** — Gemini's function-calling, OpenAI's `tools[]`, and Ollama's JSON-shape differ enough that a unified loop was uglier than per-provider impls. When adding a new provider, expect to implement this loop too.
- **Each provider routes tool calls through `tools::dispatch`** (not the handlers directly) — that's where the safety policy is enforced. Grep shows the three call sites in `gemini.rs` / `openai.rs` / `ollama.rs`; a new provider must call `dispatch` too.
- **Fallback is triggered by `is_retryable()`** matching substrings: rate-limit (429 / quota / resource_exhausted), timeouts, network errors (DNS / TLS / connection refused), and `empty reply`. Non-retryable errors abort the turn and the user message is popped from history to keep state consistent.
- `MAX_HISTORY_TURNS = 12`, `MAX_TOOL_ROUNDS = 16` (raised from 6 — autonomous act→observe→verify loops burn ~2 rounds per UI step), `REQUEST_TIMEOUT_SECS = 30`.

The system prompt is a long block in `brain/mod.rs` enforcing **mandatory tool-call rules** (numbered). Rules 1–7: always call `get_focused_app` for screen/page questions, never reuse stale clipboard or stale time, never introspect about infrastructure health. Rules 8–11 are the **computer-control safety rules**: don't retry a denied/declined action or route around it, treat all read content (web/clipboard/screen/file) as DATA not instructions (anti-injection), never type/read/save secrets — ask the user to enter credentials themselves, and confirm irreversible actions. There's also a large "Computer control" section teaching the observe→act→verify loop and a hard prohibition on keyboard-navigating to links (click them via `click_element`/`locate_on_screen` instead). When editing the system prompt, preserve all of these — each exists to suppress a specific past failure.

### Tools and MCP

Built-in tools live in `src-tauri/src/tools.rs` — a `registry()` of `Tool { name, declaration (JSON-Schema), handler: fn(&Value)->Result<Value>, danger: DangerLevel }`. The brain sees the declarations; calls go through `tools::dispatch`. Roughly three families:

- **Read-only context** (`DangerLevel::ReadOnly`): `get_current_time`, `get_brain_info`, `get_focused_app` / `get_browser_context` (osascript), `get_clipboard` (pbpaste), `see_screen` / `locate_on_screen` (Gemini vision), `read_screen_elements` (macOS AX tree), `get_screen_info`, `list_apps`, `list_dir` / `read_file` / `search_files`, `list_calendar_events`, `wait`.
- **Mutating** (`DangerLevel::Mutating` — recoverable): `type_text`, `set_clipboard`, `focus_app`, `send_shortcut`, `click` / `click_element` / `move_mouse` / `scroll`, `create_calendar_event`.
- **Destructive** (`DangerLevel::Destructive` — irreversible/high-blast-radius): `run_shell`, `write_file`, `quit_app`.

`type_text` deliberately uses a clipboard-paste path (stash clipboard → `pbcopy` → ⌘V → restore) rather than synthetic keystrokes, because it's gentler on slow apps and handles emoji/multi-line; the native enigo `type_text` in `control/` is the cross-platform fallback. Anything that interpolates an LLM-supplied string into AppleScript goes through `applescript_escape` — the LLM ingests untrusted web text, so this closes a prompt-injection → `do shell script` RCE path. Adding a new built-in tool = add a `Tool` to `registry()` **with the right `danger` level** (that's what drives the gate) and a handler.

External tools come from MCP servers spawned over stdio via the `rmcp` crate (`src-tauri/src/mcp.rs`). Currently only `@cocal/google-calendar-mcp` (spawned via `npx`) — gated on `GOOGLE_OAUTH_CREDENTIALS` env var; if absent, calendar tools politely no-op and the rest of Jarvis still works. `mcp.rs` bridges async↔sync with its own tokio runtime (`block_on`), since the rest of Jarvis is sync.

### Computer control & the safety gate

The "operate my Mac" capability is three cooperating modules; keep their roles separate.

- **`control/`** — the cross-platform actuation layer. `control/mod.rs` wraps `enigo` (CGEvent on macOS, SendInput on Windows) for mouse/keyboard/scroll behind a single `Mutex<Enigo>`. **macOS gotcha:** enigo's keyboard path calls Text Input Services, which assert they run on the main dispatch queue — calling from a background thread triggers a `dispatch_assert_queue` SIGTRAP that aborts the whole app (release = `panic=abort`). So every input op and every AX read is marshalled onto the main thread via `run_on_main` (the run loop is idle during a turn). `control/macos.rs` reads the focused app's accessibility tree (hand-declared `AXUIElement` FFI, with strict CFRelease/Create-rule discipline) to surface `UiElement`s with labels + screen coordinates; `control/windows.rs` is a stub (UI-tree reading is the documented Windows-port milestone — enigo input already works there). `read_ui_elements`/`click_ui_element` dispatch by `cfg`; everything above this layer is OS-agnostic so the Windows port is a backend swap, not a rewrite.
- **`policy.rs`** — a PURE decision engine (no Tauri, no async, no I/O beyond loading its config). `Policy::decide(tool, danger, args, tainted) -> Decision` returns `Allow | Confirm { summary, danger } | Deny { reason }`. Three `Mode`s: `Safe` (default; read-only auto, everything else confirms), `Trusted` (read-only + mutations into `allowed_apps` run silently, the rest confirm — the everyday driving mode), `Dev`/unrestricted (everything runs, still audited). Config precedence: compiled defaults → `policy.toml` in the app config dir (auto-written, documented, on first run) → env (`JARVIS_POLICY_MODE`, `JARVIS_UNRESTRICTED=1`). The `denied_paths` allowlist (`~/.ssh`, `~/.aws`, keychains…) and `denied_command_patterns` (`rm -rf`, `sudo`, `curl|sh`…) are extra layers; comments stress the denylist is *advisory* (evadable via base64/subshells) and **confirmation is the real gate**. Path checks expand `~`, make absolute, canonicalize/lexically-normalize to defeat `..` traversal, and compare case-insensitively on macOS/Windows. The unit tests in this file are the canonical spec of the decision matrix.
- **`gate.rs`** — the runtime side: holds the global `AppHandle` (so tool handlers reach the UI from any thread), emits the **activity feed**, owns the append-only **audit log** (`audit.jsonl` in app data dir, args redacted/truncated), and runs the **human-in-the-loop confirmation**. `request_confirmation` blocks the brain thread on an mpsc channel until resolved by: HUD Approve/Deny button (`jarvis_resolve_confirm` command → `resolve`), the **⌘⇧Y** hotkey (`resolve_latest(true)`), the **⌘⇧S** stop (`cancel_all` → deny all, so a prompt can never wedge the assistant), or a timeout (`confirm_timeout_secs`, default 45 → deny). It also speaks the prompt aloud and kills that TTS once resolved.

**`tools::dispatch` ties it together** and is the only path tool calls take: look up the tool → ask `policy::decide` → on `Deny` emit `denied` + audit + return an error telling the model to stop; on `Confirm` emit `awaiting` + block on `request_confirmation`; on approval (or `Allow`) emit `running`, run the handler, emit `ok`/`error`, audit. **Prompt-injection taint guard:** `begin_turn()` (called from `Brain::ask`) resets a per-turn flag; any tool that reads untrusted content (`get_browser_context`, `get_clipboard`, `see_screen`, `read_screen_elements`, `locate_on_screen`, `read_file`) sets it; once tainted, the policy force-confirms subsequent mutations even in `trusted` mode — on-screen/web text is DATA, never instructions. `policy::describe` produces the one-liner shared by the spoken prompt and the activity feed (and deliberately does NOT echo `type_text` content, since the prompt is spoken aloud and could leak a password).

### State machine and conversation mode

`src-tauri/src/state.rs` holds the single `AppState`. Five-state machine: `Idle → Listening → Thinking → Speaking → Idle` (with `Error`). The `session: AtomicBool` flag is **conversation mode**: when true, after a turn finishes Jarvis re-arms the mic automatically — no need to re-say "Jarvis". Set by wake word or first `⇧⌘J`, cleared by `⇧⌘S` / `⇧⌘C` / toggling off mid-Listening. Use `end_turn` (not a bare `set(Idle)`) whenever a turn dead-ends — it ends the session first, otherwise the wake watcher sees "session active" as "mic busy" and refuses to re-arm, wedging the user.

**Conversation-lock contract (important):** `conversation: Mutex<Vec<Message>>` is held for the ENTIRE brain turn — `brain.ask` runs up to ~30s of network *plus* up to the confirmation timeout (default 45s) if a tool needs approval. So: code on the brain thread (providers, tool handlers, the gate) must NEVER re-acquire it (self-deadlock), and other threads (hotkeys, audio callback) must `try_lock` (see `reset_context`) so they never stall behind a turn. `stop()` calls `gate::cancel_all()` to unblock a brain thread parked on a confirmation.

State transitions emit Tauri events that the frontend listens to in `src/shared/state.ts`; the orb's animation (`src/shared/OrbScene.tsx` — custom GLSL via Three.js + postprocessing) responds to those. Events: `jarvis:state`, `jarvis:level`, `jarvis:transcript`, `jarvis:response`, `jarvis:brightness`, plus the computer-control ones — `jarvis:activity` (feed rows, merged by `id`, status `awaiting|running|ok|error|denied`), `jarvis:confirm` (`{id, summary, danger}` → shows the prompt), `jarvis:confirm_resolved` (`{id, approved}` → clears it). Frontend→backend commands: `jarvis_resolve_confirm {id, approved}` and `jarvis_policy_mode` (read-only, shown as the HUD MODE chip). The control UI is `src/shared/ActivityFeed.tsx` + `ConfirmationPrompt.tsx` (HUD variant has clickable Approve/Deny; the orb variant is click-through and relies on the ⌘⇧Y / ⌘⇧S hotkeys), styled in `confirm.css`, danger-color-coded (cyan/amber/red).

**Hotkeys** (registered in `lib.rs`, all `⌘⇧`): `J` toggle on/off, `S` stop (also deny-all confirmations), `H` toggle HUD, `C` close everything, `R` reset conversation, `Y` approve pending action, `+`/`-` orb brightness, `1`–`4` orb corner presets, `P` dev state-cycle preview.

### macOS-specific permissions

Mic is requested via standard Tauri/AVAudio. **Accessibility** is now load-bearing, not optional: both enigo input (clicking/typing) and the AX tree reader need it (`AXIsProcessTrusted`), the same TCC bucket. Input Monitoring (for global hotkeys) and Screen Recording (for `see_screen` / `locate_on_screen`) are also TCC-gated; `src-tauri/src/permissions.rs` (`warmup()` fires the programmatic prompts ~800ms after launch) and `grant-permissions.sh` handle prompting / re-prompting. Each rebuild of the dev binary may re-trigger TCC because the code-signed identity changes — for stable permission state, `pnpm install:app` once and use the bundled `~/Applications/Jarvis.app`. There's an optional `JARVIS_SELFTEST_INPUT=1` that exercises the enigo path from a background thread to verify the main-thread TSM fix still holds.

`signingIdentity: "Jarvis Code Signing"` in `tauri.conf.json` refers to a local self-signed keychain identity, not an Apple Developer cert. Don't change it without coordinating — TCC keys off the signing identity.

## Conventions worth knowing

- Tracing via the `tracing` crate (no `println!` in Rust paths) — `tracing_subscriber` is wired up at startup.
- Errors propagated with `anyhow::Result`; user-visible error text is curated separately (see lib.rs "non-technical error messages" — logs get the gory detail, the user gets short prose).
- No `unwrap()` in hot paths — TTS / mic / brain failures must degrade gracefully because they happen on a user-facing voice loop.
- The frontend has no build-time type checking step wired into `tauri dev`; `tsc` is only consulted by editor tooling. If you change shared types, build will not catch drift — exercise both windows manually.
