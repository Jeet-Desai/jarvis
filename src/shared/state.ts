import { createSignal } from "solid-js";
import { listen } from "@tauri-apps/api/event";

export type JarvisState = "idle" | "listening" | "thinking" | "speaking" | "error";

export const [state, setState] = createSignal<JarvisState>("idle");
export const [level, setLevel] = createSignal<number>(0); // 0..1 audio level
export const [transcript, setTranscript] = createSignal<string>("");
export const [response, setResponse] = createSignal<string>("");
/** User-controlled multiplicative brightness for the orb. Adjusted via
 *  ⇧⌘+ / ⇧⌘- hotkeys. Clamped 0.3..2.0 by backend before emitting. */
export const [brightness, setBrightness] = createSignal<number>(1.0);

let initialized = false;

export async function initStateBridge() {
  if (initialized) return;
  initialized = true;

  await listen<JarvisState>("jarvis:state", (e) => setState(e.payload));
  await listen<number>("jarvis:level", (e) => setLevel(e.payload));
  await listen<string>("jarvis:transcript", (e) => setTranscript(e.payload));
  await listen<string>("jarvis:response", (e) => setResponse(e.payload));
  await listen<number>("jarvis:brightness", (e) => setBrightness(e.payload));
}
