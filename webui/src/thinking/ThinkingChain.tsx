/**
 * 单行思考链(spec §10):输入区正上方,仅进行中的思考出现,固定 28px。
 * 超宽始终显示最右端:flex-end + overflow hidden + 左侧 mask,
 * MUST NOT 用 JS 修改 scrollLeft 追尾;换行不闪空(保持上一行直到新内容)。
 */
import { createEffect, createSignal, Show, type Component } from "solid-js";
import IconBrain from "~icons/lucide/brain";
import type { SessionStore } from "../session/store";

export const ThinkingChain: Component<{ store: SessionStore }> = (props) => {
  const [shown, setShown] = createSignal("");

  createEffect(
    () => {
      const id = props.store.state[0].activeReasoningId;
      if (id == null) return null;
      for (const turn of Object.values(props.store.state[0].turns)) {
        const step = turn.steps.find((s) => s.id === id);
        if (step?.kind === "thinking") {
          const text = step.text;
          return { id, line: text.slice(text.lastIndexOf("\n") + 1) };
        }
      }
      return null;
    },
    (current) => {
      // 空行(刚收到换行)保持上一次显示,不闪空(§10.1.2)。
      if (current == null) {
        setShown("");
        return;
      }
      if (current.line !== "") setShown(current.line);
    },
  );

  return (
    <Show when={props.store.state[0].activeReasoningId != null}>
      <div class="thinking-chain" role="status">
        <span class="thinking-icon"><IconBrain /></span>
        <span class="thinking-line">{shown()}</span>
      </div>
    </Show>
  );
};
