import { createSignal } from "solid-js";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

export type JarvisState = "idle" | "listening" | "thinking" | "speaking" | "error";

export type Danger = "read_only" | "mutating" | "destructive";
export type ActivityStatus = "awaiting" | "running" | "ok" | "error" | "denied";

export interface ActivityItem {
  id: number;
  tool: string;
  summary: string;
  danger: Danger;
  status: ActivityStatus;
}

export interface PendingConfirm {
  id: number;
  summary: string;
  danger: Danger;
}

/** A dev agent in the orchestrator sidecar (mirrors AgentManager.summary()). */
export type AgentStatus = "spawning" | "working" | "idle" | "stopped" | "error";

export interface AgentItem {
  id: string;
  name: string;
  status: AgentStatus;
  branch: string;
  worktree: string;
  model: string;
  cost_usd: number;
  turns: number;
  last: string;
  /** Epoch seconds of spawn — drives the RUNTIME readout. */
  created?: number;
  /** false = detached record restored from the registry (not controllable). */
  live?: boolean;
  /** true while the self-correcting QA loop is driving this agent. */
  qa_active?: boolean;
}

export const [state, setState] = createSignal<JarvisState>("idle");
export const [level, setLevel] = createSignal<number>(0); // 0..1 audio level
export const [transcript, setTranscript] = createSignal<string>("");
export const [response, setResponse] = createSignal<string>("");
/** User-controlled multiplicative brightness for the orb. Adjusted via
 *  ⇧⌘+ / ⇧⌘- hotkeys. Clamped 0.3..2.0 by backend before emitting. */
export const [brightness, setBrightness] = createSignal<number>(1.0);

/** Last ~12 autonomous-control activity items, newest first. Updates merge by id. */
export const [activity, setActivity] = createSignal<ActivityItem[]>([]);
/** The currently pending computer-control confirmation, or null. */
export const [pendingConfirm, setPendingConfirm] = createSignal<PendingConfirm | null>(null);
/** Backend policy mode: "safe" | "trusted" | "dev". Fetched once on init. */
export const [policyMode, setPolicyMode] = createSignal<string>("");

/** Live dev-agent roster, refreshed ~every 2s by the backend poll task. */
export const [agents, setAgents] = createSignal<AgentItem[]>([]);

/** One event from an agent's history (manager.py `_event`). */
export interface AgentEvent {
  t: number;
  kind: string; // say | tool | error | qa_loop | announce | spawn | instruct | stop
  text: string;
}

/** Window-load timestamp — the HUD webview is created at app launch (hidden),
 *  so this doubles as the app-uptime origin for the UPTIME readout. */
export const appStartMs = Date.now();

const MAX_ACTIVITY = 12;

let initialized = false;

export async function initStateBridge() {
  // Idempotent per-window: each Tauri window is its own JS context, so this
  // guard prevents double-attaching listeners within a single window (which
  // would otherwise cause each event to be handled twice).
  if (initialized) return;
  initialized = true;

  // Browser preview (plain `pnpm dev` outside Tauri): no event bridge exists,
  // so expose the setters for manual/QA driving and skip the listeners.
  if (!("__TAURI_INTERNALS__" in window)) {
    (window as unknown as Record<string, unknown>).__jarvisPreview = {
      setState,
      setLevel,
      setTranscript,
      setResponse,
      setActivity,
      setAgents,
      setPendingConfirm,
      setPolicyMode,
    };
    return;
  }

  await listen<JarvisState>("jarvis:state", (e) => setState(e.payload));
  await listen<number>("jarvis:level", (e) => setLevel(e.payload));
  await listen<string>("jarvis:transcript", (e) => setTranscript(e.payload));
  await listen<string>("jarvis:response", (e) => setResponse(e.payload));
  await listen<number>("jarvis:brightness", (e) => setBrightness(e.payload));

  await listen<ActivityItem>("jarvis:activity", (e) => {
    const item = e.payload;
    setActivity((prev) => {
      const idx = prev.findIndex((a) => a.id === item.id);
      if (idx >= 0) {
        // Update existing item in place (preserve ordering).
        const next = prev.slice();
        next[idx] = { ...next[idx], ...item };
        return next;
      }
      // New item — newest first, capped.
      return [item, ...prev].slice(0, MAX_ACTIVITY);
    });
  });

  // Full dev-agent roster snapshot (not merged — the backend sends the whole
  // list each poll, so replacing is correct and reflects removals).
  await listen<AgentItem[]>("jarvis:agent", (e) => setAgents(e.payload ?? []));

  await listen<PendingConfirm>("jarvis:confirm", (e) => setPendingConfirm(e.payload));

  await listen<{ id: number; approved: boolean }>("jarvis:confirm_resolved", (e) => {
    setPendingConfirm((cur) => (cur && cur.id === e.payload.id ? null : cur));
  });

  // Fetch the policy mode once (best-effort; leave blank on failure).
  try {
    const mode = await invoke<string>("jarvis_policy_mode");
    setPolicyMode(mode);
  } catch {
    // ignore — UI tolerates an empty mode
  }
}

/** Approve or deny a pending computer-control confirmation. */
export async function resolveConfirm(id: number, approved: boolean): Promise<void> {
  try {
    await invoke("jarvis_resolve_confirm", { id, approved });
  } catch {
    // The backend also emits jarvis:confirm_resolved; if the invoke fails the
    // prompt stays up so the user can retry via hotkey.
  }
}

/** Drive the assistant state machine (the CORE view's debug state buttons). */
export async function setJarvisState(s: JarvisState): Promise<void> {
  try {
    await invoke("jarvis_set", { newState: s });
  } catch {
    // non-fatal — the buttons are a dev affordance
  }
}

/** Switch to the MINI orb (toggle_hud shows the orb and hides the HUD). */
export async function showMini(): Promise<void> {
  try {
    await invoke("toggle_hud");
  } catch {
    // ignore
  }
}

/** Spawn a dev agent from the AGENTS space. Resolves when the sidecar has
 *  created it (the roster poll then picks it up within ~2s). */
export function spawnAgent(task: string): Promise<string> {
  return invoke<string>("jarvis_agent_spawn", { task });
}

/** Stop a dev agent (keeps its worktree for review). */
export function stopAgent(agentId: string): Promise<string> {
  return invoke<string>("jarvis_agent_stop", { agentId });
}

/** Send a follow-up instruction to a live dev agent. */
export function instructAgent(agentId: string, instruction: string): Promise<string> {
  return invoke<string>("jarvis_agent_instruct", { agentId, instruction });
}

/** Fetch an agent's full status + event history for the detail console.
 *  Returns null on any failure (orchestrator down, browser preview, …). */
export async function fetchAgentStatus(
  agentId: string,
): Promise<{ events?: AgentEvent[] } | null> {
  try {
    const raw = await invoke<string>("jarvis_agent_status", { agentId });
    let v: unknown = JSON.parse(raw);
    // The MCP layer wraps tool replies as {"text": "<json>"} — unwrap it.
    if (v && typeof v === "object" && "text" in (v as Record<string, unknown>)) {
      v = JSON.parse((v as Record<string, string>).text);
    }
    return v as { events?: AgentEvent[] };
  } catch {
    return null;
  }
}
