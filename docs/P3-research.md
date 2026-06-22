# P3 research dossier — "voice pilot for the dev PC"

> Captured 2026-06-22 from parallel research agents (web-sourced). 3 of 4 reports
> completed; the 4th (voice-dev competitive UX landscape) was interrupted by the
> Claude usage limit and is still owed. Preserved verbatim-ish so the cost
> is never paid twice. Links are as-cited by the agents — verify before relying.

## Convergent conclusions (agreed across multiple reports)
1. **The QA loop lives in the Python sidecar, not Rust** (reports 1 and 4 independently). Rust stays the auth/voice/UI layer.
2. **Lean hard AWAY from pixel-clicking.** Reliability waterfall: structured APIs/CLI → browser DOM (Playwright a11y tree) → macOS AX tree → vision+grounding (OmniParser/Set-of-Mark) → raw pixel coords (last resort = our current `locate_on_screen`).
3. **Judge ≠ builder.** A separate vision "critic" scoring against a rubric beats single-pass self-review; ~3–5 iterations, keep best-scoring, with hard stop conditions.
4. **Reuse, don't rebuild:** Playwright (Python) + Playwright MCP + `claude-agent-sdk` `can_use_tool`/hooks/`create_sdk_mcp_server`, bridged into the existing `gate.rs`/`policy.rs`.

---

## Report 1 — Closed-loop visual QA pipeline

**Serve + screenshot:** Playwright (Python) is the clear winner. Pattern: boot a local server (`python -m http.server` for static, or `vite preview`/`pnpm dev`), Playwright headless Chromium → `wait_for_load_state("networkidle")` (+ ~500ms or wait for a `data-qa="loaded"` sentinel for SPAs) → `page.screenshot(full_page=True)` at breakpoints (375 mobile, 1280 desktop). Capture console errors via `page.on("console")` + `page.on("pageerror")` listeners attached *before* navigation.

**Loop shape (research-validated "Critic-in-the-Loop"):** Generate → Render → *Visual* critique (VLM on pixels) → *Code* critique (LLM on diff) → Improve → Evaluate → keep best-of-N. Separating visual from code critique beat single-pass; ~3 cycles ≈ +18% quality. Stop = fixed budget (3–5) + keep highest-scoring, not necessarily last.

**Pixel-diff vs LLM-judge:** pixel-diff (odiff/pixelmatch/Lost Pixel) is for *regression* ("did it change?"), useless for *spec adherence*. Vision LLMs are weak pixel comparators but strong **semantic judges** — use LLM-judge for "does it match the brief," optionally odiff as a cheap "did anything change" gate.

**Judge prompting (avoid hallucinated praise):** force a **rubric** (layout/typography/color/responsiveness/spec-adherence, 1–5 each); require `PASS`/`FAIL`/`NEEDS_WORK` verdict *before* prose; demand a numbered **fix list**; include the spec text in the prompt. (arxiv MLLM-as-UI-Judge: Claude 3.5+ >75% factor agreement w/ humans.)

**Recommended pipeline (Python sidecar):** dev-agent builds → sidecar boots server on free port → Playwright navigates, captures console errors + 2 screenshots → base64 → judge (vision) returns `{scores, verdict, fix_list[]}` → PASS or max-iter? done : feed fix_list back to agent (resume session) → repeat → Rust receives final screenshot+verdict via Tauri event, speaks result.

