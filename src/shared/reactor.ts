/**
 * The JARVIS "reactor" — a rotating geodesic wireframe sphere with
 * state-reactive rings, rendered on Canvas2D. Lifted from the HUD design
 * handoff prototype (design_handoff_jarvis_hud) — keep the math in sync
 * with the design's motion table if the handoff is revised.
 *
 * Framework-agnostic: no Solid imports. Components own a <canvas> and call
 * ReactorRenderer.draw(canvas, ...) from a requestAnimationFrame loop.
 */

import type { JarvisState } from "./state";

/** Sphere/ring base color per assistant state (RGB triplets). */
const CYAN: [number, number, number] = [95, 233, 245];
const RED: [number, number, number] = [255, 107, 94];

interface SpherePoint {
  x: number;
  y: number;
  z: number;
}

export class ReactorRenderer {
  private pts: [number, number, number][] = [];
  private edges: [number, number][] = [];
  private sphereRot = 0;
  private sphereTilt = 0;
  private time = 0;
  /** Smoothed audio level — meter ballistics (fast attack, slow release) so
   *  the stepped 45ms TTS envelope reads as flowing loudness, not flicker. */
  private lvl = 0;

  constructor(nodeCount = 92) {
    this.buildSphere(nodeCount);
  }

  /** Fibonacci-sphere distribution; each point linked to its 5 nearest neighbours. */
  private buildSphere(n: number) {
    const pts: [number, number, number][] = [];
    const GA = Math.PI * (3 - Math.sqrt(5));
    for (let i = 0; i < n; i++) {
      const y = 1 - (i / (n - 1)) * 2;
      const r = Math.sqrt(Math.max(0, 1 - y * y));
      const th = i * GA;
      pts.push([Math.cos(th) * r, y, Math.sin(th) * r]);
    }
    const edges: [number, number][] = [];
    const seen = new Set<string>();
    for (let i = 0; i < n; i++) {
      const d: [number, number][] = [];
      for (let j = 0; j < n; j++) {
        if (i === j) continue;
        const dx = pts[i][0] - pts[j][0];
        const dy = pts[i][1] - pts[j][1];
        const dz = pts[i][2] - pts[j][2];
        d.push([dx * dx + dy * dy + dz * dz, j]);
      }
      d.sort((a, b) => a[0] - b[0]);
      for (let k = 0; k < 5; k++) {
        const j = d[k][1];
        const key = i < j ? `${i}-${j}` : `${j}-${i}`;
        if (!seen.has(key)) {
          seen.add(key);
          edges.push([i, j]);
        }
      }
    }
    this.pts = pts;
    this.edges = edges;
  }

  /** Advance the shared animation clock — call once per frame, before draw(). */
  tick(state: JarvisState) {
    const speed =
      state === "thinking" ? 0.013
      : state === "listening" ? 0.006
      : state === "speaking" ? 0.011
      : 0.0032;
    this.sphereRot += speed;
    this.sphereTilt += speed * 0.35;
    this.time += 0.016;
  }

