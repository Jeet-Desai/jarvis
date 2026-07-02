import { createSignal, onMount, onCleanup, Show, Index } from "solid-js";
import {
  state,
  transcript,
  response,
  agents,
  appStartMs,
  setJarvisState,
  type JarvisState,
} from "../shared/state";
import { agentMeta, isActive } from "../shared/agentMeta";
import { ActivityFeed } from "../shared/ActivityFeed";
import { Reactor } from "../shared/ReactorCanvas";

const STATES: { key: JarvisState; label: string }[] = [
  { key: "idle", label: "IDLE" },
  { key: "listening", label: "LISTEN" },
  { key: "thinking", label: "THINK" },
  { key: "speaking", label: "SPEAK" },
];

/**
 * CORE — the glanceable command deck: central reactor, state pill, activity
 * column, live agents mini-list and the YOU/JARVIS conversation cards.
 * Click-through except the explicitly interactive panels.
 */
export function CoreView(props: { onOpenAgent: (id: string | null) => void }) {
  const [uptime, setUptime] = createSignal(fmtUptime());

  onMount(() => {
    const id = setInterval(() => setUptime(fmtUptime()), 1000);
    onCleanup(() => clearInterval(id));
  });

  const jColor = () =>
    state() === "thinking" ? "var(--jarvis-blue)"
    : state() === "error" ? "var(--jarvis-red)"
    : "var(--jarvis-cyan)";
  const activeCount = () => agents().filter(isActive).length;
  const miniItems = () => agents().slice(0, 5);

  return (
    <div class="core-view">
      {/* state pill */}
      <div class="core-pill" style={{ "border-color": jColor() }}>
        <span class="core-pill-dot" style={{ background: jColor(), "box-shadow": `0 0 12px ${jColor()}` }} />
        <span class="core-pill-text">JARVIS // {state().toUpperCase()}</span>
      </div>

      {/* central reactor */}
      <div class="core-reactor">
        <Reactor size="560px" />
      </div>

      {/* state controls (dev affordance — the voice pipeline drives these in normal use) */}
      <div class="core-statebar">
        <Index each={STATES}>
          {(s) => (
            <button
              class="core-statebtn"
              classList={{ on: state() === s().key }}
              onClick={() => setJarvisState(s().key)}
            >
              {s().label}
            </button>
          )}
        </Index>
      </div>

      {/* left status column */}
      <div class="core-left">
        <div class="core-heading">ACTIVITY</div>
        <ActivityFeed limit={6} />
        <div class="core-divider" />
        <div class="core-readouts">
          <div>
            UPTIME <span>{uptime()}</span>
          </div>
          <div>
            AGENTS <span>{activeCount()} ACTIVE</span>
          </div>
          <div>
            MEM <span>NOMINAL</span>
          </div>
        </div>
      </div>

      {/* right agents mini-list */}
      <div class="core-agents">
        <div class="core-agents-head">
          <span class="core-heading">AGENTS</span>
          <button class="core-viewall" onClick={() => props.onOpenAgent(null)}>
            VIEW ALL →
          </button>
        </div>
        <Show when={miniItems().length > 0} fallback={<div class="core-agents-empty">no agents</div>}>
          <div class="core-agents-list">
            <Index each={miniItems()}>
              {(a) => {
                const m = () => agentMeta(a());
                return (
                  <div class="core-agent-row" style={{ "border-left-color": m().c }} onClick={() => props.onOpenAgent(a().id)}>
                    <div class="core-agent-top">
                      <span class="core-agent-name">{a().name}</span>
                      <span class="core-agent-state" style={{ color: m().c }}>
                        {m().detached ? "DETACHED" : m().label}
                      </span>
                    </div>
                    <div class="core-agent-task">{a().last || "—"}</div>
                  </div>
                );
              }}
            </Index>
          </div>
        </Show>
      </div>

      {/* conversation cards */}
      <div class="core-convo">
        <div class="core-card">
          <div class="core-card-label">YOU</div>
          <div class="core-card-text" title={transcript()}>
            {transcript() || <span class="core-placeholder">— press ⌘⇧J and speak —</span>}
          </div>
        </div>
        <div class="core-card">
          <div class="core-card-label">JARVIS</div>
          <div class="core-card-text" title={response()}>
            {response() || <span class="core-placeholder">— standing by —</span>}
          </div>
        </div>
      </div>
    </div>
  );
}

function fmtUptime(): string {
  const s = Math.max(0, Math.floor((Date.now() - appStartMs) / 1000));
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(Math.floor(s / 3600))}:${p(Math.floor((s % 3600) / 60))}:${p(s % 60)}`;
}
