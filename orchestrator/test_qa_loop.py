#!/usr/bin/env python3
"""
Standalone proof for the P3b closed self-correcting QA LOOP control flow — no real
Claude agents, no browser, no network, so it's free and instant. Proves the loop
terminates on every stop condition and the pure helpers behave:

  - PASS on pass 2 stops "passed" after exactly one fix instruct
  - a re-reporting (no-progress) judge stops "stuck" before max_iters
  - an always-different-but-never-passing judge runs to max_iters → "exhausted"
  - a hung fix turn stops "fix_timeout"; an errored fix turn stops "agent_error"
  - a thrown serve/capture/judge stops "qa_error"
  - start_qa_loop returns IMMEDIATELY and refuses a detached agent
  - _issue_words / _jaccard / _build_critique behave

The serve/capture/judge trio is monkeypatched on the `manager` module; the dev agent
is a stub whose `client.query` completes (or hangs / errors) its turn the way the real
drain loop would. Run this BEFORE spending a cent on a real loop:

    ./.venv/bin/python test_qa_loop.py   (or: uv run python test_qa_loop.py)
"""
import asyncio
import contextlib
import os
import tempfile
from pathlib import Path

# Pin the registry + agents root to a throwaway dir BEFORE importing manager, so a
# _persist() during the loop never touches the real workspace roster.
_TMP = Path(tempfile.mkdtemp(prefix="jarvis-qaloop-test-"))
os.environ["JARVIS_AGENTS_REGISTRY"] = str(_TMP / "agents-registry.json")
os.environ["JARVIS_AGENTS_ROOT"] = str(_TMP / "agents")

import manager  # noqa: E402
from manager import (  # noqa: E402
    Agent,
    AgentManager,
    _build_critique,
    _issue_words,
    _jaccard,
)

PASS, FAIL = "\033[92mPASS\033[0m", "\033[91mFAIL\033[0m"
_ok = True


def check(label: str, cond: bool) -> None:
    global _ok
    _ok = _ok and bool(cond)
    print(f"  [{PASS if cond else FAIL}] {label}")


# ── fakes ─────────────────────────────────────────────────────────────────────

@contextlib.contextmanager
def fake_serve_app(worktree, build=True):
    """Yield a dummy URL; no real http.server, no port."""
    yield "http://127.0.0.1:0/"


async def fake_capture(url, spec="", out_dir=None, sentinel=None):
    """Minimal capture() shape — one shot, no errors. The shot path is never read
    (the fake judge ignores it and _qa_result only stores the path string)."""
    return {
        "url": url,
        "shots": [{"label": "desktop", "path": str(_TMP / "x.png"), "width": 1280, "height": 900}],
        "console_errors": [],
        "page_errors": [],
        "out_dir": str(_TMP / "shots"),
    }


def scripted_judge(verdicts):
    """An async judge() that returns the given verdicts in order, repeating the last
    one forever once exhausted. Returns (judge_fn, calls) so tests can count calls."""
    seq = [dict(v) for v in verdicts]
    calls = {"n": 0}

    async def _judge(spec, shots, console_errors=None, page_errors=None, model=None):
        v = dict(seq[min(calls["n"], len(seq) - 1)])
        calls["n"] += 1
        v.setdefault("model", "sonnet")
        v.setdefault("raw", "")
        return v

    return _judge, calls


async def raising_judge(spec, shots, console_errors=None, page_errors=None, model=None):
    raise RuntimeError("judge exploded")


class StubClient:
    """Stand-in for ClaudeSDKClient. `query` records the instruction and, after the
    main loop reaches its `await turn_done.wait()`, completes the turn the way the
    real drain loop would — setting status (idle/error) then turn_done. `complete`:
      "ok"    → turn finishes idle (status stays whatever the loop set: working)
      "error" → turn finishes with status=error
      "hang"  → turn never completes (drives fix_timeout)
    """

    def __init__(self, complete: str = "ok"):
        self.complete = complete
        self.queries: list[str] = []
        self.agent: Agent | None = None

    async def query(self, text: str) -> None:
        self.queries.append(text)
        if self.complete == "hang":
            return
        agent = self.agent

        async def _finish():
            # Yield once so the loop's synchronous `_set_status(working)` runs first,
            # exactly like the real drain delivering ResultMessage after the query.
            await asyncio.sleep(0)
            if self.complete == "error":
                agent.status = "error"
            agent.turn_done.set()

        asyncio.create_task(_finish())


def patch_qa(judge_fn) -> None:
    manager.serve_app = fake_serve_app
    manager.capture = fake_capture
    manager.judge = judge_fn


