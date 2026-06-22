# P3b plan — the closed self-correcting QA loop (the hero)

> Start here for P3b. Base: `main` @ `0640d9d` (clean). Read `docs/P3-plan.md` §2-§3
> for the original framing and `docs/P3-research.md` Report 1 for the loop shape.
> P3a (one-shot `qa_web_app`) is DONE, verified in-app, committed. P3b makes it a
> LOOP: judge → critique the agent → re-judge → until PASS. Respect the hard rules
> (protected paths; no `Co-Authored-By`).

## The north star
`build → verify → CORRECT → re-verify` hands-free. Voice request → agent builds →
Jarvis QAs it → if it's wrong, Jarvis critiques the agent → agent fixes → re-QA →
"it passes now." Voice in, self-correction, spoken verdict out.

## THE load-bearing lesson from P3a testing (do not relearn this the hard way)
When asked "check the landing page", the brain **bypassed `qa_web_app`** and improvised
with `run_shell` (cat/ls/open the page in the real browser) + `see_screen`. Fixed for the
one-shot by a system-prompt steer (`brain/mod.rs` Rule 7a). **Implication for P3b: the loop
MUST be sidecar-driven and deterministic — NEVER a brain-driven "call qa_web_app, then
instruct, then call qa_web_app again" sequence.** The brain's only jobs are (1) KICK OFF the
loop with one tool call and (2) let the proactive voice NARRATE it. All the
serve→capture→judge→instruct→re-judge control flow lives in Python.

## Architecture
```
brain (one call) ── start_qa_loop(target, spec, max_iters) ──► returns immediately
                                                                   │ (async task)
orchestrator/manager.py::_qa_fix_loop  ◄───────────────────────────┘
   loop i in 1..=max_iters:
     serve_app(worktree) → capture() → judge()        # reuse qa.py (P3a)
     PASS?            → stop "passed"
     no new progress? → stop "stuck"
     last iter?       → stop "exhausted"
     else: build critique from judge.fixes → agent.client.query(critique)
           → await agent.turn_done (drain sets it on ResultMessage) → next i
   each verdict/outcome → agent.announce (+ announce_seq++)  # the spoken line
        │
Rust poll thread (P2) reads announce_seq deltas → speaks via try_announce (Idle-gated)
```

## Staged build (cost-disciplined — validate cheap before spending the subscription)

### Stage 1 — the Python loop controller + FREE unit tests (no agent, no network)
All in `orchestrator/manager.py` (+ `orchestrator/test_qa_loop.py`).

