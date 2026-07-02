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

# Closed-loop visual QA (P3): serve a built web app, screenshot it, judge it against
# a spec with Claude. qa.py is import-light at module load (playwright + the SDK are
# imported lazily inside its functions), so this top-level import is cheap.
from qa import serve_app, capture, judge

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


def _spoken_verdict(name: str, verdict: dict, cap: dict) -> str:
    """A short, TTS-friendly sentence for the QA verdict — what Jarvis says aloud."""
    v = verdict.get("verdict", "NEEDS_WORK")
    fixes = verdict.get("fixes") or []
    errs = list(cap.get("console_errors") or []) + list(cap.get("page_errors") or [])
    if v == "PASS":
        s = f"{name} passes visual QA against your spec."
        if fixes:
            s += f" One thing you could still polish: {fixes[0]}"
        return s
    head = "needs some work" if v == "NEEDS_WORK" else "fails visual QA"
    s = f"{name} {head}."
    if fixes:
        s += f" Top issue: {fixes[0]}"
        if len(fixes) > 1:
            s += f" {len(fixes)} issues in total."
    if errs:
        s += f" I also caught {len(errs)} console error{'s' if len(errs) != 1 else ''}."
    return s


def _qa_result(worktree: Path, cap: dict, verdict: dict) -> dict:
    """Assemble the spoken-friendly QA result the brain returns to the user. Kept
    compact (no base64, no raw judge text) since it flows back into the LLM context;
    full screenshots live on disk at `shot_dir`."""
    fixes = verdict.get("fixes") or []
    return {
        "verdict": verdict.get("verdict"),
        "scores": verdict.get("scores", {}),
        "summary": verdict.get("summary", ""),
        "spoken": _spoken_verdict(worktree.name, verdict, cap),
        "top_issues": fixes[:3],
        "fixes": fixes,
        "screenshots": [s["path"] for s in cap.get("shots", [])],
        "shot_dir": cap.get("out_dir"),
        "console_errors": cap.get("console_errors", []),
        "page_errors": cap.get("page_errors", []),
        "target": str(worktree),
        "judge_model": verdict.get("model"),
    }


# ── closed self-correcting QA loop (P3b) — pure, unit-testable helpers ─────────

# Jaccard similarity between two consecutive fix-sets at/above which we declare the
# agent is "stuck" (re-reporting the same problems). 0.8 tolerates rewording while
# catching genuine no-progress. Override is intentionally NOT exposed — it's a tuned
# control-flow constant, not a user knob.
_NO_PROGRESS_SIM = 0.8

# Generic filler stripped from fix text before comparison, so the issue-set similarity
# keys off the NOUNS (hero, headline, button, contrast, padding) rather than the
# boilerplate verbs/qualifiers a judge wraps them in. Short words (≤3 chars: "the",
# "add", "fix", …) are already dropped by length, so this only lists the 4+ char filler.
_FIX_STOPWORDS = frozenset({
    "make", "ensure", "improve", "increase", "reduce", "adjust", "update", "change",
    "should", "would", "could", "needs", "need", "more", "less", "with", "this",
    "that", "from", "into", "your", "also", "must", "very", "some", "such", "than",
    "then", "them", "they", "have", "here", "when", "where", "which", "while",
    "consider", "currently", "instead", "overall", "better", "properly", "across",
})


def _issue_words(fixes) -> set:
    """The bag of meaningful words across a fix list — lowercased, alphanumeric,
    >3 chars, minus generic filler. Used to compare one pass's issues against the
    next so that reworded-but-identical feedback still reads as 'no progress'."""
    words: set[str] = set()
    for fix in (fixes or []):
        for tok in re.findall(r"[a-z0-9]+", str(fix).lower()):
            if len(tok) > 3 and tok not in _FIX_STOPWORDS:
                words.add(tok)
    return words


def _jaccard(a: set, b: set) -> float:
    """Set similarity |a∩b| / |a∪b|. Two empty sets are identical (1.0); one empty
    and one non-empty share nothing (0.0)."""
    if not a and not b:
        return 1.0
    if not a or not b:
        return 0.0
    union = len(a | b)
    return len(a & b) / union if union else 1.0


