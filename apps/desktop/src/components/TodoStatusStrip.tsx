import { For, Show, createMemo, createSignal, createEffect } from "solid-js";
import type { TaskInfo } from "../lib/types";

export type TodoItemStatus = "pending" | "in_progress" | "completed" | "cancelled";

/** Truncate to ~25 CJK or ~50 ASCII visual width. */
function truncTitle(t: string): string {
  let w = 0;
  const max = 50;
  for (let i = 0; i < t.length; i++) {
    const cw = /[\u4e00-\u9fff\u3000-\u303f\uff00-\uffef]/.test(t[i]) ? 2 : 1;
    if (w + cw > max) return t.slice(0, i) + "…";
    w += cw;
  }
  return t;
}

/** Pure display: receives tasks directly from WebSocket Dashboard events. */
export default function TodoStatusStrip(props: {
  tasks: TaskInfo[];
  currentTodoId?: string | null;
}) {
  const [expanded, setExpanded] = createSignal(false);
  const [completedFlash, setCompletedFlash] = createSignal<Set<string>>(new Set());

  const statusLabel = (status: string) =>
    ({ pending: "待处理", in_progress: "进行中", completed: "已完成", cancelled: "已取消" } as Record<string, string>)[status] ?? status;
  const statusIcon = (status: string) =>
    ({ pending: "○", in_progress: "◌", completed: "✓", cancelled: "—" } as Record<string, string>)[status] ?? "○";
  const count = (status: string) =>
    props.tasks.filter(t => t.status === status).length;

  const activeItem = createMemo(() =>
    props.tasks.find(t => t.id === props.currentTodoId)
    ?? props.tasks.find(t => t.status === "in_progress")
    ?? null,
  );

  const carousel = createMemo(() => {
    const active = activeItem();
    if (!active) return { prev: null as TaskInfo | null, current: null as TaskInfo | null, next: null as TaskInfo | null };
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

  const doneCount = () => count("completed") + count("cancelled");
  const totalCount = () => props.tasks.length;

  const summaryLine = () => {
    const total = props.tasks.length;
    const done = count("completed") + count("cancelled");
    const pending = count("pending");
    const inProg = count("in_progress");
    if (pending === 0 && inProg === 0) return `✓ 全部完成 (${done}/${total})`;
    const parts: string[] = [];
    if (inProg > 0) parts.push(`${inProg} 进行中`);
    if (pending > 0) parts.push(`${pending} 待处理`);
    return `${parts.join(" · ")} · ${done}/${total}`;
  };

  // ── Completion flash tracking ──
  createEffect(
    () => props.tasks,
    (tasks) => {
      for (const t of tasks) {
        if (t.status === "completed" || t.status === "cancelled") {
          if (!completedFlash().has(t.id)) {
            const next = new Set(completedFlash());
            next.add(t.id);
            setCompletedFlash(next);
            setTimeout(() => {
              setCompletedFlash(prev => {
                const s = new Set(prev);
                s.delete(t.id);
                return s;
              });
            }, 800);
          }
        }
      }
    },
  );

  // ── Single row render helper (reused in roulette) ──
  const renderRow = (item: TaskInfo | null, role: "prev" | "current" | "next", showProgress: boolean) => {
    if (!item) {
      return <div class={`todo-roulette-row row-${role} is-empty`}><span class="todo-roulette-dot">·</span></div>;
    }
    const isFlashing = completedFlash().has(item.id);
    return (
      <div
        class={`todo-roulette-row row-${role} status-${item.status}${isFlashing ? " flash-complete" : ""}`}
        data-todo-id={item.id}
      >
        <span class="todo-roulette-dot">
          <i class={`todo-ci s-${item.status}${item.status === "in_progress" ? " pulse" : ""}`}>
            {statusIcon(item.status)}
          </i>
        </span>
        <span class="todo-roulette-id">{item.id}</span>
        <span class="todo-roulette-text">{truncTitle(item.subject)}</span>
        <span class={`todo-roulette-badge s-${item.status}`}>{statusLabel(item.status)}</span>
        {showProgress && (
          <span class="todo-roulette-progress">
            <span class="todo-progress-track"><span class="todo-progress-fill" style={{ width: `${donePct()}%` }} /></span>
            <small>{doneCount()}/{totalCount()}</small>
          </span>
        )}
      </div>
    );
  };

  // ── Collapsed content ──
  const collapsedContent = createMemo(() => {
    const car = carousel();
    const cur = car.current;

    // No active item → summary line
    if (!cur) {
      return (
        <div class="todo-row todo-summary">
          <span>{summaryLine()}</span>
          <span class="todo-progress">
            <span class="todo-progress-track"><span class="todo-progress-fill" style={{ width: `${donePct()}%` }} /></span>
            <small>{doneCount()}/{totalCount()}</small>
          </span>
        </div>
      );
    }

    // Active item → 3-row roulette
    return (
      <div class="todo-roulette">
        {renderRow(car.prev, "prev", false)}
        {renderRow(car.current, "current", true)}
        {renderRow(car.next, "next", false)}
      </div>
    );
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
        {collapsedContent()}
      </Show>
    </section>
  </Show>;
}
