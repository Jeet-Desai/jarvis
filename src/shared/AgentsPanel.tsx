import { Show, Index } from "solid-js";
import { agents, type AgentItem, type AgentStatus } from "./state";
import "./confirm.css";

// Status → glyph, reusing the activity-feed visual language. "working" pulses
// (same animation as a running action) so an active agent reads as live.
const GLYPH: Record<AgentStatus, string> = {
  spawning: "◌",
  working: "▶",
  idle: "✓",
  stopped: "⊘",
  error: "✕",
};

// Map an agent status onto the existing jv-glyph-* colour classes so we don't
// invent a second colour palette (working≈running cyan, idle≈ok green, …).
const GLYPH_CLASS: Record<AgentStatus, string> = {
  spawning: "jv-glyph-awaiting",
  working: "jv-glyph-running",
  idle: "jv-glyph-ok",
  stopped: "jv-glyph-denied",
  error: "jv-glyph-error",
};

/**
 * Compact live roster of dev agents from the orchestrator sidecar. Driven by
 * the `agents` signal (refreshed ~every 2s by the backend poll task). Mirrors
 * ActivityFeed's look; renders a faint placeholder when no agents exist.
 */
export function AgentsPanel(props: { limit?: number }) {
  const items = () => agents().slice(0, props.limit ?? 6);

  return (
    <Show when={items().length > 0} fallback={<div class="jv-activity-empty">no agents</div>}>
      <div class="jv-activity">
        {/* Index (position-keyed), NOT For (reference-keyed): the backend re-emits
            a brand-new AgentItem[] every ~2s poll, so For would unmount+remount
            every row each refresh (visible flicker). Index keeps the row DOM
            mounted and only updates the changed text/glyph in place. */}
        <Index each={items()}>{(agent) => <AgentRow agent={agent} />}</Index>
      </div>
    </Show>
  );
}

// `agent` is an Index accessor (() => AgentItem) so the row stays mounted and its
// fields update reactively in place across refreshes (no remount = no flicker).
function AgentRow(props: { agent: () => AgentItem }) {
  const a = () => props.agent();
  const cost = () => `$${(a().cost_usd ?? 0).toFixed(2)}`;
  return (
    <div class="jv-agent-row" title={a().last}>
      <div class="jv-agent-head">
        <span class={`jv-activity-glyph ${GLYPH_CLASS[a().status]}`}>{GLYPH[a().status]}</span>
        <span class="jv-agent-name">{a().name}</span>
        <span class="jv-agent-meta">
          {a().status} · {cost()}
        </span>
      </div>
      <Show when={a().last}>
        <div class="jv-agent-last">{a().last}</div>
      </Show>
    </div>
  );
}