def _build_critique(spec: str, verdict: dict, pass_num: int) -> str:
    """The work order sent back to the dev agent after a NEEDS_WORK/FAIL judge:
    the spec, the verdict + summary, the weakest rubric areas (<4/5), and a NUMBERED
    fix list — phrased as 'change the code in your worktree, then commit'."""
    fixes = verdict.get("fixes") or []
    scores = verdict.get("scores") or {}
    summary = (verdict.get("summary") or "").strip()
    weak = [k for k, v in sorted(scores.items()) if isinstance(v, (int, float)) and v < 4]
    numbered = "\n".join(f"{i}. {f}" for i, f in enumerate(fixes, 1)) or (
        "1. (no specific fixes listed — raise overall visual quality and spec adherence)"
    )
    parts = [
        f"Visual QA (pass {pass_num}) reviewed your work and the verdict is "
        f"{verdict.get('verdict', 'NEEDS_WORK')} — it does not yet meet the spec.",
        f'\nThe spec it is judged against:\n"""\n{(spec or "").strip()}\n"""',
    ]
    if summary:
        parts.append(f"\nReviewer summary: {summary}")
    if weak:
        parts.append(f"\nWeakest areas (scored below 4/5): {', '.join(weak)}.")
    parts.append(f"\nAddress these, most important first:\n{numbered}")
    parts.append(
        "\nMake the ACTUAL code changes in your worktree to fix every point above, "
        "keeping the rest of the page intact. When done, run "
        '`git add -A && git commit -m "QA fixes"` so the changes are saved to git, '
        "then end with a one-line summary of what you changed."
    )
    return "\n".join(parts)


def _qa_max_iters() -> int:
    """Default number of QA-fix passes. Override JARVIS_QA_MAX_ITERS (min 1)."""
    try:
        return max(1, int(os.environ.get("JARVIS_QA_MAX_ITERS", "3")))
    except ValueError:
        return 3


