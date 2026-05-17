import { onMount, onCleanup, createEffect } from "solid-js";
import * as THREE from "three";
import { Line2 } from "three/examples/jsm/lines/Line2.js";
import { LineSegments2 } from "three/examples/jsm/lines/LineSegments2.js";
import { LineGeometry } from "three/examples/jsm/lines/LineGeometry.js";
import { LineSegmentsGeometry } from "three/examples/jsm/lines/LineSegmentsGeometry.js";
import { LineMaterial } from "three/examples/jsm/lines/LineMaterial.js";
import {
  EffectComposer, RenderPass, EffectPass, BloomEffect, KernelSize,
} from "postprocessing";

const BLOOM_BASE_INTENSITY = 0.85;
import { state, level, brightness } from "./state";

type Props = {
  size?: number | string;
  intensityScale?: number;
};

// ── Canonical "Jarvis cyan" palette (Iron Man 2 / Avengers 1 era) ───────────
const CYAN_HERO   = 0x3ddbd9; // primary stroke
const CYAN_HOT    = 0xa8f0ff; // bright highlight / hot pixel
const CYAN_DIM    = 0x1b9aaa; // mid-tone, idle
const TEAL_DEEP   = 0x0a4d5c; // deep shadow
const RED_ERROR   = 0xff5050;

const BAR_COUNT          = 96;
const BAR_RING_RADIUS    = 1.50;
const OUTER_RING_RADIUS  = 1.70;
const MIDDLE_RING_RADIUS = 1.35;
const INNER_RING_RADIUS  = 1.00;
const SPHERE_RADIUS      = 0.55;

/**
 * The Jarvis holographic orb — canonical Iron Man 2 / Avengers 1 design.
 *
 * Composition:
 *   - Outer ring at r=1.7 with 24 tick-marks every 15°
 *   - Middle ring at r=1.35, dashed/segmented, counter-rotating
 *   - Inner ring at r=1.0, audio-pulsed radius
 *   - Wireframe icosahedron at r=0.55, detail=2 (canonical) — line segments
 *     only, no filled faces. This is the "energy core".
 *   - 96 radial rim bars between r=1.5 and outward, audio-reactive
 *   - Scanner sweep line that rotates around the outer ring when Thinking
 *
 * Per-state behavior:
 *   IDLE      — slow drift, dim, bars flat, sphere breathes 1–2%
 *   LISTENING — bars react to MIC level (uLevel from cpal), inner ring breathes
 *   THINKING  — scanner sweeps fast, rings counter-rotate fast, sphere spins
 *   SPEAKING  — bars dance to pseudo-speech envelope (multi-frequency sin
 *               modulation that mimics syllable rhythm — since macOS `say`
 *               doesn't expose live amplitude). Inner ring contracts on
 *               "syllable" onsets, sphere bass-pulses.
 *
 * Cinematic glow via post-process bloom. The orb only fills ~55% of the
 * canvas so bloom has plenty of transparent margin to fade naturally and
 * never reaches the window edge — no visible square.
 */
