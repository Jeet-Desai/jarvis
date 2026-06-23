#!/usr/bin/env python3
"""
P3b Stage 2 — ONE real validation run of the closed self-correcting QA loop.

This is the first subscription-cost run: it spawns a FRESH dev agent on a task
deliberately SHORT of the spec (hero only), then drives manager.start_qa_loop,
which serves → screenshots → judges → critiques → re-judges. Expected arc:
  pass 1 → NEEDS_WORK (missing the 3-card features section + the CTA)
  loop instructs the agent → it adds them
  pass 2 → PASS  (or, if the judge is still unhappy, a clean 'exhausted' at max_iters)

No Rust / brain / MCP involved — pure Python through the manager. The headless
browser launches once per QA pass.

    ./.venv/bin/python qa_loop_stage2.py        # or: uv run python qa_loop_stage2.py

Tail the loop live in another shell:
    tail -f ~/claude/jarvis-workspace/orchestrator.log | grep -E 'qa-loop|nimbus'
"""
import asyncio
import json
import time
from pathlib import Path

from manager import AgentManager

TASK = (
    "Build a tiny one-page STATIC site: an index.html plus a style.css, for a product "
    "called 'Nimbus'. Include ONLY a hero section — a large headline and one line of "
    "subtext. Do NOT add any other sections. Plain hand-written HTML and CSS, no build "
    "step, no framework. Keep it to those two files."
)
SPEC = (
    "A landing page for the product 'Nimbus' that has: (1) a hero section with a headline "
    "and subtext, (2) a features section of EXACTLY three cards, and (3) a primary "
    "call-to-action button. It should look clean and modern and be responsive on both "
    "mobile and desktop."
)
RESULT_FILE = Path(__file__).resolve().parent / "stage2-result.json"


async def wait_idle(mgr: AgentManager, aid: str, timeout: float = 300.0) -> str:
    """Poll until the agent's current turn finishes (idle/error/stopped)."""
    done = {"idle", "error", "stopped"}
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        st = mgr.status(aid)["status"]
        if st in done:
            return st
        await asyncio.sleep(2)
    return mgr.status(aid)["status"]


def _loop_done_event(agent) -> dict | None:
    for e in reversed(agent.events):
        if e["kind"] == "qa_loop" and e["text"].startswith("loop done"):
            return e
    return None


async def main() -> int:
    mgr = AgentManager()

    print("── SPAWN — fresh agent, deliberately short of spec (hero only) ──")
    sp = await mgr.spawn(project_dir="nimbus-qaloop", task=TASK, name="nimbus", model="sonnet")
    aid = sp["id"]
    print(f"  agent {aid}  worktree={sp['worktree']}")
    st = await wait_idle(mgr, aid)
    files0 = sorted(p.name for p in Path(sp["worktree"]).iterdir() if p.is_file())
    print(f"  first build turn: {st}  files={files0}")

    print("\n── START QA-AND-FIX LOOP (max_iters=2) — should return immediately ──")
    t0 = time.monotonic()
    started = await mgr.start_qa_loop(aid, SPEC, max_iters=2)
    print(f"  returned in {time.monotonic()-t0:.2f}s: {started}")

    print("\n── LOOP RUNS (each pass: serve → screenshot → judge → maybe fix) ──")
    deadline = time.monotonic() + 900
    last_seq = mgr.agents[aid].announce_seq
    # print the announce captured by start_qa_loop's kickoff event too
    while time.monotonic() < deadline:
        a = mgr.agents[aid]
        if a.announce_seq != last_seq:
            last_seq = a.announce_seq
            print(f"  🗣  announce#{a.announce_seq}: {a.announce}")
        if _loop_done_event(a) is not None:
            break
        await asyncio.sleep(2)

    a = mgr.agents[aid]
    files1 = sorted(p.name for p in Path(a.worktree).iterdir() if p.is_file())
    out = {
        "agent_id": aid,
        "worktree": str(a.worktree),
        "files_before": files0,
        "files_after": files1,
        "final_announce": a.announce,
        "loop_done": _loop_done_event(a),
        "qa_loop_events": [e for e in a.events if e["kind"] == "qa_loop"],
        "all_spoken": [e["text"] for e in a.events if e["kind"] == "announce"],
        "cost_usd": round(a.cost_usd, 4),
        "turns": a.turns,
    }
    RESULT_FILE.write_text(json.dumps(out, indent=2))
    print("\n── RESULT ──")
    print(json.dumps(out, indent=2))
    print(f"\n  (also written to {RESULT_FILE})")

    await mgr.stop(aid)
    ok = out["loop_done"] is not None
    print(f"\n{'PASS' if ok else 'FAIL'} — loop terminated cleanly: {ok}")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