  /**
   * Render one frame. `level` (0..1, live mic/TTS level) nudges intensity so
   * the reactor stays audio-reactive like the old WebGL orb; `bright` is the
   * user's ⇧⌘+/⇧⌘- multiplier (0.3..2.0).
   */
  draw(canvas: HTMLCanvasElement, state: JarvisState, withRings: boolean, level = 0, bright = 1) {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    if (!w || !h) return;
    // Re-check BOTH dimensions: a width-only check leaves a square CSS canvas
    // stuck at the default 150px backing height (design-handoff gotcha).
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    const cx = w / 2;
    const cy = h / 2;
    const err = state === "error";
    const C = err ? RED : CYAN;
    // Meter ballistics: jump up fast on onsets (~4 frames), decay slowly
    // (~400ms tail) — raw envelope steps never reach the pixels directly.
    this.lvl += (level - this.lvl) * (level > this.lvl ? 0.45 : 0.06);
    const lv = this.lvl;
    const base =
      state === "idle" ? 0.5
      : state === "listening" ? 0.9
      : state === "thinking" ? 0.82
      : state === "speaking" ? 1
      : 0.7;
    const intensity = Math.min(1.35, (base + lv * 0.2) * bright);
    const rad = Math.min(w, h) * 0.125;
    const t = this.time;

    if (withRings) this.drawRings(ctx, cx, cy, rad, state, t, intensity, C, lv);

    const ay = this.sphereRot;
    const ax = Math.sin(this.sphereTilt) * 0.28;
    const proj: SpherePoint[] = this.pts.map((p) => {
      const [x, y, z] = p;
      const x1 = x * Math.cos(ay) + z * Math.sin(ay);
      const z1 = -x * Math.sin(ay) + z * Math.cos(ay);
      const y1 = y * Math.cos(ax) - z1 * Math.sin(ax);
      const z2 = y * Math.sin(ax) + z1 * Math.cos(ax);
      return { x: cx + x1 * rad, y: cy + y1 * rad, z: z2 };
    });

    ctx.lineWidth = 1;
    for (const [i, j] of this.edges) {
      const a = proj[i];
      const b = proj[j];
      const za = (a.z + 1) / 2;
      const zb = (b.z + 1) / 2;
      const al = (0.07 + ((za + zb) / 2) * 0.5) * intensity;
      ctx.strokeStyle = `rgba(${C[0]},${C[1]},${C[2]},${al})`;
      ctx.beginPath();
      ctx.moveTo(a.x, a.y);
      ctx.lineTo(b.x, b.y);
      ctx.stroke();
    }
    for (const p of proj) {
      const z = (p.z + 1) / 2;
      ctx.fillStyle = `rgba(${C[0]},${C[1]},${C[2]},${(0.2 + z * 0.7) * intensity})`;
      ctx.beginPath();
      ctx.arc(p.x, p.y, z * 1.7 + 0.4, 0, 7);
      ctx.fill();
    }

    const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, rad * 1.5);
    g.addColorStop(0, `rgba(${C[0]},${C[1]},${C[2]},${0.2 * intensity})`);
    g.addColorStop(1, "rgba(0,0,0,0)");
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, w, h);
  }

  /** Per-state ring rendering — see the motion table in the design handoff. */
  private drawRings(
    ctx: CanvasRenderingContext2D,
    cx: number,
    cy: number,
    rad: number,
    state: JarvisState,
    t: number,
    intensity: number,
    C: [number, number, number],
    level: number,
  ) {
    const col = (a: number) => `rgba(${C[0]},${C[1]},${C[2]},${a})`;
    const ringR = [rad * 1.85, rad * 2.55, rad * 3.25];

    if (state === "thinking") {
      // rings distort into slow organic blobs
      ringR.forEach((R, k) => {
        ctx.beginPath();
        for (let d = 0; d <= 360; d += 3) {
          const th = (d * Math.PI) / 180;
          const rr = R * (1 + 0.06 * Math.sin(6 * th + t * 3 + k) + 0.038 * Math.sin(9 * th - t * 2));
          const x = cx + Math.cos(th) * rr;
          const y = cy + Math.sin(th) * rr;
          if (d === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.closePath();
        ctx.lineWidth = 1.2;
        ctx.strokeStyle = col(0.18 + 0.12 * intensity);
        ctx.stroke();
      });
    } else if (state === "speaking") {
      // Speech drive: the smoothed REAL TTS loudness (level is pre-filtered
      // with attack/release ballistics in draw()) riding over a synthetic
      // speech cadence — syllable-rate bursts (FM for irregularity) shaped by
      // a slower word-gap swell. The 0.35 floor means troughs DIM the bars,
      // never black them out — no flicker, just rhythm.
      const burst = 0.5 + 0.5 * Math.sin(t * 7.6 + 1.2 * Math.sin(t * 3.4));
      const gap = 0.62 + 0.38 * Math.sin(t * 1.8 + 2.1);
      const cadence = Math.pow(burst, 1.3) * gap;
      const voice = Math.min(1, level * 1.3);
      const drive = 0.35 + 0.65 * Math.max(cadence * 0.7, voice);
      // soft radiating sound circles behind the bars (steady alpha — the bars
      // carry the rhythm; modulating these too reads as global blinking)
      for (let k = 0; k < 3; k++) {
        const ph = (t * 0.8 + k / 3) % 1;
        const pr = rad * 2.0 + ph * rad * 2.0;
        ctx.lineWidth = 1.2;
        ctx.strokeStyle = col((1 - ph) * 0.16 * intensity);
        ctx.beginPath();
        ctx.arc(cx, cy, pr, 0, 7);
        ctx.stroke();
      }
      // baseline ring the equalizer columns stand on
      const baseR = rad * 2.15;
      ctx.lineWidth = 1.2;
      ctx.strokeStyle = col(0.16 * intensity);
      ctx.beginPath();
      ctx.arc(cx, cy, baseR, 0, 7);
      ctx.stroke();
      // circular equalizer — thin rounded columns, outward only
      const n = 52;
      const bw = ((2 * Math.PI * baseR) / n) * 0.4;
      ctx.lineCap = "round";
      ctx.shadowColor = `rgba(${C[0]},${C[1]},${C[2]},${0.28 * intensity})`;
      ctx.shadowBlur = 4;
      for (let i = 0; i < n; i++) {
        const th = (i / n) * Math.PI * 2;
        // smooth low-spatial-frequency spectrum scrolling at speech pace
        let s = 0.5 + 0.5 * Math.sin(i * 0.55 - t * 5.6);
        s *= 0.6 + 0.4 * Math.sin(i * 0.27 + t * 3.3);
        s = Math.pow(s, 0.75) * drive;
        const hh = rad * (0.12 + 0.78 * s * intensity);
        const ux = Math.cos(th);
        const uy = Math.sin(th);
        ctx.lineWidth = bw;
        // narrow alpha swing on a raised floor: loudness moves the bar HEIGHTS,
        // brightness only breathes — the dark↔glow strobe came from swinging both
        ctx.strokeStyle = col(0.3 + 0.22 * s * intensity);
        ctx.beginPath();
        ctx.moveTo(cx + ux * baseR, cy + uy * baseR);
        ctx.lineTo(cx + ux * (baseR + hh), cy + uy * (baseR + hh));
        ctx.stroke();
      }
      ctx.shadowBlur = 0;
      ctx.lineCap = "butt";
    } else if (state === "listening") {
      // static guide rings + a comet that orbits and spirals between rings
      for (const R of ringR) {
        ctx.lineWidth = 1;
        ctx.strokeStyle = col(0.12 + 0.1 * intensity);
        ctx.beginPath();
        ctx.arc(cx, cy, R, 0, 7);
        ctx.stroke();
      }
      const ringMult = [3.25, 2.55, 1.85];
      const seq = [0, 1, 2, 1]; // outer → middle → inner → middle, ping-pong
      const hold = 2.8;
      const trans = 1.7;
      const seg = hold + trans;
      const omega = 2.1;
      const cyc = seg * seq.length;
      const ease = (e: number) => e * e * (3 - 2 * e);
      const pathAt = (tt: number) => {
        const u = ((tt % cyc) + cyc) % cyc;
        const si = Math.floor(u / seg);
        const f = u - si * seg;
        const cur = ringMult[seq[si]];
        const nxt = ringMult[seq[(si + 1) % seq.length]];
        const r = f < hold ? cur : cur + (nxt - cur) * ease((f - hold) / trans);
        const ang = tt * omega;
        return { x: cx + Math.cos(ang) * rad * r, y: cy + Math.sin(ang) * rad * r };
      };
      const N = 42;
      const dt = 0.028;
      ctx.lineCap = "round";
      let prev = pathAt(t - N * dt);
      for (let j = N - 1; j >= 0; j--) {
        const p = pathAt(t - j * dt);
        const frac = 1 - j / N;
        ctx.lineWidth = 0.4 + 2.6 * frac;
        ctx.strokeStyle = col(frac * frac * 0.85 * intensity);
        ctx.beginPath();
        ctx.moveTo(prev.x, prev.y);
        ctx.lineTo(p.x, p.y);
        ctx.stroke();
        prev = p;
      }
      const hp = pathAt(t);
      ctx.shadowColor = `rgba(${C[0]},${C[1]},${C[2]},${0.85 * intensity})`;
      ctx.shadowBlur = 10;
      ctx.fillStyle = col(0.95 * intensity);
      ctx.beginPath();
      ctx.arc(hp.x, hp.y, 3, 0, 7);
      ctx.fill();
      ctx.shadowBlur = 0;
      ctx.lineCap = "butt";
    } else {
      // idle (and error): three concentric rings with marching dashes;
      // error blinks them red at a slow pulse
      const blink = state === "error" ? 0.6 + 0.4 * Math.sin(t * 6) : 1;
      ringR.forEach((R, k) => {
        ctx.save();
        ctx.setLineDash(k === 1 ? [6, 9] : k === 2 ? [2, 8] : []);
        ctx.lineDashOffset = -t * 22 * (k + 1) * (k % 2 ? 1 : -1);
        ctx.lineWidth = 1;
        ctx.strokeStyle = col((0.1 + 0.16 * intensity) * blink);
        ctx.beginPath();
        ctx.arc(cx, cy, R, 0, 7);
        ctx.stroke();
        ctx.restore();
      });
    }
  }
}
