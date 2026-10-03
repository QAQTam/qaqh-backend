/**
 * 待办面板(spec 外新增,对齐 TUI draw_todo 的数据面):
 * 停靠消息区右侧(grid 轨道宽度动画推让消息列),顶部常驻计数入口;
 * 有待办自动展开、清空自动收起,手动开合优先于自动(清空后重置为自动)。
 *
 * 状态视觉:空心圆=待办;spinner=进行中;绿点+划线=完成;红点=取消。
 * 逐条错峰入场动画;数据单源 store.todos(todo.list RPC,
 * dashboard_updated 控制事件/激活/投影重置驱动刷新)。
 */
import { createEffect, createSignal, For, Show, type Component } from "solid-js";
import IconChevronDown from "~icons/lucide/chevron-down";
import IconChevronRight from "~icons/lucide/chevron-right";
import IconListTodo from "~icons/lucide/list-todo";
import IconLoader from "~icons/lucide/loader-circle";
import { STR } from "../lib/strings";
import type { SessionStore } from "../session/store";

type TodoStatus = "pending" | "in_progress" | "completed" | "cancelled";

function normalizeStatus(raw: unknown): TodoStatus {
  switch (raw) {
    case "in_progress":
    case "in-progress":
    case "running":
      return "in_progress";
    case "completed":
    case "done":
      return "completed";
    case "cancelled":
    case "canceled":
      return "cancelled";
    default:
      return "pending";
  }
}

const TodoRow: Component<{ item: { title?: string; status?: string; evidence?: string }; index: number }> = (props) => {
  const status = () => normalizeStatus(props.item.status);
  return (
    <li
      class={{ "todo-row": true, [`st-${status()}`]: true }}
      style={{ "animation-delay": `${Math.min(props.index, 12) * 40}ms` }}
    >
      <Show
        when={status() === "in_progress"}
        fallback={<span class="todo-dot" aria-hidden="true" />}
      >
        <span class="todo-dot todo-spin" aria-hidden="true"><IconLoader /></span>
      </Show>
      <span class="todo-title" title={props.item.evidence ?? props.item.title}>
        {props.item.title}
      </span>
    </li>
  );
};

export const TodoPanel: Component<{ store: SessionStore; onOpenChange: (open: boolean) => void }> = (props) => {
  const items = () => props.store.todos[0]();
  // null = 自动(有待办展开/无待办收起);true/false = 用户手动开合,清空后重置。
  const [manual, setManual] = createSignal<boolean | null>(null);
  createEffect(
    () => items().length,
    (total) => {
      // 清空后重置为自动模式(下次有待办会再自动展开)。
      if (total === 0) setManual(null);
    },
  );
  const open = () => manual() ?? items().length > 0;
  // 注意:Solid 2 的 signal setter 会返回被赋的值,effect 回调绝不能是
  // 表达式箭头体——返回的布尔值会被当成 cleanup 引发 REACTIVITY_HALTED。
  createEffect(
    open,
    (isOpen) => {
      props.onOpenChange(isOpen);
    },
  );

  const doneCount = () => items().filter((item) => normalizeStatus(item.status) === "completed").length;

  return (
    <aside class="todo-dock" aria-label={STR.todoTitle}>
      <div class="todo-panel">
        <button
          type="button"
          class="todo-head"
          aria-expanded={open() ? "true" : "false"}
          onClick={() => setManual(!open())}
        >
          <IconListTodo />
          <span>{STR.todoTitle}</span>
          <Show when={items().length > 0}>
            <b class="todo-count">{STR.todoDone(doneCount(), items().length)}</b>
          </Show>
          <span class="todo-chevron">
            <Show when={open()} fallback={<IconChevronRight />}><IconChevronDown /></Show>
          </span>
        </button>
        <Show when={open()}>
          <Show
            when={items().length > 0}
            fallback={<div class="todo-empty">{STR.todoEmpty}</div>}
          >
            <ul class="todo-list">
              <For each={items()}>
                {(item, index) => <TodoRow item={item} index={index()} />}
              </For>
            </ul>
          </Show>
        </Show>
      </div>
    </aside>
  );
};
