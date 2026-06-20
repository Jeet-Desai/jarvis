#!/usr/bin/env python3
"""
Standalone proof for AgentManager (no Jarvis, no MCP) — the Phase-1 analogue of
the Phase-0 spike. Proves:

  - two agents run CONCURRENTLY, each in its own git worktree/branch
  - acceptEdits writes land inside each agent's OWN worktree (isolation)
  - a follow-up instruct() reaches a live agent (persistent session)
  - neither agent's files leak into the other worktree or the main project
  - status/cost tracking works

Run:  uv run python test_manager.py
"""
import asyncio
import shutil
import subprocess
import time
from pathlib import Path

from manager import AgentManager

PROJ = Path.home() / ".jarvis-test" / "demo"     # scratch "user project" repo
PASS, FAIL = "\033[92mPASS\033[0m", "\033[91mFAIL\033[0m"


def sh(args, cwd=None):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"$ {' '.join(args)}\n{r.stderr}")
    return r


def banner(s):
    print(f"\n{'='*70}\n{s}\n{'='*70}")


def setup_project():
    banner("SETUP — scratch git project (the 'user repo')")
    # Clean any prior agent worktrees registered against this project.
    if PROJ.exists():
        wt = subprocess.run(["git", "worktree", "list", "--porcelain"],
                            cwd=PROJ, capture_output=True, text=True)
        for line in wt.stdout.splitlines():
            if line.startswith("worktree ") and "/.jarvis/agents/" in line:
                subprocess.run(["git", "worktree", "remove", "--force", line.split(" ", 1)[1]],
                               cwd=PROJ, capture_output=True, text=True)
    shutil.rmtree(PROJ, ignore_errors=True)
    PROJ.mkdir(parents=True)
    sh(["git", "init", "-q", "-b", "main"], PROJ)
    sh(["git", "config", "user.email", "demo@jarvis.local"], PROJ)
    sh(["git", "config", "user.name", "demo"], PROJ)
    (PROJ / "README.md").write_text("# demo project\n")
    sh(["git", "add", "-A"], PROJ)
    sh(["git", "commit", "-qm", "init"], PROJ)
    print(f"  project repo: {PROJ}")


async def wait_idle(mgr, ids, timeout=180):
    """Poll until every agent has finished its current turn (idle/error/stopped)."""
    start = time.monotonic()
    done = {"idle", "error", "stopped"}
    while time.monotonic() - start < timeout:
        st = {i: mgr.status(i)["status"] for i in ids}
        print(f"  …{time.monotonic()-start:4.0f}s  {st}")
        if all(s in done for s in st.values()):
            return st
        await asyncio.sleep(3)
    return {i: mgr.status(i)["status"] for i in ids}


async def main():
    setup_project()
    mgr = AgentManager()

    banner("SPAWN — two agents, concurrently, in separate worktrees")
    a = await mgr.spawn(str(PROJ),
                        "Create a file calc.py with a function add(a, b) that returns a + b, "
                        "and a `if __name__ == '__main__':` block that prints add(2, 3).",
                        name="calc")
    b = await mgr.spawn(str(PROJ),
                        "Create a file NOTES.md containing exactly one line: demo project notes.",
                        name="notes")
    print(f"  spawned A={a['id']} ({a['branch']})  B={b['id']} ({b['branch']})")

    st = await wait_idle(mgr, [a["id"], b["id"]])
    print(f"  first turns complete: {st}")

    banner("INSTRUCT — follow-up to agent A (persistent session)")
    await mgr.instruct(a["id"], "Now also add a subtract(a, b) function to calc.py.")
    await wait_idle(mgr, [a["id"]])

    # ── verification (filesystem ground truth) ──
    banner("VERIFY")
    awt = Path(mgr.status(a["id"])["worktree"])
    bwt = Path(mgr.status(b["id"])["worktree"])
    calc = awt / "calc.py"
    notes = bwt / "NOTES.md"
    calc_txt = calc.read_text() if calc.exists() else ""

    checks = {
        "agent A wrote calc.py in its OWN worktree": calc.exists(),
        "calc.py has add()": "def add" in calc_txt,
        "follow-up instruct added subtract()": "def subtract" in calc_txt,
        "agent B wrote NOTES.md in its OWN worktree": notes.exists(),
        "isolation: A's calc.py NOT in B's worktree": not (bwt / "calc.py").exists(),
        "isolation: NOTES.md NOT in A's worktree": not (awt / "NOTES.md").exists(),
        "isolation: neither file in the main project dir": (
            not (PROJ / "calc.py").exists() and not (PROJ / "NOTES.md").exists()
        ),
        "agents on separate branches": a["branch"] != b["branch"],
        "cost tracked for both": mgr.status(a["id"])["cost_usd"] > 0
                                 and mgr.status(b["id"])["cost_usd"] > 0,
    }

    print(f"  A worktree : {awt}")
    print(f"  B worktree : {bwt}")
    print(f"  calc.py    : {[l for l in calc_txt.splitlines() if 'def ' in l]}")
    total = mgr.status(a["id"])["cost_usd"] + mgr.status(b["id"])["cost_usd"]
    print(f"  total cost : ${total:.4f}")
    print()
    for name, ok in checks.items():
        print(f"  [{PASS if ok else FAIL}] {name}")

    banner("CLEANUP — stop both agents (keep worktrees for inspection)")
    await mgr.stop(a["id"])
    await mgr.stop(b["id"])

    allok = all(checks.values())
    banner(f"PHASE 1.1 {'ALL GREEN — orchestrator core works' if allok else 'has FAILURES'}")
    return 0 if allok else 1


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
