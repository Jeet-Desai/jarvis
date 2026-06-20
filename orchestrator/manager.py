"""
AgentManager — spawn and supervise Claude Code dev agents in isolated git worktrees.

This is the core of the Jarvis dev-orchestrator. It is deliberately free of any
MCP / Jarvis coupling so it can be tested standalone (see test_manager.py); the
MCP stdio surface that Jarvis talks to is a thin wrapper in server.py.

Each agent:
  - gets its own git WORKTREE on its own branch (parallel-safe, reviewable, revertible)
  - runs with permission_mode="acceptEdits" (file edits auto-apply, shell whitelisted)
  - is a persistent ClaudeSDKClient so it can be instructed repeatedly over time
  - is drained by a background task that tracks status, cost, and recent activity

Hard-won rules baked in (from the Phase-0 spike):
  - worktrees live OUTSIDE any enclosing repo (AGENTS_ROOT in ~/.jarvis/agents)
  - setting_sources=[] so no parent CLAUDE.md/settings bleed into the agent
  - the workspace path is pinned via an appended system prompt, because Claude
    Code's Write tool otherwise resolves bare relative paths to $HOME
"""
from __future__ import annotations

import asyncio
import subprocess
import time
import uuid
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from claude_agent_sdk import (
    ClaudeSDKClient,
    ClaudeAgentOptions,
    AssistantMessage,
    ResultMessage,
    TextBlock,
    ToolUseBlock,
)

# Agent worktrees live here — a dedicated area OUTSIDE any project/jarvis repo.
AGENTS_ROOT = Path.home() / ".jarvis" / "agents"
# Default to a cheaper model than Opus for a fleet; caller can override per spawn.
DEFAULT_MODEL = "sonnet"
DEFAULT_ALLOWED_TOOLS = ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]


def _run(args: list[str], cwd: Optional[Path] = None) -> subprocess.CompletedProcess:
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True)


def _tool_brief(block: ToolUseBlock) -> str:
    """One-line, human/voice-friendly description of a tool call."""
    name = getattr(block, "name", "tool")
    inp = getattr(block, "input", {}) or {}
    for k in ("command", "file_path", "path", "pattern", "url"):
        if k in inp:
            return f"{name}({k}={str(inp[k])[:80]})"
    return name


@dataclass
class Agent:
    id: str
    name: str
    project_dir: Path
    worktree: Path
    branch: str
    client: ClaudeSDKClient
    model: str
    status: str = "spawning"        # spawning | working | idle | stopped | error
    cost_usd: float = 0.0
    turns: int = 0
    last_text: str = ""
    events: deque = field(default_factory=lambda: deque(maxlen=50))
    drain_task: Optional[asyncio.Task] = None
    created: float = field(default_factory=time.time)

    def summary(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "status": self.status,
            "branch": self.branch,
            "worktree": str(self.worktree),
            "model": self.model,
            "cost_usd": round(self.cost_usd, 4),
            "turns": self.turns,
            "last": self.last_text[:200],
        }