def _qa_fix_timeout() -> float:
    """Seconds to wait for one agent fix turn. Override JARVIS_QA_FIX_TIMEOUT."""
    try:
        return max(1.0, float(os.environ.get("JARVIS_QA_FIX_TIMEOUT", "240")))
    except ValueError:
        return 240.0


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
    events: deque = field(default_factory=lambda: deque(maxlen=200))
    drain_task: Optional[asyncio.Task] = None
    created: float = field(default_factory=time.time)
    # ── closed-loop QA (P3b) ──
    # Set by the drain loop when a turn's ResultMessage arrives, so the QA-fix
    # loop can await "the agent finished the fix I asked for". Cleared before each
    # instruct. asyncio.Event() binds to no running loop at construction (3.10+),
    # so it is safe as a dataclass default_factory.
    turn_done: asyncio.Event = field(default_factory=asyncio.Event)
    # The current spoken line + a monotonic sequence. The Rust poll thread (P2)
    # speaks `announce` once each time `announce_seq` increases past what it saw.
    announce: Optional[str] = None
    announce_seq: int = 0
    # True while a _qa_fix_loop task is driving this agent. The voice layer uses it
    # to SUPPRESS the generic "finished its turn"/"ran into an error" lines during a
    # loop — the loop's own `announce` lines carry the narrative instead.
    qa_active: bool = False

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
            # Spoken-update channel for the QA-fix loop (P3b): the latest line and
            # its sequence, so the voice layer narrates each verdict exactly once;
            # qa_active suppresses the generic status lines while the loop is running.
            "announce": self.announce,
            "announce_seq": self.announce_seq,
            "qa_active": self.qa_active,
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

    # ── closed-loop visual QA (P3) ───────────────────────────────────────────

    def _worktree_by_name(self, name: str) -> Optional[Path]:
        """Find an agent's worktree by its friendly NAME (live agents first, then
        detached registry). Voice-friendly: when the user says 'QA the
        landing-page-builder agent', the brain may pass that name rather than the
        opaque 8-hex id."""
        key = name.strip().lower()
        for a in self.agents.values():
            if a.name.lower() == key:
                return Path(a.worktree)
        for r in self.registry.values():
            if str(r.get("name", "")).lower() == key and r.get("worktree"):
                return Path(r["worktree"])
        return None

    def _resolve_qa_worktree(self, target: str) -> Path:
        """Resolve a qa_web_app `target` to a directory to serve. `target` may be a
        known agent_id (live OR detached → its worktree), an agent NAME, or a
        filesystem path — tried in that order. The protected-paths hard rule
        applies, same as spawn."""
        raw = (target or "").strip()
        if not raw:
            raise ValueError("qa_web_app needs an agent_id, agent name, or project directory path")
        _assert_not_protected(raw)
        if raw in self.agents:
            wt = Path(self.agents[raw].worktree)            # 1) live agent_id
        elif raw in self.registry and self.registry[raw].get("worktree"):
            wt = Path(self.registry[raw]["worktree"])       # 2) detached agent_id
        else:
            by_name = self._worktree_by_name(raw)           # 3) agent name
            wt = by_name if by_name is not None else Path(raw).expanduser()  # 4) path
        wt = wt.resolve()
        _assert_not_protected(str(wt))
        if not wt.is_dir():
            raise FileNotFoundError(
                f"QA target not found (not a known agent id/name, and not a directory): {wt}"
            )
        return wt

    async def qa_web_app(self, target: str, spec: str, build: bool = True) -> dict:
        """Visually QA a web app a dev agent built: serve it on an ephemeral local
        port, screenshot it at mobile + desktop, and judge it against `spec` with
        Claude. `target` is an agent_id or a project-dir path. Returns a
        spoken-friendly PASS/NEEDS_WORK/FAIL verdict with scores, top issues, and the
        screenshot paths. Read-only: it only serves + screenshots, never modifies."""
        worktree = self._resolve_qa_worktree(target)
        log.info("[qa] START     | target=%s worktree=%s spec=%.80s",
                 target, worktree, " ".join((spec or "").split()))
        try:
            with serve_app(worktree, build=build) as url:
                cap = await capture(url, spec)
                verdict = await judge(
                    spec, cap["shots"], cap.get("console_errors"), cap.get("page_errors")
                )
        except Exception as e:  # noqa: BLE001 - surface a readable error to the brain
            log.warning("[qa] ERROR     | target=%s: %r", target, e)
            raise
        result = _qa_result(worktree, cap, verdict)
        log.info("[qa] DONE      | verdict=%s scores=%s shots=%s",
                 result["verdict"], result["scores"], result["shot_dir"])
        return result

    def _resolve_live_agent(self, target: str) -> Agent:
        """Resolve a `start_qa_loop` target to a LIVE in-process Agent (one with a
        usable ClaudeSDKClient). Unlike `_resolve_qa_worktree`, a detached registry
        record is NOT enough — the loop INSTRUCTS the agent to fix issues, which
        needs a live SDK session. Accepts a live agent_id or a live agent NAME."""
        raw = (target or "").strip()
        if not raw:
            raise ValueError("start_qa_loop needs a live agent_id or agent name")
        _assert_not_protected(raw)  # hard protected-paths rule
        if raw in self.agents:
            return self.agents[raw]                       # 1) live agent_id
        key = raw.lower()
        for a in self.agents.values():
            if a.name.lower() == key:
                return a                                  # 2) live agent name
        # A roster entry exists but has no live session — explain why it can't loop.
        detached = raw in self.registry or any(
            str(r.get("name", "")).lower() == key for r in self.registry.values()
        )
        if detached:
            raise ValueError(
                f"Agent {raw!r} is in the roster but DETACHED (no live session). The "
                "QA-and-fix loop has to instruct it to make changes, which needs a live "
                "agent — re-spawn it, then run the loop against the new agent."
            )
        raise KeyError(f"no live agent with id or name {raw!r}")

    async def start_qa_loop(self, target: str, spec: str, max_iters: Optional[int] = None) -> dict:
        """Kick off the closed self-correcting QA loop on a LIVE dev agent and return
        IMMEDIATELY. The loop (serve → screenshot → judge → critique → re-judge until
        PASS/stuck/exhausted) runs minutes as a background task, so it must not block
        the MCP call; the proactive voice layer reports each verdict via `announce`.

        The brain's only jobs are to KICK THIS OFF with one call and let the voice
        narrate — all control flow lives here, never in a brain-driven tool dance."""
        agent = self._resolve_live_agent(target)
        _assert_not_protected(str(agent.worktree))
        iters = _qa_max_iters() if max_iters is None else max(1, int(max_iters))
        turn_timeout = _qa_fix_timeout()
        log.info("[qa-loop] START     | agent=%s/%s max_iters=%d", agent.name, agent.id, iters)
        agent.qa_active = True  # suppress generic status narration while the loop runs
        self._event(agent, "qa_loop", f"starting QA-and-fix loop (max_iters={iters})")
        self._persist(agent)
        asyncio.create_task(self._qa_fix_loop(agent, spec, iters, turn_timeout))
        return {
            "started": True,
            "agent_id": agent.id,
            "name": agent.name,
            "max_iters": iters,
            "message": (
                f"Running QA-and-fix on {agent.name} (up to {iters} passes). "
                "I'll tell you the outcome as it goes."
            ),
        }

    async def _qa_fix_loop(
        self, agent: Agent, spec: str, max_iters: int, turn_timeout: float
    ) -> dict:
        """The loop body (operates on a resolved LIVE agent). Each iteration serves +
        screenshots + judges the worktree; on PASS it stops, otherwise it critiques the
        agent and awaits its fix turn before re-judging. Always terminates via one of
        the stop conditions — never loops forever, never silently gives up."""
        history: list[dict] = []
        spoken: list[str] = []
        prev_words: Optional[set] = None
        last_cap: Optional[dict] = None
        last_verdict: Optional[dict] = None
        passed = False
        stop_reason = "exhausted"
        i = 0

        def say(text: str) -> None:
            self._announce(agent, text)
            spoken.append(text)

        try:
            # The brain often fires spawn_dev_agent then start_qa_loop in the SAME
            # turn, so the agent may still be mid-build with an empty worktree. Wait
            # for that in-flight build turn to finish before the first QA pass
            # (mirrors the manual wait used in the Stage-2 validation). Clearing
            # first means we await THIS turn, not a stale completion; the drain sets
            # status→idle BEFORE turn_done, so a working status implies the event is
            # not yet set for the current turn → no lost-wakeup race.
            if agent.status == "working":
                self._event(agent, "qa_loop", "waiting for the build to finish before first QA")
                log.info("[qa-loop] WAIT      | agent=%s waiting for in-flight build turn", agent.id)
                agent.turn_done.clear()
                try:
                    await asyncio.wait_for(agent.turn_done.wait(), turn_timeout)
                except asyncio.TimeoutError:
                    say(f"{agent.name} is still building after a while, so I haven't QA'd it yet. "
                        "Want to take a look?")
                    stop_reason = "build_timeout"
            if stop_reason != "build_timeout" and agent.status == "error":
                say(f"{agent.name} hit an error while building, so there's nothing to QA yet. "
                    "Want to take a look?")
                stop_reason = "agent_error"

            # Skip QA entirely if the build never produced anything usable.
            passes = range(0) if stop_reason in ("build_timeout", "agent_error") else range(1, max_iters + 1)
            for i in passes:
                # serve → capture → judge (reuse the P3a one-shot pieces).
                try:
                    with serve_app(agent.worktree) as url:
                        cap = await capture(url, spec)
                        verdict = await judge(
                            spec, cap["shots"], cap.get("console_errors"), cap.get("page_errors")
                        )
                except Exception as e:  # noqa: BLE001 - a serve/capture/judge failure stops honestly
                    log.warning("[qa-loop] ERROR     | agent=%s pass=%d: %r", agent.id, i, e)
                    say(f"I hit an error QA-ing {agent.name}: {e}. Want to take a look?")
                    stop_reason = "qa_error"
                    break

                last_cap, last_verdict = cap, verdict
                v = verdict.get("verdict", "NEEDS_WORK")
                scores = verdict.get("scores", {})
                fixes = verdict.get("fixes") or []
                history.append({"pass": i, "verdict": v, "scores": scores, "fixes": fixes})
                self._event(agent, "qa_loop", f"pass {i}: {v} scores={scores}")
                log.info("[qa-loop] pass %d    | agent=%s verdict=%s scores=%s", i, agent.id, v, scores)

                if v == "PASS":
                    say(f"{agent.name} passed visual QA on pass {i}.")
                    passed, stop_reason = True, "passed"
                    break

                # No progress: this pass's issues ~match the previous pass's → stop honestly.
                words = _issue_words(fixes)
                if prev_words is not None and _jaccard(prev_words, words) >= _NO_PROGRESS_SIM:
                    say(
                        f"{agent.name} isn't making progress — same QA issues after pass {i}. "
                        "Want to take a look?"
                    )
                    stop_reason = "stuck"
                    break
                prev_words = words

                # Out of passes.
                if i == max_iters:
                    top = fixes[0] if fixes else "it still needs polish"
                    say(
                        f"I've QA'd {agent.name} {max_iters} times and it still needs work — "
                        f"{top}. Want to take a look?"
                    )
                    stop_reason = "exhausted"
                    break

                # Otherwise instruct the agent to fix the issues and await its turn.
                say(f"Pass {i}: {agent.name} still needs work — sending it the fixes.")
                agent.turn_done.clear()
                await agent.client.query(_build_critique(spec, verdict, i))
                self._set_status(agent, "working")
                try:
                    await asyncio.wait_for(agent.turn_done.wait(), turn_timeout)
                except asyncio.TimeoutError:
                    say(f"{agent.name}'s fix took too long, so I stopped. Want to take a look?")
                    stop_reason = "fix_timeout"
                    break
                if agent.status == "error":
                    say(f"{agent.name} hit an error while fixing the QA issues. Want to take a look?")
                    stop_reason = "agent_error"
                    break
        finally:
            # The loop is no longer driving this agent — let generic status narration
            # resume, and never leave qa_active stuck on if something unexpected
            # propagated out of the loop.
            agent.qa_active = False
            self._persist(agent)

        result = {
            "agent_id": agent.id,
            "name": agent.name,
            "passed": passed,
            "stop_reason": stop_reason,
            "iterations": i,
            "final": (
                _qa_result(agent.worktree, last_cap, last_verdict)
                if last_cap is not None and last_verdict is not None
                else None
            ),
            "history": history,
            "spoken": spoken,
        }
        log.info("[qa-loop] DONE      | agent=%s passed=%s stop=%s iters=%d",
                 agent.id, passed, stop_reason, i)
        self._event(agent, "qa_loop", f"loop done: passed={passed} stop={stop_reason} iters={i}")
        return result

    # ── reads (sync; cheap control-plane queries) ────────────────────────────

    def list(self) -> list[dict]:
        """Live agents (this process) merged with detached history from the
        registry, newest first. Live agents override their persisted record."""
        merged: dict[str, dict] = dict(self.registry)  # detached, live=False
        for aid, a in self.agents.items():
            merged[aid] = a.summary()                   # live, live=True
        return sorted(merged.values(), key=lambda r: r.get("created", 0), reverse=True)

    def status(self, agent_id: str, full: bool = False) -> dict:
        # Two consumers, two budgets: the BRAIN gets a compact view (15 events,
        # 240 chars each — it rides into the LLM context), while the HUD detail
        # panel asks full=True for real history (100 events, 2000 chars).
        n, cap = (100, 2000) if full else (15, 240)
        if agent_id in self.agents:
            agent = self.agents[agent_id]
            s = agent.summary()
            s["events"] = [{**e, "text": e["text"][:cap]} for e in list(agent.events)[-n:]]
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
                    # Wake any QA-fix loop awaiting "this turn finished" (P3b). The
                    # status is already set above, so a loop that races to read it
                    # sees error/idle correctly.
                    agent.turn_done.set()
        except asyncio.CancelledError:
            raise
        except Exception as e:  # noqa: BLE001 - a dead stream must mark the agent, not crash the server
            self._set_status(agent, "error")
            self._event(agent, "error", repr(e))

    def _event(self, agent: Agent, kind: str, text: str) -> None:
        # Store generous text (the HUD console shows it via status(full=True));
        # brain-facing and log views re-truncate at serve time.
        agent.events.append({"t": round(time.time(), 1), "kind": kind, "text": text[:2000]})
        # Mirror to the tailable log so the build is observable live.
        log.info("[%s/%s] %-9s | %s", agent.name, agent.id, kind, text[:240].replace("\n", " "))

    def _announce(self, agent: Agent, text: str) -> None:
        """Publish a spoken line for the QA-fix loop. Bumps announce_seq so the Rust
        poll thread (P2) speaks it exactly once, mirrors it to the event feed, and
        persists so a detached snapshot still carries the last thing said."""
        agent.announce = text
        agent.announce_seq += 1
        self._event(agent, "announce", text)
        self._persist(agent)

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
