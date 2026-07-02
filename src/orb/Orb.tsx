import { onMount, Show } from "solid-js";
import { state, agents, initStateBridge } from "../shared/state";
import { isActive } from "../shared/agentMeta";
import { Reactor } from "../shared/ReactorCanvas";
import { ConfirmationPrompt } from "../shared/ConfirmationPrompt";
import "./orb.css";

/**
 * MINI — the always-on quick-glance orb (design handoff §3): the reactive
 * sphere + rings, a state pill, working/idle/error agent chips and a NOW
 * line. The window is a click-through NSPanel (only dragging works), so
 * everything here is display-only; ⌘⇧Y / ⌘⇧S remain the action hotkeys.
 */
export function Orb() {
  onMount(() => initStateBridge());

  const jColor = () =>
    state() === "thinking" ? "var(--jarvis-blue)"
    : state() === "error" ? "var(--jarvis-red)"
    : "var(--jarvis-cyan)";

  const workingCount = () => agents().filter(isActive).length;
  const idleCount = () => agents().filter((a) => a.status === "idle" && a.live !== false).length;
  const errorCount = () => agents().filter((a) => a.status === "error").length;
  const qaCount = () => agents().filter((a) => a.qa_active).length;

  return (
    <div class="orb-root" data-tauri-drag-region>
      <div class="mini-stack">
        <div class="mini-orb">
          <Reactor size="100%" />
        </div>

        <div class="mini-pill" style={{ "border-color": jColor() }}>
          <span class="mini-pill-dot" style={{ background: jColor(), "box-shadow": `0 0 8px ${jColor()}` }} />
          <span class="mini-pill-text">{state().toUpperCase()}</span>
        </div>

        {/* chips appear only for non-zero counts — zero rows are noise */}
        <div class="mini-chips">
          <Show when={workingCount() > 0}>
            <div class="mini-chip working">
              <span class="mini-chip-dot" />
              {workingCount()} WORKING
            </div>
          </Show>
          <Show when={idleCount() > 0}>
            <div class="mini-chip idle">
              <span class="mini-chip-dot" />
              {idleCount()} IDLE
            </div>
          </Show>
          <Show when={errorCount() > 0}>
            <div class="mini-chip err">
              <span class="mini-chip-dot" />
              {errorCount()} ERR
            </div>
          </Show>
          <Show when={qaCount() > 0}>
            <div class="mini-chip qa">
              <span class="mini-chip-dot" />
              {qaCount()} QA
            </div>
          </Show>
        </div>
      </div>

      {/* Compact, text-focused confirmation — the orb window is click-through,
          so the ⌘⇧Y / ⌘⇧S hotkeys are the real path to act on it. */}
      <ConfirmationPrompt variant="orb" />
    </div>
  );
}