def make_live_agent(mgr: AgentManager, name: str, complete: str = "ok") -> Agent:
    wt = _TMP / "wt" / name
    wt.mkdir(parents=True, exist_ok=True)
    client = StubClient(complete=complete)
    a = Agent(
        id=f"{name}-id", name=name, project_dir=wt, worktree=wt, branch="main",
        client=client, model="sonnet",
    )
    client.agent = a
    mgr.agents[a.id] = a
    return a


# verdict factories
def needs(fixes, **scores):
    s = {"layout": 2, "typography": 3, "color": 3, "responsiveness": 3, "spec_adherence": 2}
    s.update(scores)
    return {"verdict": "NEEDS_WORK", "scores": s, "summary": "needs work", "fixes": list(fixes)}


def passing():
    return {"verdict": "PASS",
            "scores": {"layout": 5, "typography": 5, "color": 5, "responsiveness": 5, "spec_adherence": 5},
            "summary": "looks great", "fixes": []}


# ── tests ──────────────────────────────────────────────────────────────────────

async def test_pass_on_pass_two():
    print("\n== PASS on pass 2 → stop 'passed', exactly one fix sent ==")
    mgr = AgentManager()
    agent = make_live_agent(mgr, "passer", complete="ok")
    jf, calls = scripted_judge([needs(["Add a 3-card features section", "Add a primary CTA"]), passing()])
    patch_qa(jf)
    res = await mgr._qa_fix_loop(agent, "hero + features + CTA", max_iters=3, turn_timeout=5)
    check("passed is True", res["passed"] is True)
    check("stop_reason == 'passed'", res["stop_reason"] == "passed")
    check("iterations == 2", res["iterations"] == 2)
    check("exactly one fix instruct sent", len(agent.client.queries) == 1)
    check("two judge calls", calls["n"] == 2)
    check("history has 2 passes", len(res["history"]) == 2)
    check("final verdict carried through", res["final"]["verdict"] == "PASS")
    check("two spoken lines (instruct + pass)", len(res["spoken"]) == 2)
    check("announce_seq bumped twice", agent.announce_seq == 2)
    check("critique embeds the spec", "hero + features + CTA" in agent.client.queries[0])


async def test_stuck():
    print("\n== never-improving (same issues) → stop 'stuck' before max_iters ==")
    mgr = AgentManager()
    agent = make_live_agent(mgr, "stuckler", complete="ok")
    same = ["Increase the hero headline font size", "Add contrast to the CTA button"]
    jf, calls = scripted_judge([needs(same), needs(same), needs(same)])
    patch_qa(jf)
    res = await mgr._qa_fix_loop(agent, "a polished hero", max_iters=4, turn_timeout=5)
    check("passed is False", res["passed"] is False)
    check("stop_reason == 'stuck'", res["stop_reason"] == "stuck")
    check("stuck on pass 2", res["iterations"] == 2)
    check("only one fix sent before giving up", len(agent.client.queries) == 1)


async def test_exhausted():
    print("\n== always different but never passing → stop 'exhausted' at max_iters ==")
    mgr = AgentManager()
    agent = make_live_agent(mgr, "exhauster", complete="ok")
    jf, calls = scripted_judge([
        needs(["Hero headline overlaps the navigation bar"]),
        needs(["Footer columns misaligned on mobile breakpoint"]),
    ])
    patch_qa(jf)
    res = await mgr._qa_fix_loop(agent, "a clean responsive layout", max_iters=2, turn_timeout=5)
    check("passed is False", res["passed"] is False)
    check("stop_reason == 'exhausted'", res["stop_reason"] == "exhausted")
    check("ran all max_iters", res["iterations"] == 2)
    check("one fix sent (after pass 1)", len(agent.client.queries) == 1)


async def test_fix_timeout():
    print("\n== fix turn hangs → stop 'fix_timeout' ==")
    mgr = AgentManager()
    agent = make_live_agent(mgr, "hanger", complete="hang")
    jf, _ = scripted_judge([needs(["Add the missing CTA button"])])
    patch_qa(jf)
    res = await mgr._qa_fix_loop(agent, "needs a CTA", max_iters=3, turn_timeout=0.05)
    check("stop_reason == 'fix_timeout'", res["stop_reason"] == "fix_timeout")
    check("stopped on pass 1", res["iterations"] == 1)
    check("fix was attempted", len(agent.client.queries) == 1)


async def test_agent_error():
    print("\n== fix turn errors → stop 'agent_error' ==")
    mgr = AgentManager()
    agent = make_live_agent(mgr, "errorer", complete="error")
    jf, _ = scripted_judge([needs(["Add the missing CTA button"])])
    patch_qa(jf)
    res = await mgr._qa_fix_loop(agent, "needs a CTA", max_iters=3, turn_timeout=5)
    check("stop_reason == 'agent_error'", res["stop_reason"] == "agent_error")
    check("stopped on pass 1", res["iterations"] == 1)


