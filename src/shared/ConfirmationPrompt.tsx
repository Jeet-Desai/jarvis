import { Show } from "solid-js";
import { pendingConfirm, resolveConfirm, type Danger } from "./state";
import "./confirm.css";

const DANGER_LABEL: Record<Danger, string> = {
  read_only: "READ ONLY",
  mutating: "MUTATING",
  destructive: "DESTRUCTIVE",
};

function dangerClass(d: Danger): string {
  return `jv-danger-${d}`;
}

interface Props {
  /** "hud" = full glassy card with buttons; "orb" = compact text-focused form. */
  variant?: "hud" | "orb";
}

/**
 * Shown when a computer-control action awaits the user's approval.
 *
 * On the HUD window this renders a centered glassy card with Approve/Deny
 * buttons. On the orb window (which is click-through, so buttons can't be
 * clicked) it renders a compact text-focused card that leans on the
 * ⌘⇧Y / ⌘⇧S hotkeys.
 */
export function ConfirmationPrompt(props: Props) {
  const variant = () => props.variant ?? "hud";

  return (
    <Show when={pendingConfirm()}>
      {(pc) => (
        <Show when={variant() === "hud"} fallback={<OrbConfirm danger={pc().danger} summary={pc().summary} />}>
          <div class={`jv-confirm-overlay ${dangerClass(pc().danger)}`}>
            <div class="jv-confirm-card">
              <div class="jv-confirm-head">
                <span class="jv-badge">
                  <span class="jv-badge-dot" />
                  {DANGER_LABEL[pc().danger]}
                </span>
                <span class="jv-confirm-title">Confirm Action</span>
              </div>
              <div class="jv-confirm-summary">{pc().summary}</div>
              <div class="jv-confirm-actions">
                <button class="jv-btn jv-btn-approve" onClick={() => resolveConfirm(pc().id, true)}>
                  Approve
                </button>
                <button class="jv-btn jv-btn-deny" onClick={() => resolveConfirm(pc().id, false)}>
                  Deny
                </button>
              </div>
              <div class="jv-confirm-hint">
                <kbd>⌘⇧Y</kbd> approve · <kbd>⌘⇧S</kbd> deny
              </div>
            </div>
          </div>
        </Show>
      )}
    </Show>
  );
}

function OrbConfirm(props: { danger: Danger; summary: string }) {
  return (
    <div class={`jv-confirm-orb ${dangerClass(props.danger)}`}>
      <div class="jv-confirm-orb-card">
        <div class="jv-confirm-orb-badge">⚠ {DANGER_LABEL[props.danger]} · APPROVAL NEEDED</div>
        <div class="jv-confirm-orb-summary">{props.summary}</div>
        <div class="jv-confirm-orb-hint">
          <kbd>⌘⇧Y</kbd> approve · <kbd>⌘⇧S</kbd> deny
        </div>
      </div>
    </div>
  );
}
