/** 事件 reducer(spec §1.3 允许的纯逻辑单测:事件 reducer 与 seq 去重)。 */
import { describe, expect, test } from "bun:test";
import { applyEntry, applySnapshot, emptySession, prependPage } from "../src/session/reducer";
import type { SessionState, TimelineEntryWire } from "../src/session/types";

function entry(seq: number, turnId: string, event: Record<string, any>, round = 0): TimelineEntryWire {
  return { timeline_seq: seq, turn_id: turnId, round_num: round, event };
}

describe("applyEntry:回合生命周期", () => {
  test("turn_opened/block_opened/text_delta 推进 Turn 并提升作答", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "turn_opened", user_text: "你好" }));
    applyEntry(state, entry(2, "t1", { type: "block_opened", block: { block_id: "b1", kind: "reasoning", state: "open" } }));
    applyEntry(state, entry(3, "t1", { type: "text_delta", block_id: "b1", delta: "思考中" }));
    applyEntry(state, entry(4, "t1", { type: "block_sealed", block_id: "b1" }));
    applyEntry(state, entry(5, "t1", { type: "block_opened", block: { block_id: "b2", kind: "text", state: "open" } }));
    applyEntry(state, entry(6, "t1", { type: "text_delta", block_id: "b2", delta: "答案" }));

    const turn = state.turns["t1"]!;
    expect(turn.user.text).toBe("你好");
    expect(turn.steps).toHaveLength(2);
    expect(turn.steps[0]).toMatchObject({ kind: "thinking", text: "思考中" });
    expect(turn.answer?.text).toBe("答案");
    expect(turn.answerStepId).toBe("b2");
    expect(state.activeReasoningId).toBeNull(); // block_sealed 后思考链关闭
  });

  test("作答候选后新开工具 → 作答降级为中间叙述,不再提升", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "turn_opened", user_text: "hi" }));
    applyEntry(state, entry(2, "t1", { type: "block_opened", block: { block_id: "b1", kind: "text" } }));
    applyEntry(state, entry(3, "t1", { type: "text_delta", block_id: "b1", delta: "先看看" }));
    applyEntry(state, entry(4, "t1", { type: "block_opened", block: { block_id: "b2", kind: "tool", tool: { name: "read", state: "running" } } }));
    applyEntry(state, entry(5, "t1", { type: "text_delta", block_id: "b1", delta: "更多" }));

    const turn = state.turns["t1"]!;
    expect(turn.answer).toBeNull();
    expect(turn.steps.find((step) => step.id === "b1")).toMatchObject({ kind: "text", text: "先看看更多" });
  });

  test("tool_updated 终态写 output/display;error 带错误文本", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "block_opened", block: { block_id: "b1", kind: "tool", tool: { name: "exec", state: "running" } } }));
    applyEntry(state, entry(2, "t1", {
      type: "tool_updated",
      block_id: "b1",
      tool: {
        name: "exec", state: "failed",
        display: { body: { kind: "streams", stdout: "out", stderr: "boom", exit_code: 2 } },
        failure: { code: "x", message: "炸了" },
      },
    }));
    const step = state.turns["t1"]!.steps[0]!;
    if (step.kind !== "tool") throw new Error("expect tool step");
    expect(step.status).toBe("error");
    expect(step.output).toMatchObject({ text: "out", stderr: "boom", exitCode: 2 });
    expect(step.error).toBe("炸了");
    expect(step.endedAt).toBeDefined();
  });

  test("turn_sealed 映射状态;快照重建等价", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "turn_opened", user_text: "q" }));
    applyEntry(state, entry(2, "t1", { type: "turn_sealed", state: "cancelled" }));
    expect(state.turns["t1"]!.status).toBe("aborted");

    const snap = emptySession();
    applySnapshot(snap, {
      server_epoch: "e1",
      snapshot: {
        watermark: 7,
        turns: [{ turn_id: "t9", turn_index: 3, user_text: "hi", state: "completed", rounds: [{ round_num: 0, blocks: [{ block_id: "b1", kind: "text", state: "sealed", text: "答" }] }] }],
      },
      has_more: true,
      total_turns: 10,
      truncated_before: false,
    });
    expect(snap.watermark).toBe(7);
    expect(snap.serverEpoch).toBe("e1");
    expect(snap.oldestIndex).toBe(3);
    expect(snap.hasMore).toBe(true);
    expect(snap.turns["#3"]!.answer?.text).toBe("答");
  });
});

describe("seq 去重与翻页去重", () => {
  test("prependPage 按 turn_index 去重(turn_id 会复用,不当键)", () => {
    const state: SessionState = emptySession();
    applySnapshot(state, {
      snapshot: { watermark: 5, turns: [{ turn_id: "t2", turn_index: 1, user_text: "b", state: "completed", rounds: [] }] },
    });
    const page = {
      snapshot: {
        turns: [
          { turn_id: "t2", turn_index: 1, user_text: "b", state: "completed", rounds: [] }, // 重复,跳过
          { turn_id: "t1", turn_index: 0, user_text: "a", state: "completed", rounds: [] },
        ],
      },
      has_more: false,
    };
    prependPage(state, page);
    expect(state.slots).toHaveLength(2);
    expect(state.slots[0]).toEqual({ kind: "turn", key: "#0" });
    expect(state.oldestIndex).toBe(0);
    expect(state.hasMore).toBe(false);
  });
});
