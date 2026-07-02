import { onMount, onCleanup } from "solid-js";
import { state, level, brightness } from "./state";
import { ReactorRenderer } from "./reactor";

interface Props {
  /** CSS size of the square canvas, e.g. "560px" or "100%". */
  size?: string;
  /** Draw the state-reactive ring system around the sphere (default true). */
  withRings?: boolean;
}

/**
 * The wireframe reactor sphere. Owns its canvas + requestAnimationFrame loop;
 * reads the state/level/brightness signals untracked each frame (animation
 * lives outside the reactive graph, per the design handoff).
 */
export function Reactor(props: Props) {
  let canvas!: HTMLCanvasElement;
  const renderer = new ReactorRenderer();

  onMount(() => {
    let raf = requestAnimationFrame(function loop() {
      raf = requestAnimationFrame(loop);
      const st = state();
      renderer.tick(st);
      renderer.draw(canvas, st, props.withRings ?? true, level(), brightness());
    });
    onCleanup(() => cancelAnimationFrame(raf));
  });

  return (
    <canvas
      ref={canvas}
      class="reactor-canvas"
      style={{ width: props.size ?? "100%", height: props.size ?? "100%", display: "block" }}
    />
  );
}
