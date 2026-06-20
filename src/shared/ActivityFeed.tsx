import { Show, For } from "solid-js";
import { activity, type ActivityItem, type ActivityStatus, type Danger } from "./state";
import "./confirm.css";

const GLYPH: Record<ActivityStatus, string> = {
  awaiting: "◷",
  running: "▶",
  ok: "✓",
  error: "✕",
  denied: "⊘",
};

function dangerClass(d: Danger): string {
  return `jv-danger-${d}`;
}

/**
 * Compact live list of recent autonomous-control actions. Newest first,
 * capped to a handful of visible rows. Renders nothing meaningful when empty
 * (a faint placeholder).
 */
export function ActivityFeed(props: { limit?: number }) {
  const items = () => activity().slice(0, props.limit ?? 8);

  return (
    <Show when={items().length > 0} fallback={<div class="jv-activity-empty">no actions yet</div>}>
      <div class="jv-activity">
        <For each={items()}>{(item) => <Row item={item} />}</For>
      </div>
    </Show>
  );
}

function Row(props: { item: ActivityItem }) {
  const it = () => props.item;
  return (
    <div class={`jv-activity-row ${dangerClass(it().danger)}`}>
      <span class={`jv-activity-glyph jv-glyph-${it().status}`}>{GLYPH[it().status]}</span>
      <span class="jv-activity-summary" title={it().summary}>
        {it().summary}
      </span>
      <span class="jv-activity-dot" title={it().danger} />
    </div>
  );
}
