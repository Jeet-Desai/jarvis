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
import json
import logging
import os
import re
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

def _inside_git_repo(d: Path) -> bool:
    """True if d or any ancestor contains a .git (i.e. d is inside a git repo)."""
    return any((p / ".git").exists() for p in (d, *d.parents))


def _default_agents_root() -> Path:
    """Where agent worktrees live. MUST be outside any git repo — otherwise Claude
    Code walks up, finds an enclosing CLAUDE.md/repo, and writes to the wrong root
    (the Phase-0 spike bug). Override with the JARVIS_AGENTS_ROOT env var.

    Default: walk up from the jarvis repo to the nearest ancestor NOT inside any
    git repo, then use a dedicated, VISIBLE `jarvis-workspace/agents/` there — e.g.
    ~/claude/jarvis-workspace/agents (a sibling of the repo's parent, not hidden in
    $HOME). This is the single home for everything Jarvis does outside its repo."""
    env = os.environ.get("JARVIS_AGENTS_ROOT")
    if env:
        return Path(env).expanduser().resolve()
    d = Path(__file__).resolve().parents[1].parent  # parent of the jarvis repo
    while _inside_git_repo(d) and d != d.parent:
        d = d.parent
    return d / "jarvis-workspace" / "agents"


AGENTS_ROOT = _default_agents_root()
# Where brand-NEW projects (e.g. "build me a website") are created when the
# caller didn't point at an existing git repo. Sibling of AGENTS_ROOT, so
# everything Jarvis makes lives together under (by default) ~/claude/
# jarvis-workspace/. Override with JARVIS_PROJECTS_ROOT.
PROJECTS_ROOT = Path(
    os.environ.get("JARVIS_PROJECTS_ROOT", str(AGENTS_ROOT.parent / "projects"))
).expanduser()
# Default to a cheaper model than Opus for a fleet; caller can override per spawn.
DEFAULT_MODEL = "sonnet"
DEFAULT_ALLOWED_TOOLS = ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]

# Tailable activity log for dev agents (everything each agent says / every tool
# it runs / spawn / stop / error), so the build can be watched from outside the
# in-memory event deque: `tail -f ~/claude/jarvis-workspace/orchestrator.log`.
# Override with JARVIS_ORCH_LOG. A dedicated logger (not root/basicConfig) so it
# doesn't capture the MCP server's request spam.
LOG_PATH = Path(
    os.environ.get("JARVIS_ORCH_LOG", str(AGENTS_ROOT.parent / "orchestrator.log"))
).expanduser()
log = logging.getLogger("jarvis.orchestrator")
if not log.handlers:
    try:
        LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
        _h = logging.FileHandler(LOG_PATH)
        _h.setFormatter(logging.Formatter("%(asctime)s %(message)s"))
        log.addHandler(_h)
        log.setLevel(logging.INFO)
        log.propagate = False
    except Exception:  # logging must never break the orchestrator
        pass


def _run(args: list[str], cwd: Optional[Path] = None) -> subprocess.CompletedProcess:
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True)


def _slug(s: str) -> str:
    """Filesystem-safe lowercase project name from arbitrary text."""
    s = re.sub(r"[^A-Za-z0-9._-]+", "-", (s or "").strip()).strip("-_.")
    return s[:60].lower()


# HARD RULE: dev agents must NEVER touch the user's protected files. Mirror of
# the Rust policy guard (policy.rs `protected_path_guard`), enforced here too so a
# protected path is refused before any dir/repo is created. There are no
# built-in names; set them via JARVIS_PROTECTED_PATHS (comma-separated).
_PROTECTED_PATHS = [
    s.strip().lower() for s in os.environ.get("JARVIS_PROTECTED_PATHS", "").split(",") if s.strip()
]


def _assert_not_protected(*candidates: str) -> None:
    for c in candidates:
        lc = str(c or "").lower()
        hit = next((n for n in _PROTECTED_PATHS if n in lc), None)
        if hit:
            raise ValueError(
                f"Refused: dev agents must never work on protected files (matched '{hit}'). "
                "Hard rule in every mode — use a separate Claude Code session for those."
            )


