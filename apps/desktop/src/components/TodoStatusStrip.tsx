import { For, Show, createMemo, createSignal } from "solid-js";

export type TodoItemStatus = "pending" | "in_progress" | "completed" | "cancelled";
export type TodoTask = {
  id: string;
  subject: string;
  description: string;
  status: TodoItemStatus;
};

/** Truncate to ~15 CJK or ~25 ASCII visual width. */
function truncTitle(t: string): string {
  let w = 0;
  const max = 30;
  for (let i = 0; i < t.length; i++) {
    const cw = /[\u4e00-\u9fff\u3000-\u303f\uff00-\uffef]/.test(t[i]) ? 2 : 1;
    if (w + cw > max) return t.slice(0, i) + "…";
    w += cw;
  }
  return t;
}

/** Pure display: receives tasks directly from WebSocket Dashboard events. */
export default function TodoStatusStrip(props: {
  tasks: TodoTask[];
  currentTodoId?: string | null;
}) {
  const [expanded, setExpanded] = createSignal(false);

  const statusLabel = (status: TodoItemStatus) => ({
    pending: "待处理", in_progress: "进行中", completed: "已完成", cancelled: "已取消",
  })[status];
  const statusIcon = (status: TodoItemStatus) => ({
    pending: "○", in_progress: "◌", completed: "✓", cancelled: "—",
  })[status];
  const count = (status: TodoItemStatus) =>
    props.tasks.filter(t => t.status === status).length;

  const activeItem = createMemo(() =>
    props.tasks.find(t => t.id === props.currentTodoId)
    ?? props.tasks.find(t => t.status === "in_progress")
    ?? null,
  );

  const carousel = createMemo(() => {
    const active = activeItem();
    if (!active) return { prev: null as TodoTask | null, current: null as TodoTask | null, next: null as TodoTask | null };
    const idx = props.tasks.findIndex(t => t.id === active.id);
    return {
      prev: idx > 0 ? props.tasks[idx - 1] : null,
      current: active,
      next: idx < props.tasks.length - 1 ? props.tasks[idx + 1] : null,
    };
  });

  const donePct = () => {
    const total = props.tasks.length;
    if (total === 0) return 0;
    return Math.round((count("completed") + count("cancelled")) * 100 / total);
  };

  const summaryLine = () => {
    const total = props.tasks.length;
    const done = count("completed") + count("cancelled");
    const pending = count("pending");
    const inProg = count("in_progress");
    if (pending === 0 && inProg === 0) return `✓ 全部完成 (${done}/${total})`;
    const parts: string[] = [];
    if (inProg > 0) parts.push(`${inProg} 进行中`);
    if (pending > 0) parts.push(`${pending} 待处理`);
    return `${parts.join(" · ")} · 进度 ${done}/${total}`;
  };

  // Pre-compute collapsed row content so it stays reactive inside Show
  const collapsedRow = createMemo(() => {
    const car = carousel();
    const cur = car.current;
    if (!cur) {
      return <div class="todo-row todo-summary">
        <span>{summaryLine()}</span>
        <span class="todo-progress">
          <span class="todo-progress-track"><span class="todo-progress-fill" style={{ width: `${donePct()}%` }} /></span>
          <small>{donePct()}%</small>
        </span>
      </div>;
    }
    return <div class="todo-row">
      {car.prev ? <span class="todo-arr" aria-hidden="true">‹</span> : <span class="todo-arr is-empty" />}
      <span class="todo-cur">
        <i class={`todo-ci s-${cur.status} pulse`}>{statusIcon(cur.status)}</i>
        <span class="todo-ci-id">{cur.id}</span>
        <span class="todo-ci-text">{truncTitle(cur.subject)}</span>
        <span class={`todo-ci-badge s-${cur.status}`}>{statusLabel(cur.status)}</span>
        <span class="todo-progress">
          <span class="todo-progress-track"><span class="todo-progress-fill" style={{ width: `${donePct()}%` }} /></span>
          <small>{donePct()}%</small>
        </span>
      </span>
      {car.next ? <span class="todo-arr" aria-hidden="true">›</span> : <span class="todo-arr is-empty" />}
    </div>;
  });

  return <Show when={props.tasks.length > 0} fallback={null}>
    <section
      class={`todo-strip${count("in_progress") > 0 ? " has-active" : " all-done"}`}
      aria-label="任务进度"
      aria-expanded={expanded() ? "true" : "false"}
      onClick={() => setExpanded(!expanded())}
    >
      <Show when={!expanded()} fallback={
        <ul class="todo-list-panel">
          <For each={props.tasks}>
            {ti => (
              <li class={`todo-list-item status-${ti.status}`} data-status={ti.status}>
                <span class="todo-item-icon">{statusIcon(ti.status)}</span>
                <span class="todo-item-id-badge">{ti.id}</span>
                <span class="todo-item-title">{ti.subject}</span>
                <span class={`todo-item-pill pill-${ti.status}`}>{statusLabel(ti.status)}</span>
              </li>
            )}
          </For>
        </ul>
      }>
        {/* Render memoized row — SolidJS tracks its dependencies */}
        {collapsedRow()}
      </Show>
    </section>
  </Show>;
}
