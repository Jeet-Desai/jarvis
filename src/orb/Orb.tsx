import { onMount } from "solid-js";
import { state, initStateBridge } from "../shared/state";
import { OrbScene } from "../shared/OrbScene";
import { ConfirmationPrompt } from "../shared/ConfirmationPrompt";
import "./orb.css";

export function Orb() {
  onMount(() => initStateBridge());

  return (
    <div class="orb-root" data-tauri-drag-region>
      <OrbScene size="100%" />
      <div class="orb-label">{state()}</div>
      {/* Compact, text-focused confirmation — the orb window is click-through,
          so the ⌘⇧Y / ⌘⇧S hotkeys are the real path to act on it. */}
      <ConfirmationPrompt variant="orb" />
    </div>
  );
}
