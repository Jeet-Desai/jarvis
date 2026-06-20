import { onMount, createSignal, onCleanup } from "solid-js";
import { state, transcript, response, level, policyMode, initStateBridge } from "../shared/state";
import { OrbScene } from "../shared/OrbScene";
import { ConfirmationPrompt } from "../shared/ConfirmationPrompt";
import { ActivityFeed } from "../shared/ActivityFeed";
import "./hud.css";

export function Hud() {
  const [now, setNow] = createSignal(formatTime());

  onMount(() => {
    initStateBridge();
    const id = setInterval(() => setNow(formatTime()), 1000);
    onCleanup(() => clearInterval(id));
  });

  return (
    <div class="hud-root">
      {/* Corner brackets — Iron Man style */}
      <div class="hud-bracket tl" />
      <div class="hud-bracket tr" />
      <div class="hud-bracket bl" />
      <div class="hud-bracket br" />

      {/* Top center status bar */}
      <div class="hud-top">
        <div class="hud-tag">
          <span class="hud-dot" data-state={state()} />
          JARVIS // {state().toUpperCase()}
        </div>
      </div>

      {/* Top-right clock + date */}
      <div class="hud-clock">
        <div class="hud-clock-time">{now()}</div>
        <div class="hud-clock-date">{formatDate()}</div>
      </div>

      {/* Top-left system status */}
      <div class="hud-sysinfo">
        <div class="hud-sysrow"><span>SYS</span><b>ONLINE</b></div>
        <div class="hud-sysrow"><span>MIC</span><b>READY</b></div>
        <div class="hud-sysrow">
          <span>MODE</span>
          <b class="hud-mode" data-mode={policyMode()}>{(policyMode() || "—").toUpperCase()}</b>
        </div>
      </div>

      {/* Left activity feed — live autonomous-control actions */}
      <div class="hud-activity-panel">
        <div class="hud-panel-label">ACTIVITY</div>
        <ActivityFeed limit={8} />
      </div>

      {/* Large center orb */}
      <div class="hud-orb-wrap">
        <div class="hud-orb-ring" />
        <div class="hud-orb-ring outer" />
        <div class="hud-orb">
          <OrbScene size="100%" intensityScale={1.2} />
        </div>
        {/* Audio level bar around the orb */}
        <LevelArc />
      </div>


      {/* Conversation panel */}
      <div class="hud-bottom">
        <div class="hud-panel">
          <div class="hud-panel-label">YOU</div>
          <div class="hud-panel-text">{transcript() || <span class="hud-placeholder">— press Cmd+Shift+J and speak —</span>}</div>
        </div>
        <div class="hud-panel jarvis">
          <div class="hud-panel-label">JARVIS</div>
          <div class="hud-panel-text">{response() || <span class="hud-placeholder">— standing by —</span>}</div>
        </div>
      </div>

      {/* Confirmation overlay — sits above everything */}
      <ConfirmationPrompt variant="hud" />
    </div>
  );
}

function LevelArc() {
  // Show a thin live bar reflecting current input audio level
  return (
    <div class="hud-level">
      <div class="hud-level-fill" style={{ width: `${Math.min(100, level() * 220)}%` }} />
    </div>
  );
}

function formatTime() {
  const d = new Date();
  const hh = d.getHours().toString().padStart(2, "0");
  const mm = d.getMinutes().toString().padStart(2, "0");
  const ss = d.getSeconds().toString().padStart(2, "0");
  return `${hh}:${mm}:${ss}`;
}

function formatDate() {
  const d = new Date();
  return d.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" }).toUpperCase();
}
