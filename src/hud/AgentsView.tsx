import { createSignal, createEffect, onMount, onCleanup, Show, Index } from "solid-js";
import {
  agents,
  spawnAgent,
  stopAgent,
  instructAgent,
  fetchAgentStatus,
  type AgentItem,
  type AgentEvent,
} from "../shared/state";
import { agentMeta, fmtCost, fmtRuntime } from "../shared/agentMeta";
import { Reactor } from "../shared/ReactorCanvas";

interface Props {
  selectedId: string | null;
  onSelect: (id: string | null) => void;
}

/**
 * AGENTS — the orchestration space. Agents orbit the core on two
 * counter-rotating rings (Grid fallback for big fleets); selecting one opens
 * the detail panel with its live output and real controls (instruct / stop —
 * these call the orchestrator sidecar via dedicated Tauri commands).
 */
export function AgentsView(props: Props) {
  const [layout, setLayout] = createSignal<"orbit" | "grid">("orbit");
  const [paused, setPaused] = createSignal(false);
  const [spawnOpen, setSpawnOpen] = createSignal(false);
  const [spawnTask, setSpawnTask] = createSignal("");
  const [spawnBusy, setSpawnBusy] = createSignal(false);
  const [actionErr, setActionErr] = createSignal("");

  const sel = () => agents().find((a) => a.id === props.selectedId) ?? null;

  async function doSpawn() {
    const task = spawnTask().trim();
    if (!task || spawnBusy()) return;
    setSpawnBusy(true);
    setActionErr("");
    try {
      await spawnAgent(task);
      setSpawnOpen(false);
      setSpawnTask("");
      setLayout("orbit");
      setPaused(false);
    } catch (e) {
      setActionErr(String(e));
    } finally {
      setSpawnBusy(false);
    }
  }

  return (
    <div class="agents-view">
      {/* header */}
      <div class="agents-header">
        <div>
          <div class="agents-title">◎ AGENT ORCHESTRATION</div>
          <div class="agents-subtitle">{agents().length} AGENTS IN CLUSTER · LIVE</div>
        </div>
        <div class="agents-controls">
          <Show when={actionErr()}>
            <span class="agents-err" title={actionErr()}>
              {actionErr()}
            </span>
          </Show>
          <div class="agents-seg">
            <button classList={{ on: layout() === "orbit" }} onClick={() => setLayout("orbit")}>
              ORBIT
            </button>
            <button classList={{ on: layout() === "grid" }} onClick={() => setLayout("grid")}>
              GRID
            </button>
          </div>
          <button class="agents-freeze" onClick={() => setPaused((p) => !p)}>
            {paused() ? "▶ ROTATE" : "⏸ FREEZE"}
          </button>
          <Show
            when={spawnOpen()}
            fallback={
              <button class="agents-spawn" onClick={() => setSpawnOpen(true)}>
                + SPAWN AGENT
              </button>
            }
          >
            <div class="agents-spawn-form">
              <input
                placeholder="task for the new agent… (Enter to spawn)"
                value={spawnTask()}
                disabled={spawnBusy()}
                onInput={(e) => setSpawnTask(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") doSpawn();
                  if (e.key === "Escape") setSpawnOpen(false);
                }}
                ref={(el) => setTimeout(() => el.focus(), 0)}
              />
              <button class="agents-spawn" disabled={spawnBusy()} onClick={doSpawn}>
                {spawnBusy() ? "SPAWNING…" : "GO"}
              </button>
            </div>
          </Show>
        </div>
      </div>

      {/* body: stage + detail panel */}
      <div class="agents-body">
        <div class="agents-stage">
          <Show when={layout() === "orbit"} fallback={<GridStage selectedId={props.selectedId} onSelect={props.onSelect} />}>
            <OrbitStage paused={paused()} selectedId={props.selectedId} onSelect={props.onSelect} />
          </Show>
        </div>
        <DetailPanel agent={sel()} onError={setActionErr} />
      </div>
    </div>
  );
}

// ── orbit stage ──────────────────────────────────────────────────────────────