class AgentManager:
    def __init__(self, agents_root: Path = AGENTS_ROOT, default_model: str = DEFAULT_MODEL):
        self.agents_root = Path(agents_root)
        self.default_model = default_model
        self.agents: dict[str, Agent] = {}
        self.agents_root.mkdir(parents=True, exist_ok=True)

    # ── lifecycle ────────────────────────────────────────────────────────────

    async def spawn(
        self,
        project_dir: str,
        task: str,
        name: Optional[str] = None,
        model: Optional[str] = None,
    ) -> dict:
        """Create an isolated worktree and launch an agent on `task`. Returns at once."""
        project = Path(project_dir).expanduser().resolve()
        if not (project / ".git").exists():
            raise ValueError(
                f"{project} is not a git repository — worktree isolation needs one."
            )

        agent_id = uuid.uuid4().hex[:8]
        name = name or f"agent-{agent_id}"
        branch = f"agent/{agent_id}"
        worktree = self.agents_root / project.name / agent_id
        worktree.parent.mkdir(parents=True, exist_ok=True)

        r = _run(["git", "worktree", "add", "-b", branch, str(worktree), "HEAD"], cwd=project)
        if r.returncode != 0:
            raise RuntimeError(f"git worktree add failed: {r.stderr.strip()}")

        model = model or self.default_model
        options = ClaudeAgentOptions(
            cwd=str(worktree),
            add_dirs=[str(worktree)],
            permission_mode="acceptEdits",
            allowed_tools=DEFAULT_ALLOWED_TOOLS,
            model=model,
            setting_sources=[],  # no external CLAUDE.md/settings bleed
            system_prompt={
                "type": "preset",
                "preset": "claude_code",
                "append": (
                    "You are an autonomous software-development agent supervised by Jarvis. "
                    f"Your ENTIRE workspace is the git worktree at {worktree}. Always create, "
                    f"read, and edit files INSIDE {worktree}; never write outside it. When you "
                    "finish a task, end with a one-line summary of what you changed."
                ),
            },
        )

        client = ClaudeSDKClient(options=options)
        await client.connect()

        agent = Agent(
            id=agent_id, name=name, project_dir=project, worktree=worktree,
            branch=branch, client=client, model=model,
        )
        self.agents[agent_id] = agent
        agent.drain_task = asyncio.create_task(self._drain(agent))
        self._event(agent, "spawn", task)

        await client.query(task)
        agent.status = "working"
        return agent.summary()

    async def instruct(self, agent_id: str, instruction: str) -> dict:
        """Send a follow-up instruction to a live agent (same persistent session)."""
        agent = self._get(agent_id)
        self._event(agent, "instruct", instruction)
        await agent.client.query(instruction)
        agent.status = "working"
        return agent.summary()

    async def stop(self, agent_id: str, remove_worktree: bool = False) -> dict:
        """Interrupt + disconnect an agent. Optionally remove its worktree."""
        agent = self._get(agent_id)
        try:
            await agent.client.interrupt()
        except Exception:
            pass
        if agent.drain_task:
            agent.drain_task.cancel()
        try:
            await agent.client.disconnect()
        except Exception:
            pass
        agent.status = "stopped"
        self._event(agent, "stop", "stopped by orchestrator")
        if remove_worktree:
            _run(["git", "worktree", "remove", "--force", str(agent.worktree)],
                 cwd=agent.project_dir)
        return agent.summary()

    # ── reads (sync; cheap control-plane queries) ────────────────────────────

    def list(self) -> list[dict]:
        return [a.summary() for a in self.agents.values()]

    def status(self, agent_id: str) -> dict:
        agent = self._get(agent_id)
        s = agent.summary()
        s["events"] = list(agent.events)[-15:]
        return s

    # ── internals ────────────────────────────────────────────────────────────

    async def _drain(self, agent: Agent) -> None:
        """Consume the agent's message stream forever → status / cost / events."""
        try:
            async for msg in agent.client.receive_messages():
                if isinstance(msg, AssistantMessage):
                    agent.status = "working"
                    for b in msg.content:
                        if isinstance(b, TextBlock) and b.text.strip():
                            agent.last_text = b.text.strip()
                            self._event(agent, "say", b.text.strip())
                        elif isinstance(b, ToolUseBlock):
                            self._event(agent, "tool", _tool_brief(b))
                elif isinstance(msg, ResultMessage):
                    agent.turns += 1
                    if msg.total_cost_usd:
                        agent.cost_usd += msg.total_cost_usd
                    agent.status = "error" if msg.is_error else "idle"
                    self._event(
                        agent, "turn_done",
                        f"turn {agent.turns} done (cost=${msg.total_cost_usd}, error={msg.is_error})",
                    )
        except asyncio.CancelledError:
            raise
        except Exception as e:  # noqa: BLE001 - a dead stream must mark the agent, not crash the server
            agent.status = "error"
            self._event(agent, "error", repr(e))

    def _event(self, agent: Agent, kind: str, text: str) -> None:
        agent.events.append({"t": round(time.time(), 1), "kind": kind, "text": text[:240]})

    def _get(self, agent_id: str) -> Agent:
        if agent_id not in self.agents:
            raise KeyError(f"no agent with id {agent_id!r}")
        return self.agents[agent_id]
