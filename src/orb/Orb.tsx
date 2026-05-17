import { onMount } from "solid-js";
import { state, initStateBridge } from "../shared/state";
import { OrbScene } from "../shared/OrbScene";
import "./orb.css";

export function Orb() {
  onMount(() => initStateBridge());

  return (
    <div class="orb-root" data-tauri-drag-region>
      <OrbScene size="100%" />
      <div class="orb-label">{state()}</div>
    </div>
  );
}
