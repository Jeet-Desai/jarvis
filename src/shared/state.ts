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

const MAX_ACTIVITY = 12;

let initialized = false;

export async function initStateBridge() {
  // Idempotent per-window: each Tauri window is its own JS context, so this
  // guard prevents double-attaching listeners within a single window (which
  // would otherwise cause each event to be handled twice).
  if (initialized) return;
  initialized = true;

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
