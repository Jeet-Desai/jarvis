/**
 * Presentation metadata for dev-agent statuses — the state-colour system from
 * the design handoff (working cyan, spawning blue, idle slate, stopped warm
 * grey, error red). Detached agents (live=false, restored from the registry)
 * render in the stopped grey regardless of their recorded status.
 */
import type { AgentItem, AgentStatus } from "./state";

export interface AgentMeta {
  label: string;
  /** accent colour */
  c: string;
  /** soft translucent fill of the same hue */
  soft: string;
  pulse: boolean;
  blink: boolean;
  detached: boolean;
}

const META: Record<AgentStatus, Omit<AgentMeta, "detached">> = {
  spawning: { label: "SPAWNING", c: "#8fb9ff", soft: "rgba(143,185,255,0.14)", pulse: true, blink: false },
  working: { label: "WORKING", c: "#5fe9f5", soft: "rgba(95,233,245,0.14)", pulse: true, blink: false },
  idle: { label: "IDLE", c: "#5e7c86", soft: "rgba(94,124,134,0.12)", pulse: false, blink: false },
  stopped: { label: "STOPPED", c: "#7d7370", soft: "rgba(125,115,112,0.12)", pulse: false, blink: false },
  error: { label: "ERROR", c: "#ff6b5e", soft: "rgba(255,107,94,0.15)", pulse: false, blink: true },
};

export function agentMeta(a: AgentItem): AgentMeta {
  const m = META[a.status] ?? META.idle;
  if (a.live === false) {
    return { ...m, c: "#7d7370", soft: "rgba(125,115,112,0.12)", pulse: false, blink: false, detached: true };
  }
  return { ...m, detached: false };
}

/** Actively doing something right now (drives counts + the NOW line). */
export function isActive(a: AgentItem): boolean {
  return (a.status === "working" || a.status === "spawning") && a.live !== false;
}

export function fmtCost(a: AgentItem): string {
  return `$${(a.cost_usd ?? 0).toFixed(2)}`;
}

/** "48s" / "2m 41s" / "1h 04m" from the agent's spawn time. */
export function fmtRuntime(a: AgentItem, nowMs: number): string {
  if (!a.created) return "—";
  const s = Math.max(0, Math.floor(nowMs / 1000 - a.created));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}