1. **`Agent` dataclass** (it's a `@dataclass`): add
   - `turn_done: asyncio.Event = field(default_factory=asyncio.Event)`
   - `announce: Optional[str] = None`
   - `announce_seq: int = 0`
   and add `"announce"` + `"announce_seq"` to `summary()`. *(asyncio.Event() needs no
   running loop at construction in 3.10+ — safe in a default_factory.)*
2. **`_drain`**: right after `self._set_status(agent, "error" if msg.is_error else "idle")`
   on the `ResultMessage` branch, add `agent.turn_done.set()`.
3. **`_announce(self, agent, text)`**: `agent.announce = text; agent.announce_seq += 1;
   self._event(agent, "announce", text); self._persist(agent)`.
4. **Pure helpers** (module-level, easy to unit-test):
   - `_issue_words(fixes) -> set[str]` — lowercase each fix, strip non-alnum, drop
     stopwords + words ≤3 chars → a word-set (tolerant of rewording).
   - `_jaccard(a, b) -> float` — set similarity (handle empty/empty = 1.0, empty/x = 0.0).
   - `_build_critique(spec, verdict, pass_num) -> str` — embed the spec, the verdict +
     summary, the weak rubric areas (<4/5), and a NUMBERED fix list; tell the agent to make
     the actual code changes in its worktree and `git add -A && git commit`, keeping the rest intact.
   - Constants: `_NO_PROGRESS_SIM = 0.8`; `max_iters` default from `JARVIS_QA_MAX_ITERS` (3);
     `turn_timeout` from `JARVIS_QA_FIX_TIMEOUT` (240s).
5. **`async def _qa_fix_loop(self, agent, spec, max_iters, turn_timeout)`** — the loop body
   (operates on a resolved LIVE `Agent`). Per iteration:
   - `with serve_app(agent.worktree) as url: cap = await capture(url, spec);
     verdict = await judge(spec, cap["shots"], cap.get("console_errors"), cap.get("page_errors"))`
     (wrap in try/except → stop `"qa_error"`).
   - record `history.append({pass,verdict,scores,fixes})`; `self._event(agent,"qa_loop",…)`; log `[qa-loop]`.
   - `PASS` → `_announce("…passed visual QA on pass i")`, stop `"passed"`.
   - no-progress: `words=_issue_words(fixes)`; if `prev_words` set and `_jaccard ≥ _NO_PROGRESS_SIM`
     → `_announce("…not making progress…take a look?")`, stop `"stuck"`. Else `prev_words = words`.
   - `i == max_iters` → `_announce("I've QA'd it N times, still needs work — <top fix>. Take a look?")`,
     stop `"exhausted"`.
   - else INSTRUCT: `_announce("Pass i: still needs work, sending fixes")`;
     `agent.turn_done.clear()`; `await agent.client.query(_build_critique(...))`;
     `self._set_status(agent,"working")`; `await asyncio.wait_for(agent.turn_done.wait(), turn_timeout)`
     (TimeoutError → stop `"fix_timeout"`); if `agent.status=="error"` → stop `"agent_error"`.
   - Return `{agent_id, name, passed, stop_reason, iterations, final: _qa_result(...), history, spoken}`.
6. **`async def start_qa_loop(self, target, spec, max_iters=None)`** — resolve a **LIVE** agent
   (reuse `_worktree_by_name`/id lookup, but the agent MUST be in `self.agents` with a live client;
   a DETACHED registry agent can't be instructed — raise a clear error telling the user to spawn/re-spawn).
   `_assert_not_protected(worktree)`. Then `asyncio.create_task(self._qa_fix_loop(...))` and **return
   immediately**: `{"started": True, "agent_id", "name", "max_iters", "message": "Running QA-and-fix…"}`.
   (The loop runs minutes — it must NOT block the MCP call. Proactive voice reports the outcome.)
7. **`orchestrator/test_qa_loop.py` (FREE)** — monkeypatch `manager.serve_app`/`capture`/`judge`
   and use a fake agent (a stub `client.query` + a stub that sets `turn_done`) to assert the control
   flow: PASS-on-pass-2 stops `"passed"` after one fix; a never-improving judge stops `"stuck"` or
   `"exhausted"`; `_issue_words`/`_jaccard`/`_build_critique` behave. **Run this before spending a cent.**

### Stage 2 — ONE real validation run (first subscription cost; ASK before running)
Spawn a FRESH agent (the on-disk `landing-page-builder` is DETACHED → can't be looped) with a task
that's deliberately short of the spec, e.g. task = "build a tiny one-page site: index.html + style.css
for a product 'Nimbus' with ONLY a hero section (headline + subtext)"; qa-spec = "hero + a 3-card
features section + a primary CTA button, responsive". First judge → NEEDS_WORK (missing features/CTA)
→ loop instructs → agent adds them → re-judge → PASS. Keep `max_iters=2`. Drive it with a small Python
script through `manager.start_qa_loop` (NO Rust/brain needed yet) and tail `orchestrator.log` for the
`[qa-loop]` lines. **The headless browser launches each iteration — have the USER run the launch, or get explicit OK.**

### Stage 3 — brain trigger tool + voice narration (one Rust rebuild)
1. **`server.py`**: `@mcp.tool() async def start_qa_loop(target, spec, max_iters: int = 0)` →
   `manager.start_qa_loop(...)`.
2. **`tools.rs`**: register `start_qa_loop`, `DangerLevel::Mutating` (it drives autonomous code
   changes via repeated instructs, like `instruct_dev_agent`). Returns immediately, so no long timeout needed.
3. **`brain/mod.rs`**: extend the "Dev agents & visual QA" section — when the user wants the agent to
   FIX issues / "QA it and fix until it passes" / says "yes, have it fix those" after a NEEDS_WORK
   `qa_web_app`, call `start_qa_loop` (NOT a manual cat/instruct dance). Mirror Rule 7a's specificity.
4. **Voice narration — `lib.rs` `diff_agent_updates`**: add a second prev map
   `prev_announce: &mut HashMap<String,u64>` (thread it through the poll loop alongside `prev`; update
   the 3 unit-test call sites). For each live agent: if `announce_seq` increased since last seen and
   `announce` is non-empty, push `announce` verbatim. ALSO add `qa_active: bool` to the summary
   (true while a loop task is running) and **suppress** the generic "finished its turn"/"ran into an
   error" phrases while `qa_active` — the loop's `announce` lines carry the narrative instead.
   Add a unit test (sibling of `working_to_idle_announces_once`) for announce-seq emission.
5. Rebuild (`pnpm install:app`), relaunch, test by voice: "build me X, then QA it and fix it until it
   passes" OR (cheaper) point it at a fresh live agent and say "have the agent fix the QA issues until
   it passes" → watch the loop run and Jarvis narrate each verdict.

## Stop conditions (all required — never loop forever, never silently give up)
`passed` (PASS) · `exhausted` (max_iters) · `stuck` (issue-set ~unchanged, Jaccard ≥ 0.8) ·
`fix_timeout` (agent turn > turn_timeout) · `agent_error` (status→error) · `qa_error` (serve/capture/judge threw).
Every non-pass stop announces honestly ("I've tried N times… want to take a look?") — never silence.

## Cost notes
- Each iteration = 1 agent fix turn (sonnet) + 1 judge call (sonnet) + free serve/capture. `max_iters=3`
  ⇒ up to ~3 fix turns + ~3 judge calls on the shared usage window. Keep `max_iters` low; expose the env knob.
- All LLM traffic is Claude (already-accepted endpoint). The headless browser launches per iteration —
  fine (invisible) but it IS a browser; honor the ask-rule during manual testing.
- Build/test gates (P3c) are the cheaper, deterministic complement — a build/tsc/test failure becomes a
  free critique fed back before screenshotting. Consider folding a `build`/`tsc` check into the loop's
  pre-judge step later.

## Reuse map
| Need | Reuse | Build |
|---|---|---|
| serve / screenshot / judge | `qa.py` `serve_app`/`capture`/`judge` (P3a) | — |
| await agent turn done | `_drain` ResultMessage path | `turn_done` Event + `.set()` |
| spoken updates, Idle-gated | P2 poll thread + `try_announce` + `diff_agent_updates` | `announce`/`announce_seq` + suppress-while-qa_active |
| resolve target → agent | `_worktree_by_name` / id lookup | LIVE-agent variant for the loop |
| HUD / audit / events | `_event`, `jarvis:activity`, `audit.jsonl` | `[qa-loop]` events |