function OrbitStage(props: { paused: boolean; selectedId: string | null; onSelect: (id: string) => void }) {
  let innerRing!: HTMLDivElement;
  let outerRing!: HTMLDivElement;
  const angles = [0, 0];

  onMount(() => {
    let raf = requestAnimationFrame(function loop() {
      raf = requestAnimationFrame(loop);
      if (!props.paused) {
        angles[0] += 0.12;
        angles[1] -= 0.085;
      }
      // Rotate each ring; counter-rotate every node so its label stays upright.
      [innerRing, outerRing].forEach((ring, idx) => {
        if (!ring) return;
        ring.style.transform = `rotate(${angles[idx]}deg)`;
        ring.querySelectorAll<HTMLElement>(".agent-node").forEach((node) => {
          const a = Number(node.dataset.angle) || 0;
          node.style.transform = `rotate(${-(angles[idx] + a)}deg)`;
        });
      });
    });
    onCleanup(() => cancelAnimationFrame(raf));
  });

  const inner = () => agents().slice(0, 3);
  const outer = () => agents().slice(3);

  return (
    <div class="orbit-root">
      <div class="orbit-sweep" />
      <div class="orbit-guide g1" />
      <div class="orbit-guide g2" />
      <div class="orbit-guide g3" />
      <div class="orbit-core">
        <Reactor size="200px" withRings={false} />
      </div>
      <div class="orbit-ring" ref={innerRing}>
        <Index each={inner()}>
          {(a, i) => (
            <OrbitNode
              agent={a()}
              angle={(i / Math.max(1, inner().length)) * 360}
              radius={152}
              selected={a().id === props.selectedId}
              onSelect={() => props.onSelect(a().id)}
            />
          )}
        </Index>
      </div>
      <div class="orbit-ring" ref={outerRing}>
        <Index each={outer()}>
          {(a, i) => (
            <OrbitNode
              agent={a()}
              angle={(i / Math.max(1, outer().length)) * 360 + 24}
              radius={256}
              selected={a().id === props.selectedId}
              onSelect={() => props.onSelect(a().id)}
            />
          )}
        </Index>
      </div>
    </div>
  );
}

function OrbitNode(props: { agent: AgentItem; angle: number; radius: number; selected: boolean; onSelect: () => void }) {
  const m = () => agentMeta(props.agent);
  return (
    <div class="orbit-slot" style={{ transform: `rotate(${props.angle}deg) translate(0, ${-props.radius}px)` }}>
      <div class="agent-node" data-angle={props.angle}>
        <div
          class="agent-node-disc"
          classList={{ selected: props.selected, spawning: props.agent.status === "spawning" }}
          style={{ "--accent": m().c, "--soft": m().soft }}
          onClick={props.onSelect}
        >
          <div class="agent-node-icon">
            <span class="agent-node-dot" classList={{ pulse: m().pulse, blink: m().blink }} />
          </div>
          <div class="agent-node-name">{props.agent.name}</div>
          <div class="agent-node-state">{m().detached ? "DETACHED" : m().label}</div>
        </div>
      </div>
    </div>
  );
}

// ── grid stage ───────────────────────────────────────────────────────────────

function GridStage(props: { selectedId: string | null; onSelect: (id: string) => void }) {
  const [nowMs, setNowMs] = createSignal(Date.now());
  onMount(() => {
    const id = setInterval(() => setNowMs(Date.now()), 1000);
    onCleanup(() => clearInterval(id));
  });

  return (
    <div class="grid-root">
      <div class="grid-cards">
        <Index each={agents()}>
          {(a) => {
            const m = () => agentMeta(a());
            return (
              <div
                class="grid-card"
                classList={{ selected: a().id === props.selectedId }}
                style={{ "--accent": m().c, "--soft": m().soft }}
                onClick={() => props.onSelect(a().id)}
              >
                <div class="grid-card-top">
                  <span class="grid-card-id">
                    <span class="agent-node-dot" classList={{ pulse: m().pulse, blink: m().blink }} />
                    <span class="grid-card-name">{a().name}</span>
                  </span>
                  <span class="grid-card-state">{m().detached ? "DETACHED" : m().label}</span>
                </div>
                <div class="grid-card-task">{a().last || "—"}</div>
                <div class="grid-card-foot">
                  <span>{fmtRuntime(a(), nowMs())}</span>
                  <span>{fmtCost(a())}</span>
                </div>
              </div>
            );
          }}
        </Index>
      </div>
    </div>
  );
}

// ── detail panel ─────────────────────────────────────────────────────────────

// How each event kind renders in the console.
const EVENT_GLYPH: Record<string, string> = {
  say: ">",
  tool: "⚙",
  error: "✕",
  qa_loop: "◈",
  announce: "◉",
  spawn: "●",
  instruct: "≫",
  stop: "■",
};

