# Jarvis dev-agent orchestrator (sidecar)

Python sidecar that lets Jarvis **spawn and supervise Claude Code dev agents** in
isolated git worktrees. Jarvis (Rust) talks to it over **MCP/stdio** — the same
pattern as the Google Calendar MCP — so the brain's tool calls flow through
`tools::dispatch` and the safety gate.

Built on the official [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python).

## Files
- `manager.py` — `AgentManager`: the core. Spawn/instruct/list/status/stop agents.
  Each agent = a git worktree + a persistent `ClaudeSDKClient`, drained by a
  background task that tracks status/cost/recent activity. **No MCP/Jarvis coupling**
  (so it's unit-testable).
- `server.py` — thin `FastMCP` stdio server exposing 5 tools: `spawn_dev_agent`,
  `instruct_dev_agent`, `list_dev_agents`, `get_agent_status`, `stop_dev_agent`.
- `test_manager.py` — standalone proof (2 concurrent agents, isolation, follow-up).
- `test_persistence.py` — free/instant proof of the on-disk roster (no real agents).
- `mcp_smoke.py` — cheap MCP-surface check (no agents spawned).

## Persistence (the agent roster survives restarts)
The manager writes a JSON roster to `~/claude/jarvis-workspace/agents-registry.json`
(next to `agents/`; override with `JARVIS_AGENTS_REGISTRY`) on spawn and on every
status change. On startup `AgentManager.__init__` reloads it, so after an app/sidecar
restart `list_dev_agents` / `get_agent_status` / the HUD **AGENTS** panel still show
past agents — marked **detached** (`live: false`), with their last status, cost, and
on-disk worktree/project preserved. The live `ClaudeSDKClient` session can't be
revived, so detached agents are roster-only (you can't instruct them); a transient
`spawning`/`working` status is downgraded to `idle` on reload (nothing is draining it
anymore). Live agents always override their persisted record.

## Run
```bash
uv venv --python 3.12 && uv pip install claude-agent-sdk mcp
uv run python test_manager.py   # full proof (spawns real Sonnet agents, ~$0.20)
uv run python mcp_smoke.py       # MCP surface only (free)
uv run python server.py          # run as an MCP stdio server (what Jarvis spawns)
```

## Design rules (learned the hard way in the Phase-0 spike)
- **Agent worktrees live outside any enclosing repo** — `AGENTS_ROOT` auto-resolves
  to the nearest non-repo ancestor of the jarvis repo, in a visible
  `jarvis-workspace/agents/` (e.g. `~/claude/jarvis-workspace/agents/`). Override with
  the `JARVIS_AGENTS_ROOT` env var. If worktrees sit inside a repo, Claude Code
  auto-discovers a parent `CLAUDE.md` and writes to the wrong root.
- **`setting_sources=[]`** so no external `CLAUDE.md`/settings bleed into agents.
- **Pin the workspace in an appended system prompt** + set `cwd`/`add_dirs` — Claude
  Code's Write tool otherwise resolves bare relative paths to `$HOME`.
- **Default model = `sonnet`** for cost (Opus ~2–3× pricier); override per spawn.
- Autonomy = `permission_mode="acceptEdits"` (edits auto-apply; shell whitelisted via
  `allowed_tools`). `can_use_tool` can later escalate risky calls to Jarvis's voice gate.

## Status / next steps
- ✅ Phase 1.1 — `AgentManager` + MCP surface proven standalone.
- ✅ Phase 1.2 — wired into Jarvis (`src-tauri/src/mcp.rs`, second MCP server),
  5 tools in the brain registry with the right `DangerLevel`.
- ✅ Phase 1.3 — HUD "AGENTS" panel: `jarvis:agent` event + ~2s poll of
  `list_dev_agents`.
- ✅ Phase 2 — **persistence** (this file's registry) + **proactive voice** (Rust:
  the poll thread diffs statuses and speaks a one-line update when an agent
  finishes a turn or errors, only while Jarvis is at rest).
- ⏭️ Phase 3 (hero) — closed-loop visual QA; then parallel/merge, budgets/autonomy.

See the memory note `jarvis-3-dev-orchestrator` for the full plan + decisions.
