# HANDOFF — next session starts here

> **As of 2026-06-30 — voice I/O overhaul on `main` (UNCOMMITTED).**
> The most recent work is the "## 🎙️ Voice I/O overhaul" section directly below — start there.
> Read the auto-loaded memory notes first (`MEMORY.md` index → especially
> `jarvis-voice-io-overhaul` (newest, covers this work), `jarvis-browser-control-playwright`,
> `jarvis-subscription-brain`, `jarvis-3-dev-orchestrator`, `jarvis-qa-judge-on-claude`,
> `jarvis-protected-paths-rule`, `jarvis-no-coauthor-commits`).
> (Browser control + P2/P3/P3b sections lower down are older history — all DONE.)

## 🎙️ Voice I/O overhaul — clean TTS text, better STT, Kokoro neural voice (2026-06-30, UNCOMMITTED)

The day-to-day "Jarvis mishears me / mispronounces / sounds robotic" friction. Three phases, all LOCAL/offline (no egress), Windows-portable. Full detail + gotchas in memory **`jarvis-voice-io-overhaul`**.

- **Phase 1 — TTS text normalization (`src-tauri/src/speakable.rs`, NEW):** pure `to_speech()` called inside `tts::start_speaking` (single choke point; HUD keeps raw Markdown). Strips markdown/emoji/fenced-code, turns links→label, bare URLs→spoken host ("github dot com"), `snake_case`→words. 11 tests. ✅ shipped + installed.
- **Phase 2 — Whisper STT (`src-tauri/src/stt.rs`):** model `base.en`→**`large-v3-turbo-q5_0`** (574MB, in models/, env `JARVIS_WHISPER_MODEL`, auto-picks strongest present best-first), **`initial_prompt` name-biasing** (`JARVIS_STT_VOCAB`), greedy→**beam** (`JARVIS_WHISPER_BEAM`). ✅ shipped + installed.
- **Phase 3 — Kokoro neural TTS (`src-tauri/src/kokoro.rs` + `kokoro_vocab.rs`, NEW; replaces `say`):** `models/kokoro-v1.0.onnx` via **`ort` 2.x** + **`misaki-rs`** G2P (pure-Rust, no espeak install). Default voice **`bm_lewis`** (British male, user-picked). `tts.rs::speak_kokoro` writes a STEREO 24kHz WAV → `afplay` as the same killable `SpeechHandle`; `say` is the fallback (`JARVIS_TTS=say`, or auto if models/init absent → strictly additive). Engine behind `OnceLock<Option<Mutex<Engine>>>`, `prewarm()` in lib.rs. Code-complete, `cargo check` clean, build+install for in-app validation in progress.

**Key gotchas (also in memory):** Kokoro input tensor is named `tokens` not `input_ids` (read names off the model); `voices-v1.0.bin` is an NPZ (`[510,1,256]` per voice, index by token count); VOCAB = 114 symbols from Kokoro `config.json` (pull programmatically — LLM WebFetch mangles the IPA); mono WAV → afplay plays LEFT speaker only, must write stereo; ort `run` takes `&mut`, use `Tensor::from_array((shape,Vec))`. **Deps added** to src-tauri/Cargo.toml: ort, misaki-rs, ndarray, ndarray-npy. Env: `JARVIS_TTS`/`_TTS_VOICE`/`_TTS_SPEED`/`_WHISPER_MODEL`/`_STT_VOCAB`/`_WHISPER_BEAM` (all in `.env`). **De-risked in a scratch crate first** (scratchpad/ort-smoke). **Commit when the user asks** (no `Co-Authored-By`).

## 🌐 Browser control via Playwright MCP — accurate web actuation (2026-06-29, UNCOMMITTED)

This is the backlog "**P4 — structured actuation: adopt Playwright MCP … retires the pixel-clicking weak spot**" item (see bottom), now largely built. Memory note: **`jarvis-browser-control-playwright`** (read it — it has the full architecture + the Comet wall).

