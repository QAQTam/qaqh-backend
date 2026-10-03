/**
 * 单行思考链(spec §10):输入区正上方,仅进行中的思考出现,固定 28px。
 * 超宽始终显示最右端:flex-end + overflow hidden + 左侧 mask,
 * MUST NOT 用 JS 修改 scrollLeft 追尾;换行不闪空(保持上一行直到新内容)。
 *
 * 性能纪律(对齐 TUI `tail_cols`):
 *  - 当前行在 JS 里截尾(行长无上界时,巨长 nowrap 文本的每帧重排会突破
 *    Chromium 布局精度,引发卡顿乃至合成损坏/画面错位);
 *  - 重绘跟着帧走:上游 store 已把 delta 合成成每帧一次写入,这里再设一道
 *    120ms 限频只会把思考行的节奏钉在 ~8fps,与作答不一致;
 *  - 消失(回合结束/思考块封闭)立即清除,避免残影;空行保持上一次显示,不闪空。
 *  - 数据面只读 `activeReasoningTurnKey` 指向的那一个回合:遍历 turns 会让
 *    这条链订阅每一个回合的每一个属性(流式期每帧都要重算一遍全表)。
 */
import { createEffect, createSignal, Show, type Component } from "solid-js";
import IconBrain from "~icons/lucide/brain";
import type { SessionStore } from "../session/store";

/** 显示尾窗上限(字符):输入条 ~760px,200 字符远超可视宽度,CJK 也够。 */
const LINE_TAIL_CHARS = 200;
/** 取行文本时的截尾倍数:换行定位需要一点前文,但不必带上整段思考。 */
const LINE_SCAN_CHARS = LINE_TAIL_CHARS * 4;

export const ThinkingChain: Component<{ store: SessionStore }> = (props) => {
  const [shown, setShown] = createSignal("");

  createEffect(
    () => {
      const state = props.store.state[0];
      const id = state.activeReasoningId;
      const turnKey = state.activeReasoningTurnKey;
      if (id == null || turnKey == null) return null;
      const turn = state.turns[turnKey];
      if (turn == null) return null;
      for (let i = turn.steps.length - 1; i >= 0; i -= 1) {
        const step = turn.steps[i]!;
        if (step.kind !== "thinking" || step.id !== id) continue;
        const text = step.text;
        const tail = text.length > LINE_SCAN_CHARS ? text.slice(-LINE_SCAN_CHARS) : text;
        return { id, line: tail.slice(tail.lastIndexOf("\n") + 1) };
      }
      return null;
    },
    (current) => {
      // 消失立即清;空行(刚收到换行)保持上一次显示,不闪空(§10.1.2)。
      if (current == null) {
        setShown("");
        return;
      }
      if (current.line === "") return;
      setShown(current.line.slice(-LINE_TAIL_CHARS));
    },
  );

  return (
    <Show when={props.store.state[0].activeReasoningId != null}>
      <div class="thinking-chain" role="status">
        <span class="thinking-icon"><IconBrain /></span>
        <span class="thinking-line"><span class="thinking-line-text">{shown()}</span></span>
      </div>
    </Show>
  );
};
