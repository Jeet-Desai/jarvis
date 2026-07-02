# JARVIS Roadmap — from voice HUD to a boundary-less personal agent

*Written 2026-07-02, after the CORE/AGENTS/MINI HUD redesign shipped. Grounded in a
landscape review of desktop/computer-use agents as of mid-2026 (OpenAI Codex desktop +
background computer use, Claude Cowork/Dispatch/Scheduled Tasks, Gemini agent platform,
Devin Desktop, Agent S3, Screenpipe, Mem0/Letta/Zep, OSWorld ~85% SOTA).*

## Where Jarvis already stands (don't rebuild — double down)

The market converged on things Jarvis already has:

- **Safety gate**: 2026 industry consensus is "constrain consequences, don't try to
  classify every malicious input" — exactly the policy gate + taint tracking + confirm
  flow in `policy.rs`/`gate.rs`. This is ahead of most shipped agents. Keep it load-bearing.
- **Hybrid actuation**: AX-tree-first + DOM (Playwright) + vision fallback is what every
  serious 2026 roundup recommends. Jarvis does all three.
- **Closed verification loops**: the P3b QA-and-fix loop is the same shape as Agent S3's
  "Behavior Best-of-N" — the technique that beat the human baseline on OSWorld.
  Verification, not bigger models, is the proven reliability fix.
- **Voice-first whole-OS control is still unowned**: Codex/Claude computer use are
  text-driven. No major vendor ships an always-on, wake-word, whole-OS voice agent.
  That combination — voice + OS control + local privacy — is Jarvis's lane.

The two places the market is clearly ahead: **memory** and **proactivity**. Those are
Horizon 2's core.

---

## Horizon 1 — Robustness (make what exists bulletproof)

1. **Persistent brain session** — keep one `ClaudeSDKClient`/`claude` session alive
   instead of per-turn spawn; cuts multi-second turn latency. (Backlog item; biggest
   single UX win per engineering hour.)
2. **Streaming TTS end-to-end** — speak the first sentence while the rest generates.
   Kokoro already sentence-streams synthesis; wire the brain's streaming output into it.
3. **Fast wake word** — replace the 600ms whisper-on-rolling-buffer poll with
   openWakeWord/Porcupine (<100ms, less CPU). Keep whisper for the utterance itself.
4. **True barge-in (AEC)** — the "AEC lite" grace-window works but real echo
   cancellation (speex/webrtc-audio-processing) makes interruption reliable at any
   volume. Interrupt-anywhere is table stakes for voice agents in 2026.
5. **Per-action gating for dev agents** — route the agent SDK's `can_use_tool` through
   the same gate (P4 leftover), so a spawned dev agent's own shell commands get the
   policy treatment, not just spawn-time approval.
6. **Richer agent telemetry to the HUD** — the detail panel currently accumulates
   `last` messages; expose `get_agent_status`'s full event log through a Tauri command
   so selecting an agent shows real history (tool calls, files touched) instantly.
7. **OSWorld-style self-benchmark harness** — a small suite of scripted OS tasks
   (open app, find setting, edit file, web form) run headlessly to catch actuation
   regressions. The bottleneck in 2026 is harness quality, not model capability.
8. **Windows port** — `control/windows.rs` UI-tree reader is the one missing backend;
   everything above it is already OS-agnostic. Opens Jarvis to 90% of desktops.

## Horizon 1.5 — NEXT UP (user-requested, planned)

**Agent → terminal handoff** — open a dev agent's session in a real terminal
(`claude --resume <session-id>` from its worktree) to steer it manually; take-over /
fork / re-adopt semantics. Full researched plan: `docs/agent-terminal-handoff-plan.md`.

## Horizon 2 — Differentiators (the 2026 gaps, in Jarvis's lane)

