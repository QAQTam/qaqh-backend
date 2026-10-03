/** 分页前插滚动补偿与窗口淘汰(spec §1.3 允许的纯逻辑单测)。 */
import { describe, expect, test } from "bun:test";
import { anchorScrollTop, clampPlaceholderHeight, evictForWindow, fillGapHeights, shouldLoadOlder } from "../src/session/pagination";
import { applySnapshot, emptySession, recomputeDerived } from "../src/session/reducer";
import { gapHeight, type SessionState } from "../src/session/types";

const spansOf = (state: SessionState): string[] => {
  const gap = state.slots.find((slot) => slot.kind === "gap");
  return gap != null && gap.kind === "gap" ? gap.spans.map((span) => span.key) : [];
};
const turnKeys = (state: SessionState): string[] => state.slots.filter((slot) => slot.kind === "turn").map((slot) => slot.key);
const gapCount = (state: SessionState): number => state.slots.filter((slot) => slot.kind === "gap").length;

function stateWith(count: number, runningLast = false) {
  const state = emptySession();
  const turns = Array.from({ length: count }, (_, index) => ({
    turn_id: `t${index}`,
    turn_index: index,
    user_text: `u${index}`,
    state: runningLast && index === count - 1 ? "running" : "completed",
    rounds: [],
  }));
  applySnapshot(state, { snapshot: { watermark: count, turns } });
  return state;
}

describe("anchorScrollTop / shouldLoadOlder", () => {
  test("前插后视口按高度差下移", () => {
    expect(anchorScrollTop(100, 5_000, 6_200)).toBe(1_300);
    expect(anchorScrollTop(100, 6_200, 6_200)).toBe(100);
  });

  test("触顶阈值 = 1.5×视口高", () => {
    expect(shouldLoadOlder(700, 500, true, false)).toBe(true);
    expect(shouldLoadOlder(760, 500, true, false)).toBe(false);
    expect(shouldLoadOlder(0, 500, false, false)).toBe(false);
    expect(shouldLoadOlder(0, 500, true, true)).toBe(false);
  });
});

describe("evictForWindow", () => {
  test("超出窗口且远在视口上方的最旧回合并成一个 gap 占位,运行中永不淘汰", () => {
    const state = stateWith(4);
    // offsetTop:key→0,1,2,3 每 1000px;视口顶 3500,视口高 500 → 淘汰线 2500
    const offsetTopOf = (key: string): number => Number(key.replace("#", "")) * 1000;
    const evicted = evictForWindow(state, 2, 2, offsetTopOf, 3_500, 500);
    expect(evicted).toEqual(["#0", "#1"]);
    // 一段占位 + 两个保留回合 = 3 槽(不是 4 槽:占位不再一格一个)
    expect(state.slots).toHaveLength(3);
    expect(state.slots[0]).toMatchObject({ kind: "gap", key: "gap:#0", spans: [{ key: "#0" }, { key: "#1" }] });
    expect(turnKeys(state)).toEqual(["#2", "#3"]);
    expect(state.turns["#0"]).toBeUndefined(); // 数据随淘汰释放(只留等高占位)
    expect(state.turns["#1"]).toBeUndefined();
  });

  test("再淘汰继续并入同一段:段数不随翻页增长,段 key 不变", () => {
    const state = stateWith(5);
    const offsetTopOf = (key: string): number => Number(key.replace("#", "")) * 1000;
    evictForWindow(state, 3, 2, offsetTopOf, 3_500, 500); // 淘汰 #0,#1
    const firstGapKey = state.slots[0]!.key;
    expect(spansOf(state)).toEqual(["#0", "#1"]);

    // 视口继续下移 → #2 也够远了;它应当并进既有段而不是新开一段。
    evictForWindow(state, 2, 2, offsetTopOf, 4_500, 500);
    expect(gapCount(state)).toBe(1);
    expect(state.slots[0]!.key).toBe(firstGapKey);
    expect(spansOf(state)).toEqual(["#0", "#1", "#2"]);
    expect(turnKeys(state)).toEqual(["#3", "#4"]);
  });

  test("fillGapHeights 逐小节回填高度,gapHeight 求和", () => {
    const state = stateWith(4);
    evictForWindow(state, 2, 2, () => 0, 9_999, 500);
    fillGapHeights(state, new Map([["#0", 900], ["#1", 1_200]]));
    const gap = state.slots[0]!;
    expect(gap.kind === "gap" && gapHeight(gap.spans)).toBe(2_100);
    // 没在段里的 key(已被权威快照重新物化)不会被误写
    fillGapHeights(state, new Map([["#9", 500]]));
    expect(gap.kind === "gap" && gapHeight(gap.spans)).toBe(2_100);
  });

  test("淘汰掉的失败回合不再点亮状态点:派生字段随槽位表一起重算", () => {
    const state = stateWith(4);
    state.turns["#0"]!.status = "failed";
    recomputeDerived(state);
    expect(state.failedTurns).toBe(1);

    const evicted = evictForWindow(state, 2, 2, () => 0, 9_999, 500);

    expect(evicted).toEqual(["#0", "#1"]);
    expect(state.failedTurns).toBe(0);
  });

  test("运行中回合挡住淘汰:遇到即停,不跳过它继续拿更旧的", () => {
    // #0 已完成,#1 运行中,#2 已完成 → 只能淘汰 #0,运行中挡住后面
    const state = stateWith(3);
    state.turns["#1"]!.status = "running";
    const offsetTopOf = () => 0;
    const evicted = evictForWindow(state, 0, 2, offsetTopOf, 9_999, 500);
    expect(evicted).toEqual(["#0"]);
    expect(state.slots[1]).toMatchObject({ kind: "turn", key: "#1" });
  });

  test("视口附近的回合不淘汰", () => {
    const state = stateWith(4);
    const offsetTopOf = () => 3_000; // 距视口顶(3500)不到 2 屏
    const evicted = evictForWindow(state, 2, 2, offsetTopOf, 3_500, 500);
    expect(evicted).toHaveLength(0);
  });

  test("占位高度钳到 2 屏", () => {
    expect(clampPlaceholderHeight(50, 500)).toBe(50);
    expect(clampPlaceholderHeight(9_999, 500)).toBe(1_000);
  });
});