**Why:** OS-level web clicking (macOS AX tree + `locate_on_screen` vision) was inaccurate (clicked *right of* YouTube's Shorts icon) AND the brain then **lied** "done" off an unverified click. Fix = drive a real browser through Playwright's DOM (accurate + verifiable).

**Architecture (mirrors the calendar MCP — same `rmcp`/`TokioChildProcess` stdio pattern):**
- `mcp.rs`: `playwright: OnceLock<RunningService>` + `init_playwright(profile_dir)` spawns `npx -y @playwright/mcp@latest --output-dir <app_data>/playwright-output` + (mode-dependent flags). `playwright_call(tool,args)` (45s timeout), `playwright_ready()`.
- **Mode via env `JARVIS_BROWSER_MODE`** (read in `init_playwright`): `extension` → `--extension` (drive the user's REAL browser tabs); anything else / unset → **dedicated** Chrome (`--browser chrome --user-data-dir <app_data>/playwright-profile`). Optional `PLAYWRIGHT_MCP_EXTENSION_TOKEN` (inherited by the child) skips the connect dialog.
- `lib.rs` setup: `init_playwright` on a **background thread** (first `npx` may download).
- `tools.rs`: **10 curated `browser_*` Tools** in `registry()` — navigate, navigate_back, snapshot, click, type, press_key, select_option, hover, wait_for, tabs — handlers → `playwright_call`; declarations copied from the server's real inputSchemas (snapshot→`ref`→click-by-ref). `browser_snapshot` added to `is_untrusted_reader` (taint). `policy.rs::describe` has browser_* cases (doesn't echo typed text).
- `brain/mod.rs` system prompt **strongly rewritten**: "if `get_focused_app` returns a browser → use `browser_*` ONLY; HARD-ban `click`/`scroll`/`click_element`/`read_screen_elements`/`locate_on_screen`/`see_screen` on web pages; NEVER claim 'done' without a fresh verifying `browser_snapshot`." Rule 2 routes *acting* to browser_*; the "what I'm looking at → OS-level" loophole was removed (native-apps only).
- Gating is automatic — every tool flows through `tools::dispatch` → policy gate → audit → activity feed, and the claude-code brain sees them via `mcp_server.rs` (lists `tools::registry()`).

**STATE (2026-06-29):**
- ✅ **Dedicated-Chrome mode WORKS** — clicks accurately by DOM ref + verifies (proven: Shorts/Like/Subscriptions dead-on). `.env` is set to `JARVIS_BROWSER_MODE=dedicated` (the working mode).
- ✅ **The two bugs are FIXED & validated** — brain uses `browser_snapshot` (not vision) and reports honestly (it said "extension isn't connected" instead of faking success). The 3 sibling fixes also landed: AGENTS-panel flicker (`AgentsPanel.tsx` `<For>`→`<Index>`), verify-before-claim, AX-first.
- ⛔ **BLOCKED: driving the user's REAL Comet browser.** `@playwright/mcp --extension` is Chrome/Edge-specific — its preflight checks Chrome's profile and won't connect to the extension that IS installed in Comet ("isn't installed in your Chrome"; microsoft/playwright-mcp **#1577**). The Comet status page showed "No clients connected" the whole time. CDP-attach alternative is blocked by the **Chromium-136** default-profile rule.
- The official extension is legit: CWS "Playwright Extension" id `mmlmfjhmonkocbjadbfplnigmagldckm`, Developer **Microsoft / Playwright Team** (source in `microsoft/playwright` repo `packages/extension`).

**If revisiting the REAL-browser goal:** (a) use **Chrome** instead of Comet (extension works there); (b) **CDP→Comet** (launch Comet `--remote-debugging-port`, `--cdp-endpoint` — gamble vs Chrome-136); (c) tighten **dedicated**-mode UX (it's a separate window). Flipping modes = one `.env` line + relaunch, **no rebuild**.

**Uncommitted as of pause:** 6 source files (`AgentsPanel.tsx`, `lib.rs`, `mcp.rs`, `policy.rs`, `tools.rs`, `brain/mod.rs`) + `.gitignore` (added `.playwright-mcp/`). `.env` is gitignored. **Commit when the user asks** (no `Co-Authored-By`). `cargo check` clean; built+installed via `pnpm install:app`; app last left running in dedicated mode.

## ⛔ Two hard rules (already enforced — don't regress)
1. **protected paths**: never touch any path matching the `JARVIS_PROTECTED_PATHS` markers — hard-denied in `policy.rs::protected_path_guard` (every mode) + `orchestrator/manager.py::_assert_not_protected`. See `jarvis-protected-paths-rule`.
2. **No `Co-Authored-By`** trailer on commits in this repo.

## What works now (all on `main`, live-tested)
- **Brain runs on the Claude subscription** (no API key): Jarvis hosts its tools as a localhost MCP server (`src-tauri/src/mcp_server.rs`); the brain is a pluggable agent-driver (`src-tauri/src/brain/agent.rs`) running `claude -p` against it. Agent-agnostic (Codex/Kimi = future drivers). Primary in `LLM_PROVIDERS=claude-code,...`.
- **Dev-agent orchestrator** (`orchestrator/`, Python sidecar): "build me a website" → spawns an autonomous Claude Code agent; new projects created+committed under `~/claude/jarvis-workspace/projects/<name>` (personal git identity); existing repos get worktrees. HUD **AGENTS** panel + tailable `~/claude/jarvis-workspace/orchestrator.log`.
- Fixes shipped: mic-deaf-while-speaking (no self-listening), effort=low + anti-loop, rmcp `RunningService` kept alive (transport fix), FastMCP list-shape parse (panel fix).

## Run / test
- `pnpm install:app` (release build, ~3-5 min) → installs `~/Applications/Jarvis.app` (stable TCC). Launch from a terminal to capture logs: `RUST_LOG="warn,jarvis_lib=info" ~/Applications/Jarvis.app/Contents/MacOS/jarvis > /tmp/jarvis-dev.log 2>&1 &`
- `cargo check --manifest-path src-tauri/Cargo.toml` (fast); `cargo test --manifest-path src-tauri/Cargo.toml policy` (gate spec incl. the protected-paths test).
- Sidecar (Python) changes take effect on app **relaunch** — no Rust rebuild.
- ~~Caveat: a Finder/GUI launch lacks `~/.local/bin` on PATH~~ **FIXED this session**: `augment_path_for_gui_launch()` (lib.rs, first thing in `run()`) prepends `~/.local/bin`, `/opt/homebrew/bin`, etc. so a Dock/Finder launch finds `claude` + `npx` too. Launching from a shell is now only needed to *capture logs*, not for correctness.

## ✅ P2 DONE + LIVE-TESTED (this session, 2026-06-22)
Both slices implemented behind the unchanged tool surface and **verified in the running app**: a real dev agent built a landing page, Jarvis SPOKE "the landing-page-builder agent finished its turn" only once idle, the roster survived a real quit+relaunch (AGENTS panel showed it detached). Tests: `cargo test` = 19/19 (incl. protected-paths, 5 `proactive_tests`, 4 `path_tests`); `orchestrator/test_persistence.py` = 16/16 (free); `orchestrator/test_persistence_e2e.py` = 13/13 (one real ~$0.20 agent spawn).

**Also fixed (bonus, this session): the Finder/Dock-launch PATH bug.** A Dock launch gets `PATH=/usr/bin` from launchd → `claude` unreachable → subscription brain silently dropped → "Something went sideways". `augment_path_for_gui_launch()` in `lib.rs` (first line of `run()`, before brain/MCP init) prepends the real install dirs; pure core `compute_path_augmentation()` is unit-tested. Verified by launching the bundle with `PATH=/usr/bin` → it self-repairs and detects `claude-code`. This was pre-existing, not introduced by P2.

1. **Persistence** — `orchestrator/manager.py`. JSON roster at `~/claude/jarvis-workspace/agents-registry.json` (override `JARVIS_AGENTS_REGISTRY`), written on spawn + every status change (via a `_set_status` choke point that persists only on a real transition — no disk write per assistant message), reloaded in `AgentManager.__init__`. `list()` merges live agents over detached history (newest-first); `status()` serves detached agents too. Restored records get `live:false`, transient `spawning/working` downgraded to `idle`, `project_dir`/`is_worktree` preserved. Corrupt/missing registry → empty roster, never crashes. `Agent.summary()` now also carries `created` + `live`. **Python-only → takes effect on app relaunch, no rebuild.**
2. **Proactive voice** — Rust. `lib.rs`: the 2s poll thread now also captures `app_state` + the app handle, keeps a `prev`-status map + a small (`MAX_QUEUED=5`) local queue, and calls `diff_agent_updates()` each poll. It announces only LIVE agents' working→idle ("finished its turn") / →error ("ran into an error"); the first poll seeds silently; spawning/working starts and detached agents stay silent. Drains ≤1 update per poll via `AppState::try_announce` (state.rs), which **atomically** claims Idle→Speaking under the state write-lock (gates on Idle + no session + `gate::has_pending()`), suppresses the mic, speaks via `tts::start_speaking`, stores the handle (so ⌘⇧S/⌘⇧J kill it), then returns to Idle only if still Speaking. Never touches the conversation lock. **Rust → needs `pnpm install:app` to ship.**

### What still needs a LIVE run (the user's manual step)
- `pnpm install:app`, launch from a shell (PATH caveat below), spawn a real dev agent, let it finish a turn → confirm Jarvis SPEAKS the update only when idle (not mid-conversation), and that ⌘⇧S/⌘⇧J interrupt it.
- Restart the app with a past agent in the registry → confirm the AGENTS panel still lists it (detached) and "how's the agent doing?" answers from history.
- Watch races: trigger a wake word / ⌘⇧J at the exact moment an announcement starts — a user turn must always win.

## P3 — closed-loop visual QA (spike + P3a DONE; **P3b is the NEXT step, see below ↓**)
**Read [`docs/P3-plan.md`](docs/P3-plan.md)** (the tailored, phased plan) backed by
[`docs/P3-research.md`](docs/P3-research.md) (4 web-research reports: visual-QA pipeline,
computer-use reliability, building blocks, voice-UX landscape). TL;DR:
- **QA loop lives in the Python sidecar** (`orchestrator/qa.py`, NEW — exists now); Rust stays voice/gate/UI.
- **Builder** = dev agent on the Claude subscription; **Judge** = **Claude (sonnet)** via `claude-agent-sdk`,
  on the SAME Claude subscription (user decision 2026-06-22 — overrides the original "Gemini, free,
  off-subscription" plan; they want one consistent tool, accept the per-iteration sub cost). Rubric-based
  structured verdict. ⚠️ cost: judge runs every iteration on the shared usage window — keep the loop's
  `max_iters` tight in P3b.
- Phases: ~~spike~~ ✅ → ~~**P3a** one-shot `qa_web_app` tool (serve→screenshot→judge)~~ ✅ → **P3b** the loop
  (judge fails → `instruct_dev_agent` → re-serve → re-judge; stop conditions; **proactive voice
  narrates verdicts**, riding the P2 announcer) → **P3c** add build/tsc/lint/test gates.
- Defer `can_use_tool`→gate per-action gating to P4 (P3 keeps gating at spawn time).

### ✅ P3 spike DONE (2026-06-22, zero subscription cost)
`orchestrator/qa.py` ships working `serve_app()` + `capture()`, validated against the on-disk
`landing-page-builder` worktree (no agent spawn). Deps added: `playwright==1.60.0` + the bundled
`chrome-headless-shell` (a HEADLESS, isolated-profile binary — **not** the user's Google Chrome;
⚠️ user rule: ask before launching ANY browser, even headless — it's fine because invisible, but ask).
- `serve_app(worktree)` — ephemeral port (`socket.bind(:0)`), static (root/dist/build/out/public)
  + node-build detection, context-managed teardown. Tested via `uv run python qa.py <worktree>`.
- `capture(url, spec)` — Playwright headless, mobile(375)+desktop(1280) full-page shots; console +
  pageerror listeners attached BEFORE goto; returns `{shots[], console_errors[], page_errors[], a11y[], out_dir}`.
- **⚠️ Non-obvious gotcha found + fixed:** scroll-reveal content (`opacity:0 → .is-visible` via
  `IntersectionObserver`) is INVISIBLE in a naive headless full-page screenshot → a judge would
  false-flag "missing content". A scroll-pass to trigger it is RACY (proved: desktop `0/3`, mobile
  `1/3` reveals). Fix in `qa.py`: `_QA_INIT_SCRIPT` overrides `IntersectionObserver` to report
  intersecting immediately (deterministic "fully-scrolled" state) + `_REDUCE_MOTION_CSS` for crisp
  capture. Now `3/3` every run. **Keep this when extending capture()** — any reveal-on-scroll app hits it.

### ✅ P3a DONE + LIVE-VERIFIED (2026-06-22) — judge runs on **Claude**, not Gemini
All three slices shipped behind the unchanged tool surface; the Claude judge was live-tested against
the on-disk `/tmp/jarvis-qa-fixed` shots (one sonnet call each for a matching spec → PASS and a
deliberately-mismatched dashboard spec → FAIL, spec_adherence=1; it quoted the real hero copy, proving
it grounds in the pixels). **Working tree is uncommitted — commit when the user asks.**
1. **`judge(spec, shots, console_errors, page_errors) -> {verdict, scores, fixes[], summary, model, raw}`**
   in `qa.py` — a ONE-SHOT, **tool-less** `claude-agent-sdk` `query()` (model `sonnet`, override
   `JARVIS_QA_JUDGE_MODEL`). Both screenshots go in as **inline image blocks**; `allowed_tools=[]` +
   `setting_sources=[]` keep it sandboxed (no Read/file/web). Anti-hallucination contract kept: rubric
   (layout/typography/color/responsiveness/spec_adherence 1–5), `PASS|NEEDS_WORK|FAIL`, numbered fixes,
   spec in the prompt; no numeric scores = failed judge → one stricter retry, else raise.
   ⚠️ **GOTCHA (cost us a failed run):** Claude Code wants the **Anthropic-native image block** —
   `{"type":"image","source":{"type":"base64","media_type":"image/png","data":…}}` — NOT the flat
   `{data,mimeType}` form some SDK docs show (that fails inside Claude Code with
   `undefined is not an object (evaluating 'e.source.type')`). See `_image_blocks()`.
2. **`qa_web_app(target, spec)`** — `server.py` FastMCP tool → `manager.qa_web_app()`, which resolves
   `target` (agent_id → its worktree, live OR detached; else a path; protected-paths guard) then
   `with serve_app(wt) as url: capture()→judge()`. Returns `{verdict, scores, spoken, top_issues, fixes,
   screenshots, shot_dir, console_errors, …}` — `spoken` is the TTS one-liner.
3. **`qa_web_app` registered in `tools.rs`** as `DangerLevel::ReadOnly`; `mcp.rs` forwards by name and
   gives this one tool a **180s** timeout (serve+2 captures+judge is heavier than a spawn). `cargo check` clean.
- **✅ LIVE in-app run DONE (2026-06-23):** rebuilt + installed, launched, said "check the landing-page-builder
  agent's work against this spec…" → brain called `qa_web_app(27586549)` → pipeline ran end-to-end
  (serve→capture→Claude judge, ~18s) → Jarvis SPOKE the rubric verdict ("NEEDS_WORK… spec_adherence 3").
- **⚠️ Bug found + fixed in-app (commit `0640d9d`):** the FIRST in-app try, the brain **bypassed `qa_web_app`**
  and improvised (`list_dev_agents` + `run_shell` cat/ls/open in the user's real browser + `see_screen`). Root
  cause: no dev-agent/QA section in the system prompt + Rule 1 pulling "this page" toward screen inspection.
  Fix: `brain/mod.rs` Rule 7a mandates `qa_web_app` for QA-ing a built web app + forbids the cat/open/see_screen
  workarounds; `manager.py` resolver now also accepts an agent NAME. Re-tested → routes correctly. **This is the
  load-bearing lesson for P3b: the loop must be SIDECAR-DRIVEN, not brain-driven.**

## ✅ P3b DONE + voice-validated (2026-06-23) — the closed self-correcting QA loop
The hero loop shipped: spoke a command → one dev-agent spawn → loop WAITs for the build → pass-1 PASS → spoken verdict. Lives in `manager.py::_qa_fix_loop`/`start_qa_loop`; brain steer is Rule 7b (`brain/mod.rs`); voice narration via `announce`/`announce_seq`+`qa_active` in `lib.rs::diff_agent_updates`. Two bugs found+fixed in testing: `c9ba825` (LLM_PROVIDERS made an allowlist, stops a timed-out primary from falling over to gemini and re-spawning a duplicate agent) and `b1d9a37` (loop waits for the in-flight build before the first QA). See memory `jarvis-qa-judge-on-claude`. (Original plan below for reference.)
**Read [`docs/P3b-plan.md`](docs/P3b-plan.md)** — a full, staged, cost-disciplined plan (the design is worked
out: sidecar-driven loop, `turn_done` Event to await agent turns, `announce`/`announce_seq` → proactive voice,
stop conditions, the Python-first/Rust-last staging, free unit tests before any agent spawn). The user APPROVED
P3b knowing it spawns/instructs dev agents = **Claude subscription cost**; still ASK before the first real
agent-spawn validation (Stage 2). Base for the fresh session: `main` @ `0640d9d` (clean).

## Then (backlog, prioritized)
- **P4 — structured actuation:** ✅ *largely DONE 2026-06-29 (see "🌐 Browser control" up top, uncommitted)* — Playwright MCP is spawned like the calendar MCP, 10 `browser_*` tools registered + gated, system prompt rewritten so web pages route to `browser_*` (DOM) not pixel-clicking. **Still open:** driving the user's REAL browser (Comet blocked by #1577 / Chrome-136 — works in dedicated Chrome); per-action `can_use_tool`→gate; optional OmniParser-grounded vision for the *native-app* fallback path.
- Brain polish: persistent `ClaudeSDKClient` session (cut per-turn latency), streaming TTS, ⌘⇧S kills the `claude` subprocess.
- Plan-confirm-execute voice ritual + codebase-aware STT (resolve "the auth modal" → real file).
- Windows port = swap `control/` only (architecture already portable).

## Tip
Tail two logs while testing: `/tmp/jarvis-dev.log` (brain/state) and `~/claude/jarvis-workspace/orchestrator.log` (dev-agent activity).
