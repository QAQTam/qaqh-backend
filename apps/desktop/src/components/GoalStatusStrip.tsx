import { For, Show, createEffect, createSignal } from "solid-js";
import { request } from "../runtime/backendClient";
import { useI18n } from "../i18n";

type TodoItem = { id: string; title: string; description: string; status: string; complexity?: string; effort_min?: number };
type TodoStatus = { mode: string; current_id?: string; current_title?: string; completed: number; total: number; items: TodoItem[]; auto_turns: number };

export default function TodoStatusStrip(props: { seed: string; refreshKey: string | number }) {
  const { t } = useI18n();
  const [todo, setTodo] = createSignal<TodoStatus | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [expanded, setExpanded] = createSignal(false);
  let refreshGeneration = 0;
  async function refresh() {
    const generation = ++refreshGeneration;
    try {
      const next = await request<TodoStatus | null>("todo.status", { seed: props.seed });
      if (generation !== refreshGeneration) return;
      // Show for active / paused goals and todo lists; hide terminal states
      const visibleModes = new Set(["manual", "goal", "paused", "todo"]);
      setTodo(next && next.total > 0 && visibleModes.has(next.mode) ? next : null);
    }
    catch { if (generation === refreshGeneration) setTodo(null); }
  }
  async function action(act: "activate" | "resume" | "stop") {
    if (busy()) return;
    setBusy(true);
    try { await request("todo.action", { seed: props.seed, action: act }); }
    finally { setBusy(false); await refresh(); }
  }
  // Re-fetch when seed or refreshKey changes
  createEffect(
    () => [props.seed, props.refreshKey],
    () => { void refresh(); },
  );

  const statusIcon = (s: string) => s === "completed" ? "✓" : s === "in_progress" ? "●" : s === "cancelled" ? "✗" : "○";
  const complexityLabel = (c?: string) => c ? `[${c}]` : "";

  const modeLabel = (m: string) =>
    m === "goal" ? "Goal 模式" : m === "paused" ? "Goal 已暂停" : "Todo 列表";

  const modeDetail = (ti: TodoStatus) => {
    if (ti.mode === "goal") return `${ti.completed}/${ti.total} 完成 · ${ti.auto_turns} auto turns`;
    if (ti.mode === "paused") return `${ti.completed}/${ti.total} 完成 · 已暂停`;
    return `${ti.completed}/${ti.total} 完成`;
  };

  const isGoalish = (m: string) => m === "goal" || m === "paused";

  return <Show when={todo()}>
    {item => {
      const tItem = item()!;
      return <section class={`todo-status-strip ${tItem.mode}`} aria-label="Todo">
        <div class="todo-status-copy" onClick={() => setExpanded(!expanded())} style="cursor:pointer">
          <span class="todo-status-label">
            <i class="todo-status-dot" />
            {modeLabel(tItem.mode)}
          </span>
          <strong>
            {tItem.current_id ? `${tItem.current_id}: ${tItem.current_title}` : `${tItem.completed}/${tItem.total} items`}
          </strong>
          <small>{modeDetail(tItem)}</small>
        </div>
        <div class="todo-status-actions">
          <Show when={tItem.mode === "goal"}>
            <button disabled={busy()} onClick={() => void action("stop")} class="danger">Stop</button>
          </Show>
          <Show when={tItem.mode === "paused"}>
            <button disabled={busy()} onClick={() => void action("resume")}>Resume</button>
          </Show>
          <Show when={!isGoalish(tItem.mode)}>
            <button disabled={busy()} onClick={() => void action("activate")}>Activate</button>
          </Show>
        </div>
        <Show when={expanded()}>
          <ul class="todo-list-panel">
            <For each={tItem.items}>
              {todoItem => (
                <li class={`todo-list-item status-${todoItem.status}`}>
                  <span class="todo-item-icon">{statusIcon(todoItem.status)}</span>
                  <span class="todo-item-id-badge">{todoItem.id}</span>
                  <span class="todo-item-label">{complexityLabel(todoItem.complexity)} {todoItem.title}</span>
                  <Show when={todoItem.description}>
                    <span class="todo-item-desc"> — {todoItem.description}</span>
                  </Show>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>;
    }}
  </Show>;
}