async def test_qa_error():
    print("\n== serve/capture/judge throws → stop 'qa_error' ==")
    mgr = AgentManager()
    agent = make_live_agent(mgr, "qaerr", complete="ok")
    patch_qa(raising_judge)
    res = await mgr._qa_fix_loop(agent, "anything", max_iters=3, turn_timeout=5)
    check("stop_reason == 'qa_error'", res["stop_reason"] == "qa_error")
    check("stopped on pass 1", res["iterations"] == 1)
    check("no fix sent (failed before judging completed)", len(agent.client.queries) == 0)
    check("final is None (no usable verdict)", res["final"] is None)


async def test_start_returns_immediately_and_rejects_detached():
    print("\n== start_qa_loop: returns immediately + refuses a detached agent ==")
    mgr = AgentManager()
    agent = make_live_agent(mgr, "kickoff", complete="ok")
    jf, _ = scripted_judge([passing()])  # background loop will pass fast and finish
    patch_qa(jf)
    started = await mgr.start_qa_loop("kickoff", "a tidy page", max_iters=2)
    check("started flag set", started.get("started") is True)
    check("resolves the live agent by name", started.get("agent_id") == agent.id)
    check("max_iters echoed", started.get("max_iters") == 2)
    # Let the background task complete so it doesn't leak as a pending task.
    for _ in range(50):
        await asyncio.sleep(0)
        if agent.announce and "passed" in agent.announce:
            break
    check("background loop ran and announced a pass", "passed" in (agent.announce or ""))

    # A roster-only (detached) agent can't be looped — must raise, not silently no-op.
    mgr.registry["ghost-id"] = {"name": "ghost", "worktree": str(_TMP / "ghost"), "live": False}
    raised = False
    try:
        await mgr.start_qa_loop("ghost", "spec")
    except ValueError:
        raised = True
    check("detached agent raises ValueError", raised)

    unknown = False
    try:
        await mgr.start_qa_loop("nobody-here", "spec")
    except KeyError:
        unknown = True
    check("unknown target raises KeyError", unknown)


def test_helpers():
    print("\n== pure helpers: _issue_words / _jaccard / _build_critique ==")
    w = _issue_words(["Increase the hero headline font size"])
    check("_issue_words keeps signal nouns", {"hero", "headline", "font", "size"} <= w)
    check("_issue_words drops short words ('the')", "the" not in w)
    check("_issue_words drops filler ('increase')", "increase" not in w)

    check("_jaccard(∅,∅) == 1.0", _jaccard(set(), set()) == 1.0)
    check("_jaccard(∅,x) == 0.0", _jaccard(set(), {"a"}) == 0.0)
    check("_jaccard identical == 1.0", _jaccard({"a", "b"}, {"a", "b"}) == 1.0)
    check("_jaccard half-overlap == 1/3", abs(_jaccard({"a", "b"}, {"b", "c"}) - 1 / 3) < 1e-9)

    # Identical fix lists are the clean 'stuck' trigger (Jaccard 1.0 ≥ 0.8).
    same = ["Increase the hero headline font size", "Add contrast to the CTA button"]
    check("identical issue lists score 1.0",
          _jaccard(_issue_words(same), _issue_words(same)) == 1.0)
    # Filler/word-order rewording with the SAME content words still reads as 'stuck'.
    a = _issue_words(same)
    b = _issue_words(["The hero headline font size should increase",
                      "The CTA button still needs contrast"])
    check("filler-only rewording stays ≥ 0.8 (still 'stuck')", _jaccard(a, b) >= manager._NO_PROGRESS_SIM)
    # The threshold is deliberately strict: swapping even one CONTENT word drops it
    # below 0.8, so the loop errs toward 'exhausted' (try again) over a false 'stuck'.
    c = _issue_words(["Make the hero headline font larger", "The CTA button needs more contrast"])
    check("content-word swap falls below 0.8 (errs toward 'exhausted')",
          _jaccard(a, c) < manager._NO_PROGRESS_SIM)

    crit = _build_critique(
        "hero + features + CTA",
        needs(["Add a CTA button"], layout=2, spec_adherence=1, typography=4),
        pass_num=1,
    )
    check("critique embeds the spec", "hero + features + CTA" in crit)
    check("critique has a numbered fix", "1. Add a CTA button" in crit)
    check("critique flags weak areas (<4)", "layout" in crit and "spec_adherence" in crit)
    check("critique excludes strong areas (≥4)", "typography" not in crit.split("Weakest areas")[-1].split("\n")[0])
    check("critique tells the agent to commit", "git add -A && git commit" in crit)


async def main() -> int:
    await test_pass_on_pass_two()
    await test_stuck()
    await test_exhausted()
    await test_fix_timeout()
    await test_agent_error()
    await test_qa_error()
    await test_start_returns_immediately_and_rejects_detached()
    test_helpers()
    print()
    return 0 if _ok else 1


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
