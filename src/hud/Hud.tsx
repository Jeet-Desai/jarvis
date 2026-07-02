import { onMount, createSignal, onCleanup, Show, For } from "solid-js";
import { state, policyMode, initStateBridge, showMini } from "../shared/state";
import { ConfirmationPrompt } from "../shared/ConfirmationPrompt";
import { CoreView } from "./CoreView";
import { AgentsView } from "./AgentsView";
import "./hud.css";

type HudView = "core" | "agents";

/**
 * The full HUD overlay (design handoff: CORE + AGENTS views under a shared
 * 48px top bar). The window itself is a 1440×900 transparent NSPanel; the
 * CORE view stays click-through outside its interactive panels, while the
 * AGENTS view is a fully interactive workspace.
 */
export function Hud() {
  const [view, setView] = createSignal<HudView>("core");
  const [selectedId, setSelectedId] = createSignal<string | null>(null);
  const [now, setNow] = createSignal(new Date());

  onMount(() => {
    initStateBridge();
    const id = setInterval(() => setNow(new Date()), 1000);
    onCleanup(() => clearInterval(id));
  });

  const openAgent = (id: string | null) => {
    if (id) setSelectedId(id);
    setView("agents");
  };

  const jColor = () =>
    state() === "thinking" ? "var(--jarvis-blue)"
    : state() === "error" ? "var(--jarvis-red)"
    : "var(--jarvis-cyan)";

  const clock = () => {
    const d = now();
    const p = (n: number) => String(n).padStart(2, "0");
    return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  };
  const dateStr = () =>
    now().toLocaleDateString("en-US", { weekday: "short", day: "2-digit", month: "short" }).toUpperCase();

  return (
    <div class="hud-root">
      {/* ambient layers — all pointer-events:none */}
      <div class="hud-backdrop" data-view={view()} />
      <div class="hud-scanlines" />
      <div class="hud-vignette" />
      <Dust />
      <div class="hud-bracket tl" />
      <div class="hud-bracket tr" />
      <div class="hud-bracket bl" />
      <div class="hud-bracket br" />

      {/* top bar */}
      <div class="hud-topbar">
        <div class="hud-topbar-left">
          <div class="hud-wordmark">
            <span class="hud-wordmark-dot" style={{ background: jColor(), "box-shadow": `0 0 10px ${jColor()}` }} />
            JARVIS
          </div>
          <div class="hud-tabs">
            <button class="hud-tab" classList={{ on: view() === "core" }} onClick={() => setView("core")}>
              CORE
            </button>
            <button class="hud-tab" classList={{ on: view() === "agents" }} onClick={() => setView("agents")}>
              AGENTS
            </button>
            {/* MINI lives in the orb window — this tab swaps the panels over */}
            <button class="hud-tab" onClick={() => showMini()}>
              MINI
            </button>
          </div>
        </div>
        <div class="hud-topbar-right">
          <div class="hud-sys">
            <span>
              SYS <b class="cyan">ONLINE</b>
            </span>
            <span>
              MIC <b class="cyan">{state() === "listening" ? "LIVE" : "READY"}</b>
            </span>
            <span>
              MODE <b class="hud-mode" data-mode={policyMode()}>{(policyMode() || "—").toUpperCase()}</b>
            </span>
          </div>
          <div class="hud-clock">
            <div class="hud-clock-time">{clock()}</div>
            <div class="hud-clock-date">{dateStr()}</div>
          </div>
        </div>
      </div>

      {/* active view */}
      <Show when={view() === "core"}>
        <CoreView onOpenAgent={openAgent} />
      </Show>
      <Show when={view() === "agents"}>
        <AgentsView selectedId={selectedId()} onSelect={setSelectedId} />
      </Show>

      {/* Confirmation overlay — sits above everything */}
      <ConfirmationPrompt variant="hud" />
    </div>
  );
}

/** Floating dust motes (generated once per window). */
function Dust() {
  const motes = Array.from({ length: 16 }, () => ({
    l: Math.random() * 100,
    t: 50 + Math.random() * 50,
    size: 1 + Math.random() * 2,
    dur: 6 + Math.random() * 8,
    delay: Math.random() * 8,
  }));
  return (
    <div class="hud-dust">
      <For each={motes}>
        {(d) => (
          <span
            style={{
              left: `${d.l}%`,
              top: `${d.t}%`,
              width: `${d.size}px`,
              height: `${d.size}px`,
              "animation-duration": `${d.dur}s`,
              "animation-delay": `${d.delay}s`,
            }}
          />
        )}
      </For>
    </div>
  );
}