function DetailPanel(props: { agent: AgentItem | null; onError: (msg: string) => void }) {
  const [nowMs, setNowMs] = createSignal(Date.now());
  const [revealed, setRevealed] = createSignal(0);
  const [instruction, setInstruction] = createSignal("");
  const [busy, setBusy] = createSignal<"" | "stop" | "instruct">("");
  // Full event history from get_agent_status(full=true) — the roster's `last`
  // is a 200-char fragment, so the console is fetched, not accumulated.
  const [events, setEvents] = createSignal<AgentEvent[] | null>(null);
  let consoleEl: HTMLDivElement | undefined;
  let lastSelId: string | null = null;

  const streaming = () => {
    const a = props.agent;
    return !!a && a.live !== false && (a.status === "working" || a.status === "spawning");
  };
  const fullLog = () => {
    const evs = events();
    if (evs && evs.length > 0) {
      return evs.map((e) => `${EVENT_GLYPH[e.kind] ?? ">"} ${e.text}`).join("\n\n");
    }
    const a = props.agent;
    return a?.last ? "> " + a.last : "";
  };
  const shownLog = () => fullLog().slice(0, revealed());

  const refreshEvents = async (id: string) => {
    const st = await fetchAgentStatus(id);
    // Selection may have moved while the fetch was in flight — don't cross-wire.
    if (st?.events && props.agent?.id === id) setEvents(st.events);
  };

  onMount(() => {
    const clock = setInterval(() => setNowMs(Date.now()), 1000);
    // Keep the console fresh while an agent is selected (cheap local call).
    const poll = setInterval(() => {
      const a = props.agent;
      if (a && a.live !== false) refreshEvents(a.id);
    }, 2500);
    // Type-out reveal. The step scales with the backlog so a burst of output
    // (or selecting an agent with a long history) catches up in ~1s instead of
    // crawling, while small updates still get the character-by-character feel.
    const typer = setInterval(() => {
      const len = fullLog().length;
      const behind = len - revealed();
      if (behind > 0) setRevealed((r) => Math.min(len, r + Math.max(3, Math.ceil(behind / 12))));
    }, 66);
    onCleanup(() => {
      clearInterval(clock);
      clearInterval(poll);
      clearInterval(typer);
    });
  });

  createEffect(() => {
    const id = props.agent?.id ?? null;
    if (id !== lastSelId) {
      lastSelId = id;
      setEvents(null);
      setRevealed(streaming() ? 0 : fullLog().length);
      if (id) refreshEvents(id);
    }
  });

  // Keep the console pinned to the newest output.
  createEffect(() => {
    shownLog();
    if (consoleEl) consoleEl.scrollTop = consoleEl.scrollHeight;
  });

  async function doStop(a: AgentItem) {
    if (busy()) return;
    setBusy("stop");
    props.onError("");
    try {
      await stopAgent(a.id);
    } catch (e) {
      props.onError(String(e));
    } finally {
      setBusy("");
    }
  }

  async function doInstruct(a: AgentItem) {
    const text = instruction().trim();
    if (!text || busy()) return;
    setBusy("instruct");
    props.onError("");
    try {
      await instructAgent(a.id, text);
      setInstruction("");
    } catch (e) {
      props.onError(String(e));
    } finally {
      setBusy("");
    }
  }

  return (
    <div class="agents-detail">
      <Show
        when={props.agent}
        fallback={
          <div class="detail-empty">
            <div class="detail-empty-ring">
              <span />
            </div>
            <div>
              SELECT AN AGENT
              <br />
              TO INSPECT ITS STREAM
            </div>
          </div>
        }
      >
        {(a) => {
          const m = () => agentMeta(a());
          return (
            <div class="detail-body" style={{ "--accent": m().c, "--soft": m().soft }}>
              <div class="detail-head">
                <div class="detail-head-row">
                  <span class="detail-id">
                    <span class="agent-node-dot big" classList={{ pulse: m().pulse, blink: m().blink }} />
                    <span class="detail-name">{a().name}</span>
                  </span>
                  <span class="detail-badge">{m().detached ? "DETACHED" : m().label}</span>
                </div>
                <div class="detail-readouts">
                  <span>
                    RUNTIME <b>{fmtRuntime(a(), nowMs())}</b>
                  </span>
                  <span>
                    COST <b>{fmtCost(a())}</b>
                  </span>
                  <span>
                    TURNS <b>{a().turns ?? 0}</b>
                  </span>
                  <Show when={a().qa_active}>
                    <span class="detail-qa">QA LOOP</span>
                  </Show>
                </div>
              </div>

              <div class="detail-task">
                <div class="detail-label">LATEST</div>
                <div class="detail-task-text">{a().last || "—"}</div>
              </div>

              <div class="detail-output">
                <div class="detail-label">
                  <span class="agent-node-dot" classList={{ pulse: m().pulse }} /> LIVE OUTPUT
                </div>
                <div class="detail-console" ref={consoleEl}>
                  {shownLog() || "> no output yet"}
                  <Show when={streaming()}>
                    <span class="detail-cursor" />
                  </Show>
                </div>
              </div>

              <div class="detail-controls">
                <Show
                  when={a().live !== false}
                  fallback={<div class="detail-detached">detached session — history only</div>}
                >
                  <input
                    class="detail-instruct"
                    placeholder="instruct this agent… (Enter)"
                    value={instruction()}
                    disabled={busy() !== ""}
                    onInput={(e) => setInstruction(e.currentTarget.value)}
                    onKeyDown={(e) => e.key === "Enter" && doInstruct(a())}
                  />
                  <button class="detail-stop" disabled={busy() !== ""} onClick={() => doStop(a())}>
                    {busy() === "stop" ? "…" : "■ STOP"}
                  </button>
                </Show>
              </div>
            </div>
          );
        }}
      </Show>
    </div>
  );
}