export function OrbScene(props: Props) {
  let container!: HTMLDivElement;
  let renderer: THREE.WebGLRenderer;
  let composer: EffectComposer;
  let scene: THREE.Scene;
  let camera: THREE.PerspectiveCamera;

  // Scene elements (assigned in onMount)
  let outerRing!: Line2;
  let middleRing!: Line2;
  let innerRing!: Line2;
  let wireSphere!: LineSegments2;
  let bars!: THREE.InstancedMesh;
  let barMat!: THREE.MeshBasicMaterial;
  let bloomEffect!: BloomEffect;
  // Per-material base line widths, kept so we can scale them with brightness
  const baseLineWidths = new Map<LineMaterial, number>();

  // Materials we need to recolor based on state
  const allLineMaterials: LineMaterial[] = [];

  // Eased state amounts (0..1)
  const amts = {
    listening: { cur: 0, target: 0 },
    thinking:  { cur: 0, target: 0 },
    speaking:  { cur: 0, target: 0 },
    error:     { cur: 0, target: 0 },
  };

  // Smooth color interpolation
  const colors = {
    hero:        new THREE.Color(CYAN_HERO),
    heroTarget:  new THREE.Color(CYAN_DIM),
    intensity:       0.7,
    intensityTarget: 0.7,
  };

  const start = performance.now();
  const resolution = new THREE.Vector2(1, 1);

  // ── Helpers ────────────────────────────────────────────────────────────
  function makeLineMat(color: number, opacity: number, width: number, dashed = false) {
    const m = new LineMaterial({
      color,
      linewidth: width,
      transparent: true,
      opacity,
      resolution: resolution.clone(),
      depthWrite: false,
      dashed,
      dashSize: dashed ? 0.08 : 0,
      gapSize:  dashed ? 0.05 : 0,
    });
    allLineMaterials.push(m);
    baseLineWidths.set(m, width);
    return m;
  }

  function buildRing(radius: number, segments: number, mat: LineMaterial): Line2 {
    const positions: number[] = [];
    for (let i = 0; i <= segments; i++) {
      const a = (i / segments) * Math.PI * 2;
      positions.push(Math.cos(a) * radius, Math.sin(a) * radius, 0);
    }
    const geo = new LineGeometry();
    geo.setPositions(positions);
    const line = new Line2(geo, mat);
    line.computeLineDistances();
    return line;
  }

  function buildTicks(
    radius: number, count: number, length: number, mat: LineMaterial,
  ): LineSegments2 {
    const positions: number[] = [];
    for (let i = 0; i < count; i++) {
      const a = (i / count) * Math.PI * 2;
      const x1 = Math.cos(a) * radius;
      const y1 = Math.sin(a) * radius;
      const x2 = Math.cos(a) * (radius + length);
      const y2 = Math.sin(a) * (radius + length);
      positions.push(x1, y1, 0, x2, y2, 0);
    }
    const geo = new LineSegmentsGeometry();
    geo.setPositions(positions);
    return new LineSegments2(geo, mat);
  }

  function buildWireSphere(radius: number, detail: number, mat: LineMaterial): LineSegments2 {
    const ico = new THREE.IcosahedronGeometry(radius, detail);
    const edges = new THREE.EdgesGeometry(ico);
    const positions = Array.from(edges.attributes.position.array as Float32Array);
    const geo = new LineSegmentsGeometry();
    geo.setPositions(positions);
    return new LineSegments2(geo, mat);
  }

  function buildBars(): { mesh: THREE.InstancedMesh; mat: THREE.MeshBasicMaterial } {
    // Bar: thin tall rectangle pivoting from its base. Translate the
    // geometry so y=0 is the bottom edge → instance scale.y scales length
    // outward from the pivot at the rim.
    const geo = new THREE.BoxGeometry(0.018, 1, 0.005);
    geo.translate(0, 0.5, 0);
    const mat = new THREE.MeshBasicMaterial({
      color: CYAN_HERO,
      transparent: true,
      opacity: 0.9,
      depthWrite: false,
      toneMapped: false,
    });
    const mesh = new THREE.InstancedMesh(geo, mat, BAR_COUNT);
    return { mesh, mat };
  }

  // ── State palette ──────────────────────────────────────────────────────
  function palette(s: string): { hero: number; intensity: number } {
    switch (s) {
      case "listening": return { hero: CYAN_HERO, intensity: 1.00 };
      case "thinking":  return { hero: CYAN_HERO, intensity: 1.10 };
      case "speaking":  return { hero: CYAN_HOT,  intensity: 1.25 };
      case "error":     return { hero: RED_ERROR, intensity: 1.10 };
      default:          return { hero: CYAN_DIM,  intensity: 0.65 };
    }
  }

  onMount(() => {
    const w = container.clientWidth;
    const h = container.clientHeight;
    resolution.set(w, h);

    renderer = new THREE.WebGLRenderer({
      antialias: true,
      alpha: true,
      premultipliedAlpha: false,
      powerPreference: "high-performance",
    });
    renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
    renderer.setSize(w, h);
    renderer.setClearColor(0x000000, 0);
    renderer.outputColorSpace = THREE.SRGBColorSpace;
    container.appendChild(renderer.domElement);

    scene = new THREE.Scene();
    camera = new THREE.PerspectiveCamera(40, w / h, 0.1, 100);
    // Distance set so the outer ring spans ~65% of the canvas — leaves
    // ample margin for bloom halo to fade without ever hitting the window
    // edge (avoids the previous "visible square" problem).
    camera.position.z = 6.6;

    // ── Build elements ──
    const outerMat = makeLineMat(CYAN_HERO, 0.85, 1.6);
    const middleMat = makeLineMat(CYAN_HERO, 0.65, 1.3, /* dashed */ true);
    const innerMat = makeLineMat(CYAN_HERO, 0.80, 1.5);
    const sphereMat = makeLineMat(CYAN_HERO, 0.85, 1.0);

    outerRing  = buildRing(OUTER_RING_RADIUS, 256, outerMat);
    middleRing = buildRing(MIDDLE_RING_RADIUS, 192, middleMat);
    innerRing  = buildRing(INNER_RING_RADIUS, 192, innerMat);
    wireSphere = buildWireSphere(SPHERE_RADIUS, 2, sphereMat);

    const built = buildBars();
    bars = built.mesh;
    barMat = built.mat;

    scene.add(outerRing, middleRing, innerRing, wireSphere, bars);

    // ── Post-processing bloom ──
    // Modest intensity + medium kernel — orb sits well inside canvas, so
    // halo never reaches the window edge. Alpha preserved.
    composer = new EffectComposer(renderer, {
      frameBufferType: THREE.HalfFloatType,
    });
    composer.addPass(new RenderPass(scene, camera));
    bloomEffect = new BloomEffect({
      intensity: BLOOM_BASE_INTENSITY,
      luminanceThreshold: 0.18,
      luminanceSmoothing: 0.35,
      kernelSize: KernelSize.MEDIUM,
      mipmapBlur: true,
    });
    composer.addPass(new EffectPass(camera, bloomEffect));

    const onResize = () => {
      const w2 = container.clientWidth;
      const h2 = container.clientHeight;
      renderer.setSize(w2, h2);
      composer.setSize(w2, h2);
      camera.aspect = w2 / h2;
      camera.updateProjectionMatrix();
      resolution.set(w2, h2);
      for (const m of allLineMaterials) m.resolution.set(w2, h2);
    };
    window.addEventListener("resize", onResize);
    onCleanup(() => window.removeEventListener("resize", onResize));

    const dummy = new THREE.Object3D();

    const loop = () => {
      const t = (performance.now() - start) / 1000;

      // Ease state amounts
      for (const k of Object.keys(amts) as Array<keyof typeof amts>) {
        const a = amts[k];
        const ease = a.target > a.cur ? 0.085 : 0.05;
        a.cur += (a.target - a.cur) * ease;
        if (Math.abs(a.target - a.cur) < 1e-4) a.cur = a.target;
      }

      // Ease palette
      colors.hero.lerp(colors.heroTarget, 0.08);
      colors.intensity += (colors.intensityTarget - colors.intensity) * 0.08;

      const lit = amts.listening.cur;
      const thk = amts.thinking.cur;
      const spk = amts.speaking.cur;
      const idle = Math.max(0, 1 - Math.max(lit, thk, spk));

      // State intensity = color/glow (palette-driven, makes states feel different).
      // User brightness = clarity (opacity + line width + INVERSE bloom).
      //   ▲ brightness → sharper, more opaque, less bloom haze
      //   ▼ brightness → softer, more atmospheric, more bloom halo
      const userB     = brightness();             // 0.3..2.0
      const intensity = colors.intensity * (props.intensityScale ?? 1.0);

      // Bloom inverse: high brightness = sharp/less haze, low brightness = atmospheric
      const bloomScale = Math.max(0.30, 1.55 - userB * 0.65); // 0.3→1.35, 1.0→0.90, 2.0→0.30
      bloomEffect.intensity = BLOOM_BASE_INTENSITY * bloomScale;

      // Line opacity multiplier — gives that "more solid against the background" feel
      const opacityMul = Math.min(1.0, Math.max(0.35, userB * 0.7 + 0.30));

      // Line width multiplier — slightly thicker at higher brightness for presence
      const widthMul = 0.75 + userB * 0.35;

      // Apply line-width scaling once per frame
      for (const [m, base] of baseLineWidths) {
        m.linewidth = base * widthMul;
      }

      // ── Rotation ──
      const baseSpin = 0.04;
      outerRing.rotation.z  = -t * (baseSpin + thk * 0.55 + spk * 0.04);
      middleRing.rotation.z =  t * (baseSpin * 1.8 + thk * 0.85 + spk * 0.06);
      innerRing.rotation.z  = -t * (baseSpin * 1.2 + thk * 0.4);

      wireSphere.rotation.y = t * (0.18 + thk * 0.9 + spk * 0.20);
      wireSphere.rotation.x = Math.sin(t * 0.5) * 0.25 + thk * t * 0.4;

      // ── Ring deformation + audio-reactive radii ──
      // THINKING: wavy sine deformation around the circumference
      //           (replaces the scanner sweep with a more organic motion).
      // LISTENING: rings expand uniformly with live mic level.
      // SPEAKING: rings pulse on syllable beats — the middle ring is the
      //           most visibly "talking" element.
      const lvl = level();
      const speakBeat  = Math.abs(Math.sin(t * 4.8)) * spk;
      const speakSyll  = Math.abs(Math.sin(t * 7.2)) * spk;

      // Outer ring — small wave + slight speak pulse
      updateRing(
        outerRing,
        OUTER_RING_RADIUS,
        256,
        /*waviness*/ 0.045 * thk,
        /*freq*/     8,
        /*phase*/    0.0,
        t,
        /*scale*/    1.0 + spk * speakBeat * 0.03,
      );

      // Middle ring — most voice-reactive (stretches/contracts with speech)
      updateRing(
        middleRing,
        MIDDLE_RING_RADIUS,
        192,
        /*waviness*/ 0.06 * thk,
        /*freq*/     6,
        /*phase*/    1.4,
        t,
        /*scale*/    1.0 + lit * lvl * 0.16 + spk * speakBeat * 0.10,
      );

      // Inner ring — fine ripple + tight pulse with voice
      updateRing(
        innerRing,
        INNER_RING_RADIUS,
        192,
        /*waviness*/ 0.04 * thk,
        /*freq*/     10,
        /*phase*/    2.7,
        t,
        /*scale*/    1.0 + lit * lvl * 0.10 + spk * speakSyll * 0.08 + Math.sin(t * 0.7) * 0.01,
      );

      // ── Wire sphere "bass" pulse ──
      const sphereBreath = 1 + Math.sin(t * 0.8) * 0.02;
      const sphereBass   = Math.abs(Math.sin(t * 2.2)) * 0.10 * spk;
      const sphereListen = lvl * 0.15 * lit;
      wireSphere.scale.setScalar(sphereBreath + sphereBass + sphereListen);

      // ── Bars ──
      // Per-bar: each bar's length depends on state + phase + audio.
      // Pseudo-speech envelope: combined slow "word" rhythm + faster "syllable"
      // rhythm + per-bar fast variation → reads as natural talking.
      const wordRhythm = Math.abs(Math.sin(t * 2.6));
      const syllRhythm = Math.abs(Math.sin(t * 5.4));
      const speakEnv   = wordRhythm * syllRhythm;

      for (let i = 0; i < BAR_COUNT; i++) {
        const angle = (i / BAR_COUNT) * Math.PI * 2;
        const cx = Math.cos(angle), cy = Math.sin(angle);

        // Per-bar phase offset so they never move in lockstep
        const phase = i * 0.37;

        // IDLE: barely-there flicker
        let lenIdle = 0.02 + (Math.sin(t * 0.5 + phase) * 0.5 + 0.5) * 0.008;

        // LISTENING: amplitude tracks live mic
        const listenVar = Math.sin(t * 14 + phase) * 0.5 + 0.5;
        let lenListen = 0.02 + lvl * 1.6 * (0.35 + listenVar * 0.75);
        lenListen = Math.min(lenListen, 0.35);

        // THINKING: low rolling sine wave around the ring (Mexican-wave feel)
        const wave = Math.sin(angle * 3 + t * 4.5) * 0.5 + 0.5;
        let lenThink = 0.025 + wave * 0.04;

        // SPEAKING: dance to the pseudo-speech envelope, with per-bar variation
        const barPhase = Math.sin(t * 9.5 + phase) * 0.5 + 0.5;
        const barPhase2 = Math.sin(t * 17 + phase * 1.7) * 0.5 + 0.5;
        let lenSpeak = 0.025 + speakEnv * (barPhase * 0.55 + barPhase2 * 0.25) * 0.34;

        // Blend by state amounts
        const len =
          lenIdle   * idle +
          lenListen * lit  +
          lenThink  * thk  +
          lenSpeak  * spk;

        // Position at rim, oriented so the bar's local +Y points outward
        dummy.position.set(cx * BAR_RING_RADIUS, cy * BAR_RING_RADIUS, 0);
        dummy.rotation.set(0, 0, angle - Math.PI / 2);
        dummy.scale.set(1, Math.max(0.015, len), 1);
        dummy.updateMatrix();
        bars.setMatrixAt(i, dummy.matrix);
      }
      bars.instanceMatrix.needsUpdate = true;

      // Bars share the hero color (which is already hot-cyan when speaking
      // via palette()). Opacity scales with intensity so they're brighter
      // during active states.
      barMat.color.copy(colors.hero);
      barMat.opacity = 0.55 + 0.35 * intensity;

      // ── Apply current color/intensity to line materials ──
      for (const m of allLineMaterials) {
        m.color.copy(colors.hero);
      }
      // Bump outer/inner brightness via material opacity scaling
      outerRing.material.opacity = clamp01(0.85 * intensity * opacityMul);
      innerRing.material.opacity = clamp01(0.82 * intensity * opacityMul);
      middleRing.material.opacity = clamp01(0.62 * intensity * opacityMul);
      wireSphere.material.opacity = clamp01(0.88 * intensity * opacityMul);

      composer.render();
    };
    // Use renderer.setAnimationLoop instead of manual RAF — handles vsync
    // alignment better and avoids drift on retina displays.
    renderer.setAnimationLoop(loop);
  });

  createEffect(() => {
    const s = state();
    amts.listening.target = s === "listening" ? 1 : 0;
    amts.thinking.target  = s === "thinking"  ? 1 : 0;
    amts.speaking.target  = s === "speaking"  ? 1 : 0;
    amts.error.target     = s === "error"     ? 1 : 0;

    const p = palette(s);
    colors.heroTarget.setHex(p.hero);
    colors.intensityTarget = p.intensity;
  });

  onCleanup(() => {
    renderer?.setAnimationLoop(null);
    composer?.dispose();
    renderer?.dispose();
  });

  const sz = typeof props.size === "number" ? `${props.size}px` : (props.size ?? "100%");

  return (
    <div
      ref={container!}
      style={{
        width: sz,
        height: sz,
        position: "absolute",
        inset: 0,
        "pointer-events": "none",
        // Hard-clip the WebGL canvas to a circle — overflow:hidden on the
        // CSS parent isn't reliably honored for WebGL surfaces in WKWebView,
        // which produced the visible "square" of bloom haze in the HUD.
        "clip-path": "circle(50%)",
        "-webkit-clip-path": "circle(50%)",
      }}
    />
  );
}