def _commit_identity() -> tuple[str, str]:
    """The git identity agent projects commit as. PRIVACY: never fall through to
    the machine's GLOBAL git config, which may not be the identity you want —
    agent work must not be authored with that. Order: JARVIS_GIT_NAME/EMAIL env
    → the jarvis repo's own identity (the "same GitHub as the jarvis folder")
    → a neutral local placeholder."""
    n, e = os.environ.get("JARVIS_GIT_NAME"), os.environ.get("JARVIS_GIT_EMAIL")
    if n and e:
        return n, e
    repo = Path(__file__).resolve().parents[1]  # the jarvis repo
    n = _run(["git", "-C", str(repo), "config", "user.name"]).stdout.strip()
    e = _run(["git", "-C", str(repo), "config", "user.email"]).stdout.strip()
    if n and e:
        return n, e
    return "Jarvis", "jarvis@local"


def _git_init(p: Path) -> None:
    """Init a git repo with a neutral LOCAL identity (see _commit_identity)
    and one empty commit, so `git worktree add … HEAD` works and the agent's own
    later commits inherit that identity (local config overrides the global one)."""
    name, email = _commit_identity()
    _run(["git", "init"], cwd=p)
    _run(["git", "config", "user.name", name], cwd=p)
    _run(["git", "config", "user.email", email], cwd=p)
    _run(
        ["git", "-c", f"user.name={name}", "-c", f"user.email={email}",
         "commit", "--allow-empty", "-m", "Initial commit (Jarvis)"],
        cwd=p,
    )


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
    is_worktree: bool = True        # False = built directly in the project dir
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
            "created": round(self.created, 1),
            # An in-process Agent is always live (has a usable ClaudeSDKClient).
            # Records resurrected from the on-disk registry get live=False — the
            # SDK session can't be revived, so they're roster-only.
            "live": True,
        }


