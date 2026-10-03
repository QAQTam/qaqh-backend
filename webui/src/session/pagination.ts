/**
 * 长上下文分页纯函数(spec §14):窗口淘汰、前插、滚动补偿计算。
 * 无 DOM、无 Solid 依赖,可单测。
 */
import { recomputeDerived } from "./reducer";
import type { GapSpan, SessionState, Slot } from "./types";

/** 内存回合窗口(spec §14.1,集中定义便于调整)。 */
export const TURN_WINDOW = 50;
/** 距视口上方超过 2 屏才允许淘汰。 */
export const EVICT_SCREENS = 2;

/**
 * 滚动补偿:前插一页后 scrollHeight 从 prevHeight 变为 newHeight,
 * 视口应整体下移差值,用户正在看的回合保持屏幕原位(spec §14.2)。
 */
export function anchorScrollTop(prevScrollTop: number, prevHeight: number, newHeight: number): number {
  return prevScrollTop + Math.max(0, newHeight - prevHeight);
}

/** 是否触发向上分页:scrollTop 低于 1.5×视口高(spec §14.2)。 */
export function shouldLoadOlder(scrollTop: number, viewportHeight: number, hasMore: boolean, loading: boolean): boolean {
  return hasMore && !loading && scrollTop < viewportHeight * 1.5;
}

/**
 * 窗口淘汰(spec §14.1):内存回合数超过 `window` 时,把「视口上方超过
 * `screens` 屏」的最旧回合替换为等高占位并释放其数据;运行中回合永不淘汰。
 *
 * 必须删掉 turns 条目:只换占位的话翻页越深攒下的全量文本越多(每个回合带着
 * 工具输出与思考全文),常驻内存只增不减。
 *
 * 淘汰掉的一段**并成一个 gap 槽**(见 `types.ts` 的 `Slot`):走笔是从前向后、
 * 遇到第一个不该淘汰的回合就停,所以被淘汰的永远是相邻一段;一格一个占位会让
 * `slots` 随翻页无界增长,每次前插都要重写整表下标。
 *
 * @param offsetTopOf 返回某回合槽当前距滚动内容顶部的偏移(px);
 * @param viewportTop 视口上沿的 scrollTop;
 * @param viewportHeight 视口高。
 * @returns 本次新被淘汰的回合 key(draft 已就地修改;占位高度由视图量完后回填)。
 */
export function evictForWindow(
  draft: SessionState,
  window: number,
  screens: number,
  offsetTopOf: (key: string) => number | null,
  viewportTop: number,
  viewportHeight: number,
): string[] {
  const evicted: string[] = [];
  let loaded = draft.slots.filter((slot) => slot.kind === "turn").length;
  if (loaded <= window) return evicted;
  const limit = viewportTop - screens * viewportHeight;
  let spans: GapSpan[] = [];
  let gapKey: string | null = null;
  let at = -1; // 合并后的 gap 落位下标
  let index = 0;
  while (index < draft.slots.length && loaded > window) {
    const slot = draft.slots[index]!;
    if (slot.kind === "gap") {
      // 跨过既有段:摘出来一起重排(继续淘汰的回合与它必然相邻),并保留它的 key,
      // 免得每次淘汰都把这个行的 DOM 换掉。
      if (at < 0) at = index;
      spans = spans.concat(slot.spans);
      gapKey ??= slot.key;
      draft.slots.splice(index, 1);
      continue;
    }
    const turn = draft.turns[slot.key];
    if (!turn || turn.status === "running") break;
    const top = offsetTopOf(slot.key);
    if (top == null || top > limit) break; // 还在视口附近 → 不淘汰(保视口内容优先)
    if (at < 0) at = index;
    evicted.push(slot.key);
    spans.push({ key: slot.key, height: 0 });
    draft.slots.splice(index, 1);
    delete draft.turns[slot.key];
    loaded -= 1;
  }
  if (spans.length > 0) {
    draft.slots.splice(Math.max(at, 0), 0, { kind: "gap", key: gapKey ?? `gap:${spans[0]!.key}`, spans } satisfies Slot);
    // 槽位表变了 → 派生答案跟着重算(被淘汰的失败回合不能再点亮标签的状态点)。
    recomputeDerived(draft);
  }
  return evicted;
}

/**
 * 把量到的高度回填进 gap 的对应小节(一次写入整段,视图只调一次)。
 * 只认 gap 里已有的 key:回填期间该回合若已被权威快照重新物化,就自然找不到小节。
 */
export function fillGapHeights(draft: SessionState, heights: ReadonlyMap<string, number>): void {
  for (const slot of draft.slots) {
    if (slot.kind !== "gap") continue;
    for (const span of slot.spans) {
      const height = heights.get(span.key);
      if (height != null && height !== span.height) span.height = height;
    }
  }
}

/** 占位高度上限:超过 2 屏的占位截到 2 屏,避免单个巨大占位把视口顶飞。 */
export function clampPlaceholderHeight(height: number, viewportHeight: number): number {
  return Math.min(Math.max(0, Math.round(height)), viewportHeight * 2);
}
