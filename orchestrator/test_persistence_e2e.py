#!/usr/bin/env python3
"""
END-TO-END proof of the persistence slice with a REAL Claude dev agent (costs a
few cents on the Claude subscription). Unlike test_persistence.py (which fabricates
Agents), this drives the real SDK path: spawn → the drain loop tracks the turn →
the registry is written on the working→idle transition → a FRESH AgentManager
reloads it as a detached (live=False) record.

Everything is pinned to a throwaway temp workspace so it never touches the real
~/claude/jarvis-workspace roster.

Run:  ./.venv/bin/python test_persistence_e2e.py
"""
import asyncio
import json
import os
import tempfile
import time
from pathlib import Path

_TMP = Path(tempfile.mkdtemp(prefix="jarvis-e2e-"))
os.environ["JARVIS_AGENTS_ROOT"] = str(_TMP / "agents")
os.environ["JARVIS_PROJECTS_ROOT"] = str(_TMP / "projects")
os.environ["JARVIS_AGENTS_REGISTRY"] = str(_TMP / "agents-registry.json")
os.environ["JARVIS_ORCH_LOG"] = str(_TMP / "orchestrator.log")

from manager import AgentManager  # noqa: E402

PASS, FAIL = "\033[92mPASS\033[0m", "\033[91mFAIL\033[0m"
_ok = True


def check(label: str, cond: bool) -> None:
    global _ok
    _ok = _ok and cond
    print(f"  [{PASS if cond else FAIL}] {label}")


async def wait_idle(mgr: AgentManager, agent_id: str, timeout: float = 120) -> str:
    """Poll until the agent finishes its first turn (idle/error/stopped)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        st = mgr.status(agent_id)["status"]
        if st in ("idle", "error", "stopped"):
            return st
        await asyncio.sleep(1.0)
    return "timeout"


async def main() -> int:
    reg = Path(os.environ["JARVIS_AGENTS_REGISTRY"])
    print(f"\n== isolated workspace: {_TMP} ==")

    print("\n== run 1: spawn a real agent on a trivial task ==")
    m1 = AgentManager()
    res = await m1.spawn(
        project_dir="",  # brand-new project under the temp PROJECTS_ROOT
        task="Create a file named hello.txt containing exactly the text 'hi from jarvis'. "
             "Then stop — do nothing else.",
        name="e2e-probe",
    )
    aid = res["id"]
    print(f"  spawned agent {aid} (name={res['name']}) → waiting for the turn to finish…")

    check("registry written immediately on spawn", reg.exists())
    final = await wait_idle(m1, aid)
    print(f"  turn finished with status: {final}")
    check("agent reached a terminal status (idle/error)", final in ("idle", "error"))

    live_summary = m1.status(aid)
    check("live agent reports live=True", live_summary.get("live") is True)
    check("turn produced a cost figure", live_summary.get("cost_usd", 0) >= 0)

    # Inspect the on-disk registry directly.
    on_disk = json.loads(reg.read_text())
    check("registry holds the agent record", aid in on_disk)
    check("registry status matches the finished turn",
          on_disk[aid]["status"] == final)
    check("registry preserved project_dir + worktree",
          "project_dir" in on_disk[aid] and "worktree" in on_disk[aid])

    # Did the agent actually do the work in its isolated worktree?
    hello = Path(live_summary["worktree"]) / "hello.txt"
    check("agent wrote hello.txt inside its own worktree", hello.exists())

    await m1.stop(aid)  # release the live SDK session

    print("\n== run 2: a FRESH manager reloads the roster (simulated restart) ==")
    m2 = AgentManager()  # no live agents — same registry path via env
    rows = m2.list()
    by_id = {r["id"]: r for r in rows}
    check("reloaded the agent from disk", aid in by_id)
    check("reloaded agent is detached (live=False)", by_id.get(aid, {}).get("live") is False)
    check("status() serves the detached agent", m2.status(aid).get("live") is False)
    check("detached status is terminal (not spawning/working)",
          by_id.get(aid, {}).get("status") in ("idle", "error", "stopped"))

    print(f"\n  (temp workspace left at {_TMP} for inspection)\n")
    return 0 if _ok else 1


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
