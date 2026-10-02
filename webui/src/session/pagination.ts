/**
 * 长上下文分页纯函数(spec §14):窗口淘汰、前插、滚动补偿计算。
 * 无 DOM、无 Solid 依赖,可单测。
 */
import type { SessionState, Slot } from "./types";

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
 * `screens` 屏」的最旧回合替换为等高占位;运行中回合永不淘汰。
 *
 * @param offsetTopOf 返回某回合槽当前距滚动内容顶部的偏移(px);
 * @param viewportTop 视口上沿的 scrollTop;
 * @param viewportHeight 视口高。
 * @returns 被淘汰的 slot key 列表(draft 已就地修改)。
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
  for (let i = 0; i < draft.slots.length && loaded > window; i += 1) {
    const slot = draft.slots[i]!;
    if (slot.kind !== "turn") continue;
    const turn = draft.turns[slot.key];
    if (!turn || turn.status === "running") break;
    const top = offsetTopOf(slot.key);
    if (top == null || top > limit) break; // 还在视口附近 → 不淘汰(保视口内容优先)
    evicted.push(slot.key);
    draft.slots[i] = { kind: "placeholder", key: slot.key, height: 0 } satisfies Slot;
    loaded -= 1;
  }
  return evicted;
}

/** 占位高度上限:超过 2 屏的占位截到 2 屏,避免单个巨大占位把视口顶飞。 */
export function clampPlaceholderHeight(height: number, viewportHeight: number): number {
  return Math.min(Math.max(0, Math.round(height)), viewportHeight * 2);
}