class AgentManager:
    def __init__(self, agents_root: Path = AGENTS_ROOT, default_model: str = DEFAULT_MODEL):
        self.agents_root = Path(agents_root)
        self.default_model = default_model
        self.agents: dict[str, Agent] = {}
        self.agents_root.mkdir(parents=True, exist_ok=True)
        # Persistence: a JSON roster written on spawn + every status change and
        # reloaded here, so a sidecar/app restart still shows past agents (their
        # last status + on-disk worktree) as detached (live=False). The live SDK
        # session itself can't be revived — this preserves the roster only.
        self.registry_path = Path(
            os.environ.get(
                "JARVIS_AGENTS_REGISTRY",
                str(self.agents_root.parent / "agents-registry.json"),
            )
        ).expanduser()
        self.registry: dict[str, dict] = self._load_registry()

    # ── lifecycle ────────────────────────────────────────────────────────────

    def _resolve_project(self, project_dir: str, fallback_name: str) -> tuple[Path, bool]:
        """Return (path, build_directly).

        - Existing git REPO → build_directly=False: isolate the agent in its own
          worktree+branch so the user's working tree is never touched.
        - A bare name / empty / an existing non-repo dir → build_directly=True:
          a project Jarvis owns, built IN PLACE (files land right in the project
          dir — no empty base repo + separate worktree split). New projects live
          under PROJECTS_ROOT (~/claude), so "build me a website" just works."""
        raw = (project_dir or "").strip()
        _assert_not_protected(raw, fallback_name)  # hard protected-paths rule
        looks_like_path = raw.startswith(("/", "~", ".")) or "/" in raw
        if raw and looks_like_path:
            p = Path(raw).expanduser().resolve()
            _assert_not_protected(str(p))
            if (p / ".git").exists():
                return p, False          # existing repo → worktree isolation
            if p.exists():
                _git_init(p)
                return p, True           # their dir, now a repo → build in place
        # New project: derive a name and create it under PROJECTS_ROOT.
        base = raw if (raw and not looks_like_path) else fallback_name
        proj = PROJECTS_ROOT / (_slug(base) or f"project-{uuid.uuid4().hex[:6]}")
        proj.mkdir(parents=True, exist_ok=True)
        if not (proj / ".git").exists():
            _git_init(proj)
        return proj, True                # brand-new → build in place

    async def spawn(
        self,
        project_dir: str,
        task: str,
        name: Optional[str] = None,
        model: Optional[str] = None,
    ) -> dict:
        """Create an isolated worktree and launch an agent on `task`. Returns at once.

        `project_dir` may be an existing git repo, OR a short name / empty — in
        which case a fresh project is created under PROJECTS_ROOT (~/claude)."""
        agent_id = uuid.uuid4().hex[:8]
        # Name a brand-new project from an explicit name, else from the task.
        fallback = name or _slug(" ".join(task.split()[:5])) or f"project-{agent_id}"
        project, build_directly = self._resolve_project(project_dir, fallback)
        name = name or f"agent-{agent_id}"

        if build_directly:
            # New/owned project → agent works straight in the project dir. One
            # obvious location; no empty base repo + separate worktree.
            worktree = project
            cur = _run(["git", "branch", "--show-current"], cwd=project)
            branch = cur.stdout.strip() or "main"
            is_worktree = False
        else:
            # Existing repo → isolate on its own branch in a worktree.
            branch = f"agent/{agent_id}"
            worktree = self.agents_root / project.name / agent_id
            worktree.parent.mkdir(parents=True, exist_ok=True)
            r = _run(["git", "worktree", "add", "-b", branch, str(worktree), "HEAD"], cwd=project)
            if r.returncode != 0:
                raise RuntimeError(f"git worktree add failed: {r.stderr.strip()}")
            is_worktree = True

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
                    f"Your ENTIRE workspace is {worktree}. Always create, read, and edit files "
                    f"INSIDE {worktree}; never write outside it. HARD RULE: never read, write, "
                    "cd into, or run commands against the user's protected files — anything "
                    "matching the configured protected markers is strictly off-limits. "
                    "When you finish, run `git add -A && git commit -m \"<short summary>\"` so your "
                    "work is saved to git (never leave it as untracked files), then end with a "
                    "one-line summary of what you changed."
                ),
            },
        )

        client = ClaudeSDKClient(options=options)
        await client.connect()

        agent = Agent(
            id=agent_id, name=name, project_dir=project, worktree=worktree,
            branch=branch, client=client, model=model, is_worktree=is_worktree,
        )
        self.agents[agent_id] = agent
        agent.drain_task = asyncio.create_task(self._drain(agent))
        log.info("[%s/%s] SPAWN     | project=%s worktree=%s model=%s",
                 name, agent_id, project, worktree, model)
        self._event(agent, "spawn", task)
        self._persist(agent)  # roster entry exists the moment the agent is born

        await client.query(task)
        self._set_status(agent, "working")
        return agent.summary()

    async def instruct(self, agent_id: str, instruction: str) -> dict:
        """Send a follow-up instruction to a live agent (same persistent session)."""
        agent = self._get(agent_id)
        self._event(agent, "instruct", instruction)
        await agent.client.query(instruction)
        self._set_status(agent, "working")
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
        self._set_status(agent, "stopped")
        self._event(agent, "stop", "stopped by orchestrator")
        # Only a real worktree can be `git worktree remove`d. For a directly-built
        # project the worktree IS the project dir — don't delete the user's work.
        if remove_worktree and agent.is_worktree:
            _run(["git", "worktree", "remove", "--force", str(agent.worktree)],
                 cwd=agent.project_dir)
        return agent.summary()

    # ── reads (sync; cheap control-plane queries) ────────────────────────────

    def list(self) -> list[dict]:
        """Live agents (this process) merged with detached history from the
        registry, newest first. Live agents override their persisted record."""
        merged: dict[str, dict] = dict(self.registry)  # detached, live=False
        for aid, a in self.agents.items():
            merged[aid] = a.summary()                   # live, live=True
        return sorted(merged.values(), key=lambda r: r.get("created", 0), reverse=True)

    def status(self, agent_id: str) -> dict:
        if agent_id in self.agents:
            agent = self.agents[agent_id]
            s = agent.summary()
            s["events"] = list(agent.events)[-15:]
            return s
        # Detached agent from a previous run: serve its last-known record. Its
        # live event deque is gone, so events is empty.
        if agent_id in self.registry:
            rec = dict(self.registry[agent_id])
            rec.setdefault("events", [])
            return rec
        raise KeyError(f"no agent with id {agent_id!r}")

    # ── internals ────────────────────────────────────────────────────────────

    async def _drain(self, agent: Agent) -> None:
        """Consume the agent's message stream forever → status / cost / events."""
        try:
            async for msg in agent.client.receive_messages():
                if isinstance(msg, AssistantMessage):
                    for b in msg.content:
                        if isinstance(b, TextBlock) and b.text.strip():
                            agent.last_text = b.text.strip()
                            self._event(agent, "say", b.text.strip())
                        elif isinstance(b, ToolUseBlock):
                            self._event(agent, "tool", _tool_brief(b))
                    self._set_status(agent, "working")
                elif isinstance(msg, ResultMessage):
                    agent.turns += 1
                    if msg.total_cost_usd:
                        agent.cost_usd += msg.total_cost_usd
                    self._event(
                        agent, "turn_done",
                        f"turn {agent.turns} done (cost=${msg.total_cost_usd}, error={msg.is_error})",
                    )
                    # Persist AFTER turns/cost/last_text are updated so the
                    # registry snapshot captures the finished turn.
                    self._set_status(agent, "error" if msg.is_error else "idle")
        except asyncio.CancelledError:
            raise
        except Exception as e:  # noqa: BLE001 - a dead stream must mark the agent, not crash the server
            self._set_status(agent, "error")
            self._event(agent, "error", repr(e))

    def _event(self, agent: Agent, kind: str, text: str) -> None:
        agent.events.append({"t": round(time.time(), 1), "kind": kind, "text": text[:240]})
        # Mirror to the tailable log so the build is observable live.
        log.info("[%s/%s] %-9s | %s", agent.name, agent.id, kind, text[:240].replace("\n", " "))

    def _get(self, agent_id: str) -> Agent:
        if agent_id not in self.agents:
            raise KeyError(f"no agent with id {agent_id!r}")
        return self.agents[agent_id]

    # ── persistence (the on-disk roster) ─────────────────────────────────────

    def _set_status(self, agent: Agent, status: str) -> None:
        """Set status and persist the registry on any actual change — the single
        choke point for status writes, so 'persist on every status change' holds
        without scattering registry writes through the lifecycle methods.

        Persist ONLY on a real transition: the drain loop calls this with
        "working" on every assistant message, and we don't want one disk write
        per message. The turn-completion transition (→idle/→error) is itself a
        change, and by then turns/cost/last_text are already updated, so the
        persisted snapshot still captures the finished turn."""
        if agent.status != status:
            agent.status = status
            self._persist(agent)

    def _snapshot(self, agent: Agent) -> dict:
        """Full persistable record: the HUD summary plus the fields needed to
        recall a detached agent later (where its project/worktree lives)."""
        s = agent.summary()
        s["project_dir"] = str(agent.project_dir)
        s["is_worktree"] = agent.is_worktree
        return s

    def _persist(self, agent: Agent) -> None:
        self.registry[agent.id] = self._snapshot(agent)
        self._write_registry()

    def _write_registry(self) -> None:
        """Atomically rewrite the whole roster. Best-effort: a persistence
        failure must never break an agent (mirrors the logging guard above)."""
        try:
            self.registry_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.registry_path.with_name(self.registry_path.name + ".tmp")
            tmp.write_text(json.dumps(self.registry, indent=2))
            tmp.replace(self.registry_path)  # atomic on POSIX
        except Exception:  # noqa: BLE001
            log.warning("could not write agents registry", exc_info=True)

    def _load_registry(self) -> dict[str, dict]:
        """Load the roster from disk, marking every record detached (live=False)
        — the SDK sessions died with the previous process. A transient status
        (spawning/working) is downgraded to idle, since nothing in THIS process
        is draining that agent; otherwise the HUD would pulse 'working' forever."""
        try:
            if not self.registry_path.exists():
                return {}
            data = json.loads(self.registry_path.read_text())
            if not isinstance(data, dict):
                return {}
            out: dict[str, dict] = {}
            for aid, rec in data.items():
                if not isinstance(rec, dict):
                    continue
                rec = dict(rec)
                rec["live"] = False
                if rec.get("status") in ("spawning", "working"):
                    rec["status"] = "idle"
                out[str(aid)] = rec
            return out
        except Exception:  # noqa: BLE001 - a corrupt registry must not stop startup
            log.warning("could not load agents registry; starting fresh", exc_info=True)
            return {}
