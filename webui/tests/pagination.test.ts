/** 分页前插滚动补偿与窗口淘汰(spec §1.3 允许的纯逻辑单测)。 */
import { describe, expect, test } from "bun:test";
import { anchorScrollTop, clampPlaceholderHeight, evictForWindow, shouldLoadOlder } from "../src/session/pagination";
import { applySnapshot, emptySession } from "../src/session/reducer";

function seededTurns(count: number): ReturnType<typeof applySnapshot> extends never ? never : void {
  // helper 在下方使用
  void count;
}
void seededTurns;

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
  test("超出窗口且远在视口上方的最旧回合被替换为占位,运行中永不淘汰", () => {
    const state = stateWith(4);
    // offsetTop:key→0,1,2,3 每 1000px;视口顶 3500,视口高 500 → 淘汰线 2500
    const offsetTopOf = (key: string): number => Number(key.replace("#", "")) * 1000;
    const evicted = evictForWindow(state, 2, 2, offsetTopOf, 3_500, 500);
    expect(evicted).toEqual(["#0", "#1"]);
    expect(state.slots[0]).toMatchObject({ kind: "placeholder", key: "#0" });
    expect(state.slots[1]).toMatchObject({ kind: "placeholder", key: "#1" });
    expect(state.slots[2]).toMatchObject({ kind: "turn" });
    expect(state.turns["#0"]).toBeDefined(); // 数据仍在(仅渲染为占位)
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
