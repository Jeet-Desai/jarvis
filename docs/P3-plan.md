# P3 Plan — Closed-loop visual QA, and the wings toward "voice pilot for the dev PC"

> Tailored to Jarvis's real architecture. Built on the research in
> [`docs/P3-research.md`](./P3-research.md) and the P1/P2 work already on `main`
> (dev-agent orchestrator + subscription brain + proactive voice + persistence).
> Respects the two hard rules: protected paths; no `Co-Authored-By`.

## 0. The north star (why this is the hero)
The dev pipeline is **understand → plan → build → verify → review → ship → operate.**
Jarvis already owns *build* (autonomous agents in worktrees). P3 = **verify**, the
missing link that turns Jarvis from a *spawner* into a *self-correcting builder*.

**Killer demo (the one thing to nail):** you sit in front of a running app and say
*"Add a loading spinner to the submit button."* → Jarvis speaks a one-line plan,
spawns an agent (HUD shows it live), the agent builds → Jarvis serves it, screenshots
it, a vision critic checks it against your words → if wrong, it critiques the agent and
loops → when right, Jarvis says *"Spinner's live on the button — want me to open a
PR?"* **Voice in, visual confirmation out, zero keyboard.** No competitor (Devin,
Copilot Voice, Wispr/Willow) closes that loop on your desktop today — that's the wedge.

## 1. Architecture decision — where the loop lives
**The QA loop lives in the Python sidecar** (`orchestrator/`), not Rust. Both research
reports converged here, and it fits what exists: the sidecar already owns the agent
lifecycle (persistent `ClaudeSDKClient`, the drain loop, status/cost tracking, the
registry). Rust stays the voice/brain/gate/UI/vision-provider layer.

```
Rust/Tauri ── mcp_server (tools→brain) ──┐
   │  poll thread + proactive announcer   │   spawn_dev_agent(task, qa_spec?)
   │  (P2: diff_agent_updates/try_announce)│        │  qa_web_app(agent_id, spec)
   ▼                                       ▼        ▼
gate.rs / policy.rs            orchestrator/ (Python sidecar)
   ▲ (spawn already gated)        ├─ ClaudeSDKClient  ← the BUILDER (Claude subscription)
   │                             ├─ qa.py controller  ← NEW
   │                             │    ├─ serve_app()   (subprocess, ephemeral port)
   │                             │    ├─ capture()     (Playwright: shots+console+axe)
   │                             │    └─ judge()       (vision critic → structured verdict)
   │                             └─ on pass/fail/stuck → status event
   └──────────── proactive voice announces the verdict (rides P2) ◄┘
```

**Cost-smart split (tailored to "Claude subscription only"):**
- **Builder** = the dev agent on the Claude subscription (expensive, necessary).
- **Judge** = a **direct Gemini multimodal call from Python** (`GEMINI_API_KEY`, already
  in `.env`, free tier), mirroring `brain/vision.rs`'s primary path, with the
  OpenAI-compatible vision model as fallback. **The judge runs every iteration, so
  keeping it off the subscription is the whole game.** Only escalate to a `claude`-CLI
  judge for genuinely hard cases (opt-in).

## 2. Phased build — each phase ships something demoable

### P3a — one-shot QA tool (prove the pipeline; cheap to test)
A new tool `qa_web_app(project_dir_or_agent_id, spec)` that does **serve → screenshot →
judge → return verdict** (no loop yet).
- `orchestrator/qa.py`: `serve_app()` + `capture()` + `judge()` (see §3).
- `server.py`: expose `qa_web_app`; `manager.py`: wire it to a resolved worktree.
- `tools.rs`: register `qa_web_app` (DangerLevel::ReadOnly — it only reads/serves);
  `mcp.rs` already forwards by name.
- **Done when:** "Jarvis, check the landing page against my spec" returns a spoken
  pass/fail + the top issues, and drops the screenshot path in the activity feed.

### P3b — the closed loop (the hero)
Add the controller: judge **fails → `instruct_dev_agent(critique)` → re-serve →
re-judge**, until pass or a stop condition. Triggered by `spawn_dev_agent(..., auto_qa=
true, qa_spec=...)` or a follow-up `qa_web_app`.
- Loop runs as an async task per agent (a sibling of the existing drain task); it awaits
  "turn complete" (the drain loop already flips status→idle on `ResultMessage`; expose
  it as an `asyncio.Event`).