function clamp01(x: number): number {
  return x < 0 ? 0 : x > 1 ? 1 : x;
}

/**
 * Update a Line2's circle radius/waviness in-place — writes directly into
 * the existing InstancedInterleavedBuffer instead of reallocating one via
 * setPositions(). 3 rings × 60 fps × 256 segments adds up; the old path
 * caused visible GC stutter. This path is ~10× cheaper.
 */
function updateRing(
  line: Line2,
  baseRadius: number,
  segments: number,
  waviness: number,
  freq: number,
  phaseShift: number,
  t: number,
  scale: number,
) {
  const buf = (line.geometry as any).attributes.instanceStart.data as THREE.InstancedInterleavedBuffer;
  const arr = buf.array as Float32Array;

  // Layout: per segment, 6 floats = [startX, startY, startZ, endX, endY, endZ]
  // Compute each point once, share with the previous segment's end.
  let prevX = 0, prevY = 0;
  for (let i = 0; i <= segments; i++) {
    const a = (i / segments) * Math.PI * 2;
    const r = (baseRadius + Math.sin(a * freq + t * 2.5 + phaseShift) * waviness) * scale;
    const x = Math.cos(a) * r;
    const y = Math.sin(a) * r;
    if (i > 0) {
      const o = (i - 1) * 6;
      arr[o + 0] = prevX;
      arr[o + 1] = prevY;
      arr[o + 2] = 0;
      arr[o + 3] = x;
      arr[o + 4] = y;
      arr[o + 5] = 0;
    }
    prevX = x;
    prevY = y;
  }
  buf.needsUpdate = true;
}
