# HANDOFF — next session starts here

> As of `main` @ a clean, fully-committed checkpoint (2026-06-22). Read the
> auto-loaded memory notes first (`MEMORY.md` index → especially
> `jarvis-subscription-brain`, `jarvis-3-dev-orchestrator`,
> `jarvis-protected-paths-rule`, `jarvis-no-coauthor-commits`).

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

## ⏭️ NEXT — P3: closed-loop visual QA (IN PROGRESS — spike DONE, P3a next)
**Read [`docs/P3-plan.md`](docs/P3-plan.md)** (the tailored, phased plan) backed by
[`docs/P3-research.md`](docs/P3-research.md) (4 web-research reports: visual-QA pipeline,
computer-use reliability, building blocks, voice-UX landscape). TL;DR:
- **QA loop lives in the Python sidecar** (`orchestrator/qa.py`, NEW — exists now); Rust stays voice/gate/UI.
- **Builder** = dev agent on the Claude subscription; **Judge** = direct **Gemini** multimodal call
  from Python (free, off-subscription — it runs every iteration). Rubric-based structured verdict.
- Phases: ~~spike~~ ✅ → **P3a** one-shot `qa_web_app` tool (serve→screenshot→judge) → **P3b** the loop
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

### P3a — the immediate next step (free: Gemini judge + tool wiring, no agent spawn)
1. **`judge(spec, shots, console_errors) -> {verdict, scores, fixes[]}`** in `qa.py` — ONE Gemini
   multimodal REST call (`GEMINI_API_KEY` is set in `.env`), both screenshots in one request.
   Mirror `src-tauri/src/brain/vision.rs`'s Gemini path. Anti-hallucination contract: force a rubric
   (layout/typography/color/responsiveness/spec-adherence 1–5), `PASS|NEEDS_WORK|FAIL` BEFORE prose,
   numbered fix list, spec text in the prompt. No numeric scores = failed judge → retry stricter.
2. **`qa_web_app(project_dir_or_agent_id, spec)`** — add to `orchestrator/server.py` (FastMCP `@mcp.tool`)
   calling a new `manager.py` method that resolves an agent_id→its worktree (or takes a path), then
   `with serve_app(wt) as url: capture()→judge()`. Returns spoken-friendly verdict + top issues + shot path.
3. **Register `qa_web_app` in `src-tauri/src/tools.rs`** `registry()` with `DangerLevel::ReadOnly`
   (it only serves+reads); `mcp.rs` already forwards external MCP tools by name. Then `cargo check`.
- **Done when:** "Jarvis, check the landing page against my spec" → spoken pass/fail + top issues,
  screenshot path in the activity feed. Test the judge for FREE against the existing landing-page shots.
- **Then handoff before P3b** (P3b spawns dev agents = subscription cost; ASK the user first).

## Then (backlog, prioritized)
- **P4 — structured actuation:** adopt **Playwright MCP** (spawn like the calendar MCP); reorder
  the system prompt to a waterfall **AX tree → Playwright MCP → OmniParser-grounded vision → raw
  `locate_on_screen` (last resort)** — retires the pixel-clicking weak spot. Wire `can_use_tool`→gate.
- Brain polish: persistent `ClaudeSDKClient` session (cut per-turn latency), streaming TTS, ⌘⇧S kills the `claude` subprocess.
- Plan-confirm-execute voice ritual + codebase-aware STT (resolve "the auth modal" → real file).
- Windows port = swap `control/` only (architecture already portable).

## Tip
Tail two logs while testing: `/tmp/jarvis-dev.log` (brain/state) and `~/claude/jarvis-workspace/orchestrator.log` (dev-agent activity).
