/**
 * Todo 状态条（输入框上方，3 行滑窗）——产品定稿设计。
 *
 * 收起：上=刚完成（划线）/ 中=工作中 / 下=下一个待办。
 * 点击行 → 向上弹出全列表（浮层，不推挤转录）；行尾箭头 → 只展开该条描述。
 * 滑窗指针随 agent 翻状态移动，DOM O(1)。
 */
import { createSignal, For, Show, type Component } from "solid-js";
import { todo, type TodoItemView } from "../state";

const ICON: Record<string, string> = { pending: "○", in_progress: "◐", completed: "●", cancelled: "⊘" };

const Row: Component<{
  item: TodoItemView;
  current: boolean;
  strike: boolean;
  /** 行体点击（缺省 = 无动作）。收起态传「弹出全列表」。 */
  onBodyClick?: () => void;
}> = (props) => {
  const [open, setOpen] = createSignal(false);
  return (
    <>
      <div
        class={{ "tr-row": true, current: props.current, strike: props.strike }}
        onClick={() => props.onBodyClick?.()}
      >
        <span class="tr-icon">{ICON[props.item.status] ?? "○"}</span>
        <span class="tr-title">{props.item.title}</span>
        <button
          class="tr-arrow"
          title="描述"
          onClick={(e) => {
            e.stopPropagation();
            setOpen(!open());
          }}
        >
          {open() ? "▾" : "▸"}
        </button>
      </div>
      <Show when={open()}>
        <div class="tr-desc">
          <Show when={props.item.description}>
            <div>{props.item.description}</div>
          </Show>
          <Show when={props.item.evidence}>
            <div class="tr-evidence">证据：{props.item.evidence}</div>
          </Show>
        </div>
      </Show>
    </>
  );
};

export const TodoTicker: Component = () => {
  const [expanded, setExpanded] = createSignal(false);
  const expand = () => setExpanded(true);

  return (
    <Show when={todo()}>
      {(value) => {
        const list = value().items;
        if (list.length === 0) return null;
        const currentId = value().currentId;
        let index = currentId ? list.findIndex((i) => i.id === currentId) : -1;
        if (index < 0) index = list.findIndex((i) => i.status === "in_progress");
        if (index < 0) index = list.findIndex((i) => i.status === "pending");
        if (index < 0) index = list.length - 1;
        const above = index > 0 ? list[index - 1]! : null;
        const mid = list[index]!;
        const below = index + 1 < list.length ? list[index + 1]! : null;

        return (
          <div class="todo-ticker">
            {/* 向上弹出：全列表（浮层，不推挤转录） */}
            <Show when={expanded()}>
              <div class="todo-expanded">
                <div class="te-head">
                  <span>
                    {value().mode} · {value().counts.completed}/{value().counts.total} 完成
                  </span>
                  <button onClick={() => setExpanded(false)}>收起 ▾</button>
                </div>
                <div class="te-progress">
                  <i
                    style={`width:${value().counts.total ? (value().counts.completed / value().counts.total) * 100 : 0}%`}
                  />
                </div>
                <For each={list}>
                  {(item) => (
                    <div class={{ "te-item": true, current: currentId === item.id }}>
                      <Row item={item} current={currentId === item.id} strike={item.status === "completed"} />
                    </div>
                  )}
                </For>
              </div>
            </Show>

            {/* 3 行滑窗 */}
            <Show when={above}>
              {(item) => <Row item={item()} current={false} strike onBodyClick={expand} />}
            </Show>
            <Row item={mid} current strike={false} onBodyClick={expand} />
            <Show when={below}>
              {(item) => <Row item={item()} current={false} strike={false} onBodyClick={expand} />}
            </Show>
          </div>
        );
      }}
    </Show>
  );
};
