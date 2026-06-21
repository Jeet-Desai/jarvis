#!/usr/bin/env python3
"""
Standalone proof for the dev-agent PERSISTENCE slice (no Jarvis, no MCP, no real
Claude agents — so it's free and instant). Proves:

  - spawning/status changes write a JSON roster to disk
  - a fresh AgentManager (simulating an app/sidecar restart) reloads that roster
  - reloaded agents come back DETACHED: live=False, transient status downgraded
  - list() merges live agents over detached history (live wins), newest first
  - status() serves a detached agent's last-known record
  - a corrupt registry file degrades to an empty roster instead of crashing

Run:  ./.venv/bin/python test_persistence.py   (or: uv run python test_persistence.py)
"""
import os
import tempfile
import time
from pathlib import Path

# Pin the registry to a throwaway file BEFORE importing manager, and keep the
# agents root out of the real workspace too.
_TMP = Path(tempfile.mkdtemp(prefix="jarvis-persist-test-"))
os.environ["JARVIS_AGENTS_REGISTRY"] = str(_TMP / "agents-registry.json")
os.environ["JARVIS_AGENTS_ROOT"] = str(_TMP / "agents")

from manager import Agent, AgentManager  # noqa: E402

PASS, FAIL = "\033[92mPASS\033[0m", "\033[91mFAIL\033[0m"
_ok = True


def check(label: str, cond: bool) -> None:
    global _ok
    _ok = _ok and cond
    print(f"  [{PASS if cond else FAIL}] {label}")


def fake_agent(mgr: AgentManager, agent_id: str, name: str, status: str, **kw) -> Agent:
    """A minimal in-memory Agent (no live SDK client — None is never touched by
    summary/_snapshot) so we can drive persistence without spawning anything."""
    a = Agent(
        id=agent_id, name=name, project_dir=_TMP / "proj" / name,
        worktree=_TMP / "proj" / name, branch="main", client=None, model="sonnet",
        **kw,
    )
    a.status = status
    mgr.agents[agent_id] = a
    return a


def main() -> int:
    reg = Path(os.environ["JARVIS_AGENTS_REGISTRY"])

    print("\n== run 1: spawn-ish writes + status changes persist ==")
    m1 = AgentManager()
    a1 = fake_agent(m1, "aaa11111", "site-builder", "spawning", created=time.time() - 10)
    m1._persist(a1)
    a1.last_text = "scaffolded index.html"
    a1.cost_usd = 0.12
    a1.turns = 1
    m1._set_status(a1, "working")
    m1._set_status(a1, "idle")  # turn finished

    a2 = fake_agent(m1, "bbb22222", "api-builder", "working", created=time.time())
    m1._persist(a2)

    check("registry file written", reg.exists())
    rows1 = m1.list()
    check("run 1 list() shows 2 live agents", len(rows1) == 2)
    check("run 1 agents are live=True", all(r["live"] for r in rows1))
    check("run 1 newest-first ordering", rows1[0]["id"] == "bbb22222")

    print("\n== run 2: a fresh manager reloads the roster (restart) ==")
    m2 = AgentManager()  # no live agents — simulates a cold restart
    rows2 = m2.list()
    by_id = {r["id"]: r for r in rows2}
    check("run 2 reloaded both agents", len(rows2) == 2)
    check("run 2 agents are detached (live=False)", all(not r["live"] for r in rows2))
    check("idle agent keeps idle status", by_id["aaa11111"]["status"] == "idle")
    check("idle agent keeps last/cost/turns",
          by_id["aaa11111"]["last"] == "scaffolded index.html"
          and by_id["aaa11111"]["cost_usd"] == 0.12
          and by_id["aaa11111"]["turns"] == 1)
    check("transient 'working' downgraded to idle on reload",
          by_id["bbb22222"]["status"] == "idle")
    check("project_dir preserved for recall", "api-builder" in by_id["bbb22222"]["project_dir"])

    print("\n== run 2: live agent overrides detached history ==")
    live = fake_agent(m2, "aaa11111", "site-builder", "working", created=time.time() - 10)
    live.last_text = "now adding CSS"
    merged = {r["id"]: r for r in m2.list()}
    check("re-spawned id reads as live again", merged["aaa11111"]["live"] is True)
    check("live status wins over registry", merged["aaa11111"]["status"] == "working")
    check("still only 2 distinct agents", len(m2.list()) == 2)

    print("\n== status(): detached vs unknown ==")
    det = m2.status("bbb22222")
    check("status() serves a detached agent", det["live"] is False and det["status"] == "idle")
    check("status() detached has empty events", det.get("events") == [])
    try:
        m2.status("does-not-exist")
        check("status() unknown id raises", False)
    except KeyError:
        check("status() unknown id raises", True)

    print("\n== corrupt registry degrades gracefully ==")
    reg.write_text("{ this is not json")
    m3 = AgentManager()
    check("corrupt registry → empty roster, no crash", m3.list() == [])

    print()
    return 0 if _ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
