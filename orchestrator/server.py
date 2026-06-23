"""
MCP stdio server exposing the AgentManager to Jarvis.

Jarvis (Rust) spawns this over stdio via its existing rmcp/MCP machinery — the
same pattern as the Google Calendar MCP — and the brain calls these tools
through tools::dispatch, so every spawn passes through the safety gate.

Run standalone:  uv run python server.py   (speaks MCP over stdio)
"""
from mcp.server.fastmcp import FastMCP

from manager import AgentManager

mcp = FastMCP("jarvis-orchestrator")
manager = AgentManager()


@mcp.tool()
async def spawn_dev_agent(task: str, project_dir: str = "", name: str = "") -> dict:
    """Spawn an autonomous Claude Code dev agent to work on `task` in an isolated
    git worktree. `project_dir` is OPTIONAL: pass an existing git repo's path to
    work inside it, OR a short project name (or leave it empty) to create a
    BRAND-NEW project under ~/claude/jarvis-workspace/projects. Returns the
    agent's id, branch, worktree, and status. The agent runs in the background —
    poll get_agent_status or list_dev_agents to follow it."""
    return await manager.spawn(project_dir, task, name or None)


@mcp.tool()
async def instruct_dev_agent(agent_id: str, instruction: str) -> dict:
    """Send a follow-up instruction to a running agent (its session is persistent,
    so it remembers everything it has done so far)."""
    return await manager.instruct(agent_id, instruction)


@mcp.tool()
def list_dev_agents() -> list:
    """List all dev agents with their status, branch, cost, and last activity."""
    return manager.list()


@mcp.tool()
def get_agent_status(agent_id: str) -> dict:
    """Detailed status for one agent: state, cost, turn count, and recent events
    (what it has been saying and which tools it has used)."""
    return manager.status(agent_id)


@mcp.tool()
async def stop_dev_agent(agent_id: str, remove_worktree: bool = False) -> dict:
    """Interrupt and stop an agent. Set remove_worktree=true to also delete its
    worktree (use only after its work has been reviewed/merged or abandoned)."""
    return await manager.stop(agent_id, remove_worktree)


@mcp.tool()
async def qa_web_app(target: str, spec: str) -> dict:
    """Visually QA a web app against a spec. Serves the app on an ephemeral local
    port, screenshots it at mobile + desktop, and judges it against `spec` with
    Claude vision. `target` is an agent_id (from spawn_dev_agent / list_dev_agents)
    whose worktree to check, OR an absolute path to the web app's project directory.
    Returns a PASS/NEEDS_WORK/FAIL verdict, per-criterion scores (layout, typography,
    color, responsiveness, spec_adherence), the top issues to fix, a spoken-friendly
    summary, and the screenshot paths. Read-only: it serves + screenshots, never
    modifies the project."""
    return await manager.qa_web_app(target, spec)


@mcp.tool()
async def start_qa_loop(target: str, spec: str, max_iters: int = 0) -> dict:
    """Run the closed self-correcting QA-AND-FIX loop on a LIVE dev agent: QA the
    app, and if it falls short of `spec`, critique the agent, let it fix the code,
    and re-QA — repeating until it PASSES (or it gets stuck / runs out of passes).
    Unlike qa_web_app (which only judges, read-only), this MUTATES: it drives the
    agent to change and commit code. Returns IMMEDIATELY ({started: true, ...}); the
    loop runs in the background and Jarvis narrates each verdict by voice as it goes.
    `target` MUST be a LIVE agent's id or name (a detached/past agent can't be
    instructed — spawn a fresh one first). `max_iters` caps the passes (0 = default,
    from JARVIS_QA_MAX_ITERS, normally 3)."""
    return await manager.start_qa_loop(target, spec, max_iters or None)


if __name__ == "__main__":
    mcp.run()  # stdio transport by default