9. **Tiered personal memory** *(the #1 user complaint about every 2026 agent)*
   - Working memory: current conversation (exists).
   - Episodic: auto-summarize each session to disk; inject relevant summaries next turn.
   - Semantic: facts about the user ("my standup is at 10", "I prefer pnpm") stored as
     structured notes with explicit voice CRUD ("Jarvis, remember/forget…").
   - Implementation: local-only, Mem0-style `add()/search()` over a small SQLite+
     embedding index in the sidecar (Python — no Rust surgery). Local embeddings keep
     everything on-device. LangMem's "procedural memory" idea — Jarvis
     rewrites its own prompt hints from experience — fits the existing prompt assembly.
10. **Routines / scheduled agent turns** *(the 2026 frontier: Pulse, Scheduled Tasks,
    Warp Oz cron agents)* — "every weekday at 9: check calendar + agent fleet + inbox,
    brief me." A cron table in the sidecar that triggers a brain turn whose output rides
    the existing `try_announce` proactive-voice path. Small step, huge perceived IQ.
11. **Morning briefing / fleet standup** — voice command "standup": summarize overnight
    agent results (registry has them), calendar, and queued confirmations. Pure
    composition of existing pieces.
12. **Remote approvals + dispatch (phone)** *(Claude Dispatch / Codex mobile proved the
    UX)* — when a confirmation waits or an agent finishes, push to phone (ntfy/Tailscale
    — self-hosted); reply to approve/deny or dispatch a new task. Turns
    the gate from "you must be at the desk" into "your fleet pings you."
13. **Local screen history (opt-in)** *(Rewind died Dec 2025; Recall is Windows-only —
    the niche is vacant)* — Screenpipe-style periodic screenshot + AX-text capture into
    a local OCR/FTS index. Voice: "what was that error I saw an hour ago?" Strictly
    local, pausable, and honor `protected_path_guard` paths/apps in capture.
14. **Best-of-N verification for risky actions** *(Agent S3's +7.3-point trick)* —
    for multi-step OS tasks, run the observe→act→verify loop with an independent
    verifier pass (screenshot judge, like the QA judge) before declaring success.
    Extends `qa.py`'s judge from web apps to general computer use.
15. **Dictation + command palette anywhere** — a hold-to-talk hotkey that types the
    transcription into the focused app via the existing `type_text` path. Trivial to
    build, used daily; Raycast charges for this.

## Horizon 3 — Creative / moonshots (stand-out features)

16. **Teachable macros ("watch me do this once")** — record an AX-anchored action
    trace, name it by voice, replay with the gate enforcing each step. Personal
    automation without Shortcuts programming.
17. **Meeting copilot** — tap system audio (BlackHole/aggregate device), live local
    Whisper transcript, action items straight into memory + calendar. All offline.
18. **Voice identity** — speaker-embedding check so the always-on mic obeys only you;
    others get a polite refusal. (Also closes a real prompt-injection-by-voice hole.)
19. **Agent Spaces / shared context** *(Devin "Spaces" pattern)* — let multiple dev
    agents share a context bundle (spec, decisions, API sketches) managed by the
    sidecar, so a fleet builds one product coherently instead of N silos.
20. **Cross-vendor agent drivers** — the subscription-brain driver architecture
    (`brain/agent.rs`) was designed agent-agnostic; add Codex/Gemini CLI drivers so
    Jarvis orchestrates *any* vendor's coding agent (ACP is emerging as the open
    protocol here).
21. **HUD "mission control" extensions** — per-agent cost budgets with auto-pause,
    diff viewer + "merge & PR" button in the detail panel (gh CLI), QA-verdict badges
    on orbit nodes. The AGENTS space becomes a real dispatcher console.
22. **Ambient Q&A on screen history + memory** — combine 9 + 13: "when did I last
    talk to Sarah about the pricing page?" — answered from local index, nothing leaves
    the machine. This is the feature no big vendor can ship credibly (privacy) — a
    personal, local Jarvis can.

## Suggested order

Near term: **1, 2, 6, 15** (latency + daily-use wins) → then **9 + 10** (memory +
routines — the differentiation core) → **12** (remote approvals; unlocks true AFK
fleets) → **13/14** as the ambient-intelligence layer, with **3/4/5** slotted in as
polish between larger slices. Windows (8) whenever reach matters more than depth.
