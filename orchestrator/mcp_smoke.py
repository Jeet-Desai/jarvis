#!/usr/bin/env python3
"""
Cheap MCP-surface smoke test: connect to server.py as an MCP client over stdio,
prove the handshake + tool discovery + a no-spawn tool call. No agents spawned,
so it costs nothing — it just proves the wrapper Jarvis will talk to is wired up.

Run:  uv run python mcp_smoke.py
"""
import anyio
from pathlib import Path

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

HERE = Path(__file__).parent.resolve()
PY = HERE / ".venv" / "bin" / "python"

EXPECTED = {"spawn_dev_agent", "instruct_dev_agent", "list_dev_agents",
            "get_agent_status", "stop_dev_agent"}


async def main():
    params = StdioServerParameters(command=str(PY), args=["server.py"], cwd=str(HERE))
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            tools = await session.list_tools()
            names = {t.name for t in tools.tools}
            print("exposed tools:", sorted(names))

            res = await session.call_tool("list_dev_agents", {})
            print("list_dev_agents() →", [c.text for c in res.content])

            ok = EXPECTED <= names
            print(f"\n{'PASS' if ok else 'FAIL'}: all 5 orchestrator tools exposed over MCP")
            return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(anyio.run(main))
