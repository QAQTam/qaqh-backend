import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { request } from "../runtime/backendClient";

export type TodoItemStatus = "pending" | "in_progress" | "completed" | "cancelled";
type TodoItem = {
  id: string;
  title: string;
  description: string;
  status: TodoItemStatus;
};
type TodoStatus = {
  mode: "manual";
  current_id?: string | null;
  current_title?: string | null;
  pending?: number;
  in_progress?: number;
  completed: number;
  cancelled?: number;
  total: number;
  items: TodoItem[];
  goal_enabled?: boolean;
};

/** Compact, read-only view of the backend-owned todo status contract. */
export default function TodoStatusStrip(props: { seed: string; refreshKey: string | number }) {
  const [todo, setTodo] = createSignal<TodoStatus | null>(null);
  const [expanded, setExpanded] = createSignal(false);
  let refreshGeneration = 0;

  async function refresh() {
    const generation = ++refreshGeneration;
    try {
      const next = await request<TodoStatus | null>("todo.status", { seed: props.seed });
      if (generation !== refreshGeneration) return;
      setTodo(next && next.total > 0 ? next : null);
    }
    catch { if (generation === refreshGeneration) setTodo(null); }
  }

  // Re-fetch when seed or refreshKey changes
  createEffect(
    () => [props.seed, props.refreshKey],
    () => { void refresh(); },
  );

  const statusLabel = (status: TodoItemStatus) => ({
    pending: "待处理",
    in_progress: "进行中",
    completed: "已完成",
    cancelled: "已取消",
  })[status];
  const statusIcon = (status: TodoItemStatus) => ({
    pending: "○",
    in_progress: "●",
    completed: "✓",
    cancelled: "—",
  })[status];
  const count = (status: TodoItemStatus) => {
    const value = todo();
    if (!value) return 0;
    const contractValue = value[status];
    return typeof contractValue === "number"
      ? contractValue
      : value.items.filter(item => item.status === status).length;
  };
  const activeItem = createMemo(() => {
    const value = todo();
    if (!value) return null;
    return value.items.find(item => item.id === value.current_id)
      ?? value.items.find(item => item.status === "in_progress")
      ?? null;
  });
  const headline = () => {
    const active = activeItem();
    if (active) return `${active.id}: ${active.title}`;
    if (count("pending") === 0) return "所有 Todo 均已处理";
    return `${todo()?.total ?? 0} 项 Todo`;
  };
  const summary = () => [
    count("in_progress") > 0 ? `进行中 ${count("in_progress")}` : "",
    count("pending") > 0 ? `待处理 ${count("pending")}` : "",
    `已完成 ${count("completed")}`,
    count("cancelled") > 0 ? `已取消 ${count("cancelled")}` : "",
  ].filter(Boolean).join(" · ");

  return <Show when={todo()}>
    {item => {
      const tItem = item()!;
      return <section class={`todo-status-strip${count("in_progress") > 0 ? " has-active" : ""}`} aria-label="Todo 状态">
        <button
          type="button"
          class="todo-status-copy"
          aria-expanded={expanded() ? "true" : "false"}
          onClick={() => setExpanded(!expanded())}
        >
          <span class="todo-status-label">
            <i class="todo-status-dot" />
            Todo 列表
          </span>
          <strong>{headline()}</strong>
          <small>{summary()}</small>
          <span class="todo-status-chevron" aria-hidden="true">{expanded() ? "▴" : "▾"}</span>
        </button>
        <Show when={expanded()}>
          <ul class="todo-list-panel">
            <For each={tItem.items}>
              {todoItem => (
                <li class={`todo-list-item status-${todoItem.status}`} data-status={todoItem.status}>
                  <span class="todo-item-icon">{statusIcon(todoItem.status)}</span>
                  <span class="todo-item-id-badge">{todoItem.id}</span>
                  <span class="todo-item-body">
                    <span class="todo-item-label">{todoItem.title}</span>
                    <Show when={todoItem.description}>
                      <span class="todo-item-desc">{todoItem.description}</span>
                    </Show>
                  </span>
                  <span class={`todo-item-status status-${todoItem.status}`}>
                    {statusLabel(todoItem.status)}
                  </span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>;
    }}
  </Show>;
}