**Pitfalls:** `networkidle` insufficient for SPAs; hallucinated praise (#1 judge failure — enforce numeric scores); port collisions across parallel agents (bind :0, read assigned port); `vite preview` needs a build first (silent stale-dist failure); don't use pixel-diff as a convergence signal; per-iteration `claude` CLI cold-start 2–5s (batch both screenshots into one judge call).

**Tools:** Playwright Python (`playwright.dev/python`, prod) · `@playwright/mcp` (a11y-tree based, stable 2025) · odiff (fast pixel diff) · Lost Pixel (OSS VRT) · CITL ref arxiv 2604.05839 · MLLM-UI-Judge arxiv 2510.08783.

---

## Report 2 — Computer-use / GUI reliability (the strategic direction)

**Reliability hierarchy (most→least), with when to use:**
1. **Structured APIs / CLI / filesystem** — shell, git, file I/O, REST. Deterministic. Always prefer for dev tasks.
2. **Browser DOM via Playwright a11y tree** — 2–5KB structured vs 150K+ tokens for screenshots; 10–100× faster; role/name selectors survive layout changes. browser-use hits 89.1% WebVoyager this way.
3. **macOS AX API (AXUIElement)** — reliable for Cocoa/AppKit/SwiftUI. Fails on Qt/Electron/OpenGL/games (no AX tree). Run on main thread (Jarvis already does).
4. **Vision + Set-of-Mark / OmniParser grounding** — overlay labeled boxes so the model picks a region not raw (x,y); ~3–5× more reliable than blind coords. OmniParser v2 = standalone Python service, annotates the image *before* Claude sees it (compatible with our Claude vision). Worth adding for AX-less surfaces.
5. **Raw pixel-click (vision→coords)** — least reliable; SOTA tops ~38% OSWorld vs humans 72%+. = our current `locate_on_screen` weak spot.

**SOTA (reliability):** Agent S3 72.6% OSWorld (Oct 2025, first to beat human) · OpenAI CUA/Operator 38% OSWorld / 87% WebVoyager · Claude Computer Use ~28% OSWorld · browser-use 89.1% WebVoyager (best web) · UI-TARS ~38% · Google Project Mariner **killed May 2026** (industry pivoting to API/AX-first) · OmniParser v2 = grounding layer, not an agent.

**Recommendation:** Replace `locate_on_screen → raw coords` with a **waterfall: AX tree → OmniParser-grounded vision → raw vision**. Wire in **Playwright MCP for all web surfaces** (eliminates screenshot loops). For dev-orchestrator work, **never touch the GUI** — shell/git/file/LSP only. Insight: at SOTA, ~30–40% of failures are *planning*, not misclicks → more tool structure + verifiable subgoals helps more than better vision.

**macOS do/don't:** AX for Cocoa apps; main-thread AX+input (done); `applescript_escape` (done); cross-check `kAXErrorCannotComplete` against Finder.app; re-read AX refs per turn (cache desyncs); TCC can silently invalidate after OS updates (add startup probe); keep the taint guard on `locate_on_screen` results.

**Sources:** os-world.github.io · Agent S2/S3 arxiv 2504.00906 · OpenAI CUA · browser-use github · microsoft/OmniParser · Playwright-MCP guides.

---

## Report 3 — Reusable building blocks

**claude-agent-sdk features to adopt (exact API on `ClaudeAgentOptions`/`ClaudeSDKClient`):**
- `can_use_tool(tool_name, input, context) -> PermissionResultAllow|Deny` — **the bridge to Jarvis's gate**: sidecar callback forwards risky agent ops to `gate.rs::request_confirmation` over the existing MCP channel, blocks on ⌘⇧Y/voice/HUD, returns Allow/Deny (`Deny(interrupt=True)` hard-stops). `context.agent_id` / `context.blocked_path` ready-made.
- `hooks={"PreToolUse"/"PostToolUse": [HookMatcher(...)]}` — block risky shell pre-exec; capture tool outputs for the QA loop without polling; emit to activity feed.
- `@tool(...)` + `create_sdk_mcp_server(...)` + `mcp_servers={...}` — expose QA tools (screenshot/judge/critique) in-process to the agent, no extra process.
- `resume="<uuid>"`, `fork_session=True`, `session_id` — persist/branch agent context across QA iterations.
- `model=` per agent (cheap builder, stronger judge); `permission_mode` per-worktree; `setting_sources` for isolation.

**Adopt vs build:**
| Item | Verdict | Note |
|---|---|---|
| `playwright` (Python) | **Adopt** | QA screenshotter; console/network/a11y |
| `axe-playwright-python` | **Adopt** | one-line a11y violations JSON |
| `@playwright/mcp` (MS) | **Adopt** | agent-driven browser; spawn like calendar MCP |
| `@modelcontextprotocol/server-{filesystem,git,github}` | **Adopt** | official; safer than raw Bash for those ops |
| `ast-grep` | **Adopt** | structural refactors > regex (via Bash) |
| `ripgrep` | already in Claude Code | — |

**Minimal serve/ready/teardown:** `subprocess.Popen` the dev server with `PORT` env on a free port (`socket.bind(('',0))`), poll TCP with `socket.connect_ex` (no sleep), context-manager teardown (`terminate()`+`wait`). Static → `python -m http.server`.

**Architecture (sidecar owns the loop):**
```
Rust/Tauri ── MCP server ── qa_loop tool
                               └─ Python sidecar (orchestrator/)
                                    ├─ ClaudeSDKClient (dev agent, worktree)
                                    │    └─ can_use_tool → Jarvis gate (over MCP)
                                    ├─ serve_app() (subprocess, ephemeral port)
                                    ├─ Playwright (screenshot, console, axe)
                                    └─ vision judge (separate client, screenshot inline)
                                         └─ critique → resume dev agent → loop
```

**Reuse not rebuild:** gate/policy (`gate.rs`/`policy.rs` via `can_use_tool`), MCP stdio spawn pattern (proven w/ calendar), `audit.jsonl` (correlate via `tool_use_id`), `jarvis:activity` HUD events, `brain/vision.rs` 3-provider fallback. **Build only:** `serve_app()`, the loop, and the in-process `@tool` QA tools.

---

## Report 4 — Voice-dev competitive landscape & UX

**What works vs fails (2026):** Voice is production-ready for *prompt dictation* (~150–200 WPM driving an AI), NOT for raw code-symbol dictation (naming `camelCase`/brackets by voice is still painful without phonetic grammars like Talon). The 2026 differentiator is **codebase-aware STT** (Willow resolves "the auth modal" → the actual file/var; plain Whisper won't). Copilot Voice (GA Jun 2026: local STT, push-to-talk) is safe but *passive* — prompt delivery only, not orchestration.

**Orchestration UX — the universal pattern is PLAN → CONFIRM → EXECUTE**, never fire-and-forget. Devin 2.0 = interactive plan approval before execution (Goldman piloted w/ 12k devs). OpenHands = full decision transparency. Fully autonomous agents erode trust; *supervised delegation* builds it. Field is converging on **"human-on-the-loop"**: agents act, but pause/override/rollback are first-class.

**Ambient design:** interrupt threshold is the unsolved problem — too quiet = abandonment, too chatty = noise. Trigger updates on **meaningful state changes**, not a heartbeat timer.

| Product | One-liner | Link |
|---|---|---|
| Wispr Flow | Best-in-class voice dictation, IDE syntax-aware | wisprflow.ai/vibe-coding |
| Willow | 200ms, codebase-indexed voice (knows your file/var names) | willowvoice.com |
| Talon Voice | Full hands-free OS+code control, phonetic precision; steep onboarding | talonvoice.com |
| Copilot Voice | GA Jun 2026: local STT, push-to-talk, passive | githubnext.com/projects/copilot-voice |
| Devin 2.0 | Async SWE agent; interactive plan approval | devin.ai |
| OpenHands | OSS multi-agent orchestration; decision transparency | arxiv 2407.16741 |
| Serenade | Local-first voice-to-code, programming-vocab STT | serenade.ai |

**ADOPT:** (1) **Plan-confirm-execute with a spoken 1-line plan** before an agent starts ("I'll add dark mode in a new branch" → verbal/hotkey confirm). (2) **Codebase-aware voice** — index repo file/function/component names so STT resolves "the auth modal" to the real file. (3) **Roster + spoken ETA** on natural pauses, not timers (we already have proactive updates — gate them on state changes). (4) **Danger-tiered confirmation with matching vocal urgency** (calm=mutating, firm=destructive; reuse cyan/amber/red). (5) **Visual QA as a closing ritual** — "Done; want me to screenshot and compare to your mockup?"

**AVOID:** (1) always-on dictation of code symbols; (2) silent agents >90s (anxiety→distrust) — but no heartbeat spam either; (3) fire-and-forget autonomy (the plan-confirm loop *is* the trust mechanism); (4) treating voice as bolted-on STT/TTS over a text-first UI (voice IS Jarvis's primary UX); (5) leaking credentials via TTS (existing guard — never weaken).

**Differentiation:** every voice-coding tool today is a *prompt-delivery mechanism bolted onto an IDE*. Jarvis is the only one where voice is the *orchestration layer* — speak a request → spawn an isolated-worktree agent → live HUD → interrupt only for decisions → visual QA → verbal report. Closest is Devin, but it's web SaaS with no ambient desktop presence.

**KILLER DEMO:** sit in front of a running app, say *"Add a loading spinner to the submit button."* → agent spawns, HUD shows it live, Jarvis says "done, checking" → screenshots the running app, visually confirms the spinner, says *"Spinner's live. Want me to open a PR?"* Voice in, visual confirmation out, zero keyboard — no competitor can replicate that loop today.

Sources: wisprflow.ai · willowvoice.com · codepick.dev voice-tools · githubnext copilot-voice · devin.ai · arxiv 2407.16741 (OpenHands) · bprigent.com "7 UX patterns for human oversight in ambient AI" · sitepoint Devin realities · aufaitux agentic-AI patterns.
