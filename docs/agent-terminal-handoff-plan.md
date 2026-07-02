# Agent → terminal handoff ("steer it myself")

*Planned 2026-07-02 from web research (sources at bottom). Status: TODO — next session.*

**Goal:** a button in the HUD AGENTS detail panel (and a voice command) that opens the
selected dev agent's Claude Code session in a real terminal at its worktree, with full
conversation context, so the user can steer it manually — then optionally hand it back.

## How it works (research findings, condensed)

- Every Agent-SDK session persists to `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`
  (cwd with non-alphanumerics → `-`). The Python SDK always persists (no opt-out).
  Officially supported: *"Sessions created with `claude -p` or the Agent SDK … can be
  resumed by passing the session ID to `claude --resume <session-id>`."*
- **Session id capture:** the first `SystemMessage` with `subtype == "init"` carries
  `message.data["session_id"]` (also on every `ResultMessage.session_id`). Capture at
  spawn in `manager.py`'s drain loop and store on the `Agent` + registry record.
  Track the LATEST result's id (resume can mint a new one — issue #8069).
- **Resume is cwd-scoped**: the terminal must launch with `cwd == the agent's worktree`
  (lookup covers the project dir + its git worktrees; exact worktree is the safe bet).
  Don't rename/remove a worktree that might be handed off (Vibe Kanban #2993).
- **Concurrency:** no lock — resuming while the SDK client is attached interleaves two
  writers into one transcript. Two modes:
  - **Take over** (default): sidecar does `client.interrupt()` → drain until the
    `error_during_execution` ResultMessage → `disconnect()`; mark agent `handed_off`;
    then open the terminal with `claude --resume <id>`.
  - **Peek (fork)**: agent keeps running; terminal opens `claude --resume <id>
    --fork-session` (explicit id only — the picker ignores fork, issue #15694).
- **Re-adopt later:** new SDK query with `ClaudeAgentOptions(resume=session_id,
  cwd=worktree)` — it sees the user's terminal turns. Gate on the terminal being closed
  (else interleaving again).
- **Options don't travel** with the transcript: pass `--permission-mode acceptEdits`
  (or the user's choice) explicitly; in-process SDK MCP servers and the SDK
  `system_prompt` do NOT exist in the terminal CLI — acceptable for manual steering.
- **Opening the terminal (macOS):** default to the TCC-free path — write a one-shot
  `handoff-<agent>.command` (`#!/bin/zsh\ncd '<worktree>' && exec claude --resume '<id>'
  --permission-mode acceptEdits`), `chmod +x`, `open -a Terminal <file>` (or iTerm).
  No Apple Events, no new permission prompt. Optional nicer iTerm2 AppleScript path
  later (needs Automation TCC per target app). Preferred terminal = env
  `JARVIS_TERMINAL` → probe iTerm2 → Terminal.app fallback.

## Implementation stages

1. **Sidecar: session plumbing** (`orchestrator/manager.py`)
   - Capture `session_id` from the init SystemMessage in the drain loop; store on
     `Agent`, include in `summary()` (→ reaches the HUD roster + registry persistence,
     so DETACHED agents from past runs become resumable too — that's a big win).
   - `handoff_dev_agent(agent_id, fork: bool = False) -> dict`: take-over protocol
     (interrupt → drain → disconnect → status `handed_off`) or fork mode; returns
     `{session_id, worktree, command}`. protected-paths assert as usual.
   - `readopt_dev_agent(agent_id)`: new client with `resume=`, verify via a no-op turn.
   - Free unit tests (no agent spawn) for the state transitions.
2. **Rust:** `jarvis_agent_handoff(agent_id, fork)` command (spawn_blocking, like the
   other agent commands) → calls the sidecar tool, then writes the `.command` file and
   `open -a`s the terminal (Rust side owns the file + `open`, so the sidecar stays
   pure orchestration). Audit-log the handoff.
3. **HUD:** detail-panel buttons `⌥ OPEN IN TERMINAL` (take over) and `⑂ PEEK` (fork)
   — take-over disables INSTRUCT/STOP and shows a `HANDED OFF` badge; a `RE-ADOPT`
   button appears while handed off. Voice: "let me drive the <name> agent".
4. **Polish:** `rename_session(id, "jarvis: <agent name>")` at spawn so sessions are
   findable by name; suppress worktree cleanup for handed-off agents; `claude -v`
   check (fork+picker bug is version-sensitive).

## Gotchas checklist (from research — keep)

- Capture id EARLY (init message), not only at ResultMessage — button must work mid-turn.
- `--session-id` is create-only (collision = hard error); never pass with `--resume`.
- Interrupt doesn't flush: drain `receive_response()` before disconnect.
- Don't set `--no-session-persistence` / `CLAUDE_CODE_SKIP_PROMPT_HISTORY` anywhere.
- JSONL transcript format is internal — never parse; use SDK `list_sessions()` etc.
- `applescript_escape` any interpolated path/id if the AppleScript path is ever used.

Sources: code.claude.com/docs/en/sessions · …/agent-sdk/sessions · …/cli-reference ·
…/agent-sdk/python · anthropics/claude-code#42311 #15694 #52262 #5524 #8069 ·
BloopAI/vibe-kanban#2993 · github.com/smtg-ai/claude-squad · scriptingosx.com (open -a
.command, AppleScript TCC) · steipete.me AppleScript CLI guide