- **Stop conditions (all required):** `max_iters` (default 3, env `JARVIS_QA_MAX_ITERS`);
  **no-progress** (judge's issue-set ~unchanged two iters running → stop); cost/time
  budget; agent signals it can't proceed.
- **Proactive narration rides P2:** each verdict becomes an agent event the poll thread
  already announces. On pass: *"the landing-page agent passed visual QA on pass 2."* On
  stuck: *"I've tried 3 times — the hero still overflows on mobile. Want to take a look?"*
  (Idle-gated by `try_announce`, so it never interrupts you.)
- **Preview:** leave the dev server up on success so *"show me"* → `open http://127.0.0.1:<port>`.
- **Done when:** the killer demo runs hands-free end to end.

### P3c — broaden "verify" beyond pixels (cheap reliability)
Before/alongside the visual judge, run **structured gates** (highest reliability, near-zero
cost): `build` succeeds, then `tsc`/`eslint`/`vitest` if present. A build/type/test failure
is a *deterministic* critique fed back before we even screenshot — catches the 30–40% of
failures that are logic, not looks.
- **Done when:** the loop won't declare visual victory on an app that doesn't compile or
  whose tests fail.

## 3. Component specs (the concrete pieces to build)
- **`serve_app(worktree) -> url`** — detect kind (static `dist/` → `python -m http.server`;
  vite/next → `pnpm dev`/`vite preview`). Bind an ephemeral port (`socket.bind(('',0))`),
  poll TCP readiness with `connect_ex` (no sleep loops), context-manager teardown. **Guard:
  run the build first** (vite preview on a stale/empty `dist/` is a silent failure).
- **`capture(url, spec) -> {shots[], console_errors[], a11y[]}`** — Playwright headless
  Chromium; attach `page.on("console")`/`page.on("pageerror")` **before** `goto`;
  `wait_for_load_state("networkidle")` + a sentinel/`data-qa="loaded"` wait for SPAs;
  screenshot at 2 breakpoints (375 mobile, 1280 desktop); optional `axe-playwright-python`.
- **`judge(spec, shots, console_errors) -> {verdict, scores, fixes[]}`** — one Gemini
  multimodal call, **both screenshots in one request** (latency). Anti-hallucination
  contract (from the research): force a **rubric** (layout / typography / color /
  responsiveness / spec-adherence, 1–5), require `PASS|NEEDS_WORK|FAIL` **before** prose,
  demand a **numbered fix list**, and **put the user's spec text in the prompt**. A judge
  that returns praise with no numeric scores = a failed call → retry with a stricter system
  prompt, don't trust it.
- **Loop controller** — owns the stop conditions, the no-progress detector (diff issue
  sets), and emits status/cost events through the existing `_event`/registry path so the
  HUD + proactive voice + `audit.jsonl` all light up for free.

## 4. Reuse, don't rebuild (it's mostly wiring)
| Need | Reuse | Build |
|---|---|---|
| Agent lifecycle / sessions / cost | `manager.py` (`ClaudeSDKClient`, drain, registry) | — |
| Tool surface + safety gate | `tools.rs` + `mcp.rs` + `gate.rs`/`policy.rs` (spawn already gated) | register `qa_web_app` |
| Spoken updates / HUD / audit | P2 `diff_agent_updates`/`try_announce`, `jarvis:activity`, `audit.jsonl` | emit QA verdicts as events |
| Vision judge | `brain/vision.rs` philosophy (Gemini→OpenAI fallback) | a Python `judge()` (Gemini REST) |
| Browser | **adopt** Playwright (Python) + later **Playwright MCP** | `serve/capture/qa.py` |

## 5. Decisions to confirm (with my recommendation)
1. **Judge vision path** → ~~**Gemini-from-Python** (free, off-subscription)~~ **DECIDED 2026-06-22:
   Claude (sonnet) via `claude-agent-sdk`**, on the Claude subscription. The user chose one consistent
   tool (Claude is now primary) over the free Gemini split, accepting the per-iteration subscription
   cost. Implemented in
   `qa.py::judge` (tool-less one-shot, inline image blocks). Mitigate cost with a tight P3b `max_iters`;
   `JARVIS_QA_JUDGE_MODEL` can raise/lower the model. (In testing, sonnet grounded better than
   gemini-2.5-flash — it quoted real on-page copy.)
2. **P3 v1 scope** → **web apps only** (cleanest serve+DOM story). Native-app QA later.
3. **Budgets** → default `max_iters=3`, a wall-clock cap, and announce when truncated
   (never silently give up).
4. **`can_use_tool` per-action gating** → **defer to P4.** P3 keeps today's model (gate at
   *spawn* time; agent autonomous in its isolated worktree), so P3 isn't blocked on the
   sidecar↔gate round-trip. Prototype that bridge separately.

## 6. The broader wings (post-P3, in priority order)
- **P4 — structured actuation everywhere:** adopt **Playwright MCP** (spawn over stdio like
  the calendar MCP) so the brain drives *real browsers via the DOM*, not pixel-clicks;
  reorder the system prompt to a **waterfall: AX tree → Playwright MCP → OmniParser-grounded
  vision → raw `locate_on_screen` (last resort).** This directly retires our known weak spot.
  Wire `can_use_tool` → `gate.rs` so risky agent actions hit the voice gate.
- **Plan-confirm-execute voice ritual:** before spawning, Jarvis speaks a 1-line plan and
  waits for a verbal/⌘⇧Y confirm (the universal trust pattern from the UX research).
- **Codebase-aware voice:** index the active repo's file/symbol names so STT resolves "the
  auth modal" to the real file (the 2026 differentiator vs plain Whisper).
- **Ship/review:** voice-driven PR/commit/review; persistent `ClaudeSDKClient` brain session
  + streaming TTS to cut latency; ⌘⇧S kills the `claude` child.

## 7. Risks & mitigations
- **SPA "ready" detection** → sentinel element wait, not bare `networkidle`.
- **Port collisions across parallel agents** → ephemeral port per worktree, read it back.
- **Hallucinated praise** → rubric + numeric scores mandatory; no-score = failed judge.
- **Subscription burn** → judge off-subscription (Gemini); hard `max_iters`; budget guard.
- **`vite preview` on unbuilt dist** → build-first guard.
- **Pixel-diff as a convergence signal** → don't; use the LLM verdict (odiff only as a cheap
  "did anything change" gate, optional).

## 8. Definition of done (the pilot bar)
The killer demo runs **hands-free, reliably, three times in a row**: voice request →
agent builds → automatic visual-QA loop self-corrects → Jarvis speaks the verdict and
offers a PR → preview opens on command. Judge cost stays off the Claude subscription;
stop conditions prevent runaway loops; every step shows in the HUD + audit log.

## 9. First session's starting move
Spike `orchestrator/qa.py` `serve_app()` + `capture()` against the **already-built
`landing-page-builder` worktree** (it's on disk from P2 testing — free, no agent spawn):
serve it, screenshot it, print console errors. That validates the riskiest plumbing
(serve + ready-detect + Playwright) for **zero subscription cost** before wiring the judge.
