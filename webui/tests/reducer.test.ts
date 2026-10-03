/** 事件 reducer(spec §1.3 允许的纯逻辑单测:事件 reducer 与 seq 去重)。 */
import { describe, expect, test } from "bun:test";
import { applyEntry, applySnapshot, emptySession, prependPage, recomputeDerived } from "../src/session/reducer";
import type { SessionState, TimelineEntryWire } from "../src/session/types";

function entry(seq: number, turnId: string, event: Record<string, any>, round = 0): TimelineEntryWire {
  return { timeline_seq: seq, turn_id: turnId, round_num: round, event };
}

/** 快照/翻页页里的一条回合记录(`turn_index` 省略 = 实时路径那种无序号回合)。 */
function rawTurn(
  turnId: string,
  turnIndex: number | null,
  userText: string,
  state = "completed",
  blocks: Array<Record<string, unknown>> = [],
): Record<string, unknown> {
  return {
    turn_id: turnId,
    ...(turnIndex == null ? {} : { turn_index: turnIndex }),
    user_text: userText,
    state,
    rounds: [{ round_num: 0, blocks }],
  };
}

const slotKeys = (state: SessionState): string[] => state.slots.map((slot) => slot.key);

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

describe("失败槽与终态槽(后端契约切片 2026-10-03)", () => {
  /** 后端新语义:`failure.message` 在线上**恒存在**,无结构化 error 时是空串,
   *  此时按契约显示裸 code(`tool_failure_of` / `timeline_rebuild` 注释)。
   *  回落链用 `??` 会让空串吃掉 code → 错误正文整块消失。 */
  test("message 空串 → 回落 code(实时条目)", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "block_opened", block: { block_id: "b1", kind: "tool", tool: { name: "edit", state: "running" } } }));
    applyEntry(state, entry(2, "t1", {
      type: "tool_updated",
      block_id: "b1",
      tool: { name: "edit", state: "failed", output: "file changed since read\nHint: re-read", failure: { code: "stale_file", message: "" } },
    }));
    const step = state.turns["t1"]!.steps[0]!;
    if (step.kind !== "tool") throw new Error("expect tool step");
    expect(step.error).toBe("stale_file");
  });

  test("message 空串 → 回落 code(归档快照重建)", () => {
    const state = emptySession();
    applySnapshot(state, {
      snapshot: {
        watermark: 1,
        turns: [{
          turn_id: "t1", turn_index: 0, user_text: "q", state: "completed",
          rounds: [{ round_num: 0, blocks: [{
            block_id: "b1", kind: "tool", state: "sealed",
            tool: { name: "read", state: "failed", output: "boom", failure: { code: "tool_execution_failed", message: "" } },
          }] }],
        }],
      },
    });
    const step = state.turns["#0"]!.steps[0]!;
    if (step.kind !== "tool") throw new Error("expect tool step");
    expect(step.error).toBe("tool_execution_failed");
  });

  test("message 非空优先于 code;回合级两者皆空才用默认语", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "turn_opened", user_text: "q" }));
    applyEntry(state, entry(2, "t1", { type: "turn_sealed", state: "failed", failure: { code: "model_request_failed", message: "" } }));
    expect(state.turns["t1"]!.error!.message).toBe("model_request_failed");

    const blank = emptySession();
    applyEntry(blank, entry(1, "t2", { type: "turn_opened", user_text: "q" }));
    applyEntry(blank, entry(2, "t2", { type: "turn_sealed", state: "failed", failure: { code: "", message: "" } }));
    expect(blank.turns["t2"]!.error!.message).toBe("回合失败");
  });

  /** 顶层槽优先是**读取口径**的锁:后端目前从 body 提取顶层槽,二者同源。 */
  test("终态槽:completed_at_ms 写 endedAt,exit_code 顶层槽优先于 body", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "block_opened", block: { block_id: "b1", kind: "tool", tool: { name: "exec", state: "running" } } }));
    applyEntry(state, entry(2, "t1", {
      type: "tool_updated",
      block_id: "b1",
      tool: {
        name: "exec", state: "succeeded", completed_at_ms: 1_700_000_000_123, exit_code: 101,
        display: { body: { kind: "shell", output: "done", exit_code: 0, truncated: false } },
      },
    }));
    const step = state.turns["t1"]!.steps[0]!;
    if (step.kind !== "tool") throw new Error("expect tool step");
    expect(step.endedAt).toBe(1_700_000_000_123);
    expect(step.output).toMatchObject({ text: "done", exitCode: 101 });
  });

  test("终态槽缺席(归档 rebuild 不带 completed_at_ms)→ 本地采样兜底", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "block_opened", block: { block_id: "b1", kind: "tool", tool: { name: "exec", state: "running" } } }));
    applyEntry(state, entry(2, "t1", {
      type: "tool_updated",
      block_id: "b1",
      tool: { name: "exec", state: "succeeded", display: { body: { kind: "shell", output: "done", exit_code: 0, truncated: false } } },
    }));
    const step = state.turns["t1"]!.steps[0]!;
    if (step.kind !== "tool") throw new Error("expect tool step");
    expect(step.endedAt).toBeGreaterThan(1_600_000_000_000);
    expect(step.output).toMatchObject({ text: "done", exitCode: 0 });
  });
});

describe("派生计数(视图据此判定,不再遍历 turns)", () => {
  test("turn_opened/turn_sealed 维护 runningTurns / activeTurnKey / failedTurns", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "turn_opened", user_text: "a" }));
    expect(state.runningTurns).toBe(1);
    expect(state.activeTurnKey).toBe("t1");
    applyEntry(state, entry(2, "t1", { type: "turn_sealed", state: "completed" }));
    expect(state.runningTurns).toBe(0);
    expect(state.activeTurnKey).toBeNull();
    expect(state.failedTurns).toBe(0);

    applyEntry(state, entry(3, "t2", { type: "turn_opened", user_text: "b" }));
    applyEntry(state, entry(4, "t2", { type: "turn_sealed", state: "failed", failure: { message: "炸" } }));
    expect(state.runningTurns).toBe(0);
    expect(state.failedTurns).toBe(1);
  });

  test("block_opened/block_sealed 维护 activeReasoningTurnKey", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t1", { type: "block_opened", block: { block_id: "b1", kind: "reasoning" } }));
    expect(state.activeReasoningId).toBe("b1");
    expect(state.activeReasoningTurnKey).toBe("t1");
    applyEntry(state, entry(2, "t1", { type: "block_sealed", block_id: "b1" }));
    expect(state.activeReasoningId).toBeNull();
    expect(state.activeReasoningTurnKey).toBeNull();
  });

  test("快照重建按载入数据重算计数", () => {
    const state = emptySession();
    applySnapshot(state, {
      snapshot: {
        watermark: 2,
        turns: [
          { turn_id: "a", turn_index: 0, user_text: "a", state: "failed", rounds: [] },
          { turn_id: "b", turn_index: 1, user_text: "b", state: "running", rounds: [] },
        ],
      },
    });
    expect(state.runningTurns).toBe(1);
    expect(state.failedTurns).toBe(1);
    expect(state.activeTurnKey).toBe("#1");
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

  test("翻页页里与实时回合同 id 同序号:认成同一个,不挂第二行", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t9", { type: "turn_opened", user_text: "九" }));
    applyEntry(state, entry(2, "t9", { type: "turn_sealed", state: "completed" }));
    prependPage(state, { snapshot: { turns: [rawTurn("t9", 9, "九")] }, has_more: false });
    expect(slotKeys(state)).toEqual(["t9"]);
    expect(state.slots).toHaveLength(1);
  });
});

describe("快照回合并(重对齐不再整表重建)", () => {
  test("同一份权威数据再落地:回合对象与槽对象一个都没换", () => {
    const state = emptySession();
    const page = { snapshot: { watermark: 3, turns: [rawTurn("t1", 0, "一"), rawTurn("t2", 1, "二")] } };
    applySnapshot(state, page, 1000);
    const first = state.turns["#0"]!;
    const second = state.turns["#1"]!;
    const slots = [...state.slots];

    applySnapshot(state, { snapshot: { watermark: 9, turns: [rawTurn("t1", 0, "一"), rawTurn("t2", 1, "二")] } }, 2000);

    expect(state.turns["#0"]).toBe(first); // 内容未变 → 订阅者不重跑(TurnView/Markdown 不重挂)
    expect(state.turns["#1"]).toBe(second);
    expect(state.slots[0]).toBe(slots[0]);
    expect(state.slots[1]).toBe(slots[1]);
    expect(state.watermark).toBe(9); // 数据照实推进
  });

  test("内容变了才换对象,别的回合不受影响", () => {
    const state = emptySession();
    applySnapshot(state, { snapshot: { turns: [rawTurn("t1", 0, "一"), rawTurn("t2", 1, "二")] } }, 1000);
    const first = state.turns["#0"]!;
    const second = state.turns["#1"]!;

    applySnapshot(state, { snapshot: { turns: [rawTurn("t1", 0, "一(权威更正)"), rawTurn("t2", 1, "二")] } }, 2000);

    expect(state.turns["#0"]).not.toBe(first);
    expect(state.turns["#0"]!.user.text).toBe("一(权威更正)");
    expect(state.turns["#1"]).toBe(second);
  });

  test("实时回合与快照回合认成同一个:key 不变、对象不换、序号补记", () => {
    const state = emptySession();
    applyEntry(state, entry(1, "t50", { type: "turn_opened", user_text: "跑着呢" }));
    const live = state.turns["t50"]!;
    expect(slotKeys(state)).toEqual(["t50"]);

    // 权威页给这个回合带上了全局序号:以前它会换成 "#50" 这个新 key,正在流式的
    // 回合连 DOM 一起重挂(实测闪断 + 丢滚动位置)。
    applySnapshot(state, { snapshot: { watermark: 2, turns: [rawTurn("t50", 50, "跑着呢", "running")] } }, 5000);

    expect(state.turns["t50"]).toBe(live);
    expect(state.turns["#50"]).toBeUndefined();
    expect(slotKeys(state)).toEqual(["t50"]);
    expect(live.turnIndex).toBe(50);
    expect(state.runningTurns).toBe(1);
    expect(state.activeTurnKey).toBe("t50");
  });

  test("重对齐保住展开态与本地采样到的时间戳(快照不带后端毫秒)", () => {
    const state = emptySession();
    const blocks = [{ block_id: "b1", kind: "reasoning", state: "sealed", text: "想" }];
    applySnapshot(state, { snapshot: { turns: [rawTurn("t1", 0, "问", "completed", blocks)] } }, 1000);
    const turn = state.turns["#0"]!;
    expect(turn.workStartedAt).toBe(1000);
    expect(turn.steps[0]).toMatchObject({ kind: "thinking", endedAt: 1000 });
    turn.expanded = true;

    applySnapshot(state, { snapshot: { turns: [rawTurn("t1", 0, "问", "completed", blocks)] } }, 9000);

    expect(state.turns["#0"]).toBe(turn);
    expect(turn.expanded).toBe(true);
    expect(turn.workStartedAt).toBe(1000);
    expect(turn.steps[0]!.endedAt).toBe(1000);
  });

  test("页面不再覆盖的已结束回合被释放;仍在运行的尾部回合留在原位", () => {
    const state = emptySession();
    applySnapshot(state, { snapshot: { watermark: 5, turns: [rawTurn("t8", 8, "八"), rawTurn("t9", 9, "九")] } }, 1000);
    expect(slotKeys(state)).toEqual(["#8", "#9"]);

    // 更旧的一页:窗口右移到 [#7,#8),#9 已结束 → 数据释放。
    applySnapshot(state, { snapshot: { watermark: 6, turns: [rawTurn("t7", 7, "七"), rawTurn("t8", 8, "八")] } }, 2000);
    expect(state.turns["#9"]).toBeUndefined();
    expect(slotKeys(state)).toEqual(["#7", "#8"]);

    // 尾部新开一个运行中回合,它比这份快照更晚,权威页里没有它。
    applyEntry(state, entry(7, "t10", { type: "turn_opened", user_text: "十" }));
    applySnapshot(state, { snapshot: { watermark: 7, turns: [rawTurn("t7", 7, "七"), rawTurn("t8", 8, "八")] } }, 3000);

    expect(state.turns["t10"]).toBeDefined();
    expect(slotKeys(state)).toEqual(["#7", "#8", "t10"]);
    expect(state.runningTurns).toBe(1);
    expect(state.failedTurns).toBe(0);
  });

  test("快照先落地、同一个运行中回合的实时条目后到达:不开第二行", () => {
    const state = emptySession();
    applySnapshot(state, { snapshot: { watermark: 9, turns: [rawTurn("t50", 50, "跑着呢", "running")] } }, 1000);
    const row = state.turns["#50"]!;

    applyEntry(state, entry(10, "t50", { type: "block_opened", block: { block_id: "b1", kind: "text" } }));
    applyEntry(state, entry(11, "t50", { type: "text_delta", block_id: "b1", delta: "再输出" }));

    expect(slotKeys(state)).toEqual(["#50"]); // 同一个回合:上面那行冻结在快照内容、下面再开一行才是重复
    expect(state.turns["t50"]).toBeUndefined();
    expect(state.turns["#50"]).toBe(row);
    expect(row.answer?.text).toBe("再输出");
    expect(state.activeTurnKey).toBe("#50");
  });

  test("派生字段只有一个计算点:数据变了重算即可", () => {
    const state = emptySession();
    applySnapshot(state, { snapshot: { turns: [rawTurn("t1", 0, "一"), rawTurn("t2", 1, "二", "failed")] } }, 1000);
    expect(state.failedTurns).toBe(1);

    // 直接改数据(不经过事件路径)后重算,答案照着实情走 —— 记账式维护漏一处就错。
    state.turns["#0"]!.status = "failed";
    recomputeDerived(state);
    expect(state.failedTurns).toBe(2);
    expect(state.runningTurns).toBe(0);
    expect(state.activeTurnKey).toBeNull();
  });
});

describe("陈旧快照页不得回滚（B）", () => {
  const pageWith = (epoch: string, watermark: number, text: string) => ({
    server_epoch: epoch,
    snapshot: {
      watermark,
      turns: [{
        turn_id: "t1", turn_index: 0, user_text: "q", state: "completed",
        rounds: [{ round_num: 0, blocks: [{ block_id: "b1", kind: "text", state: "sealed", text }] }],
      }],
    },
  });

  test("同 epoch、更低 watermark 的页整页丢弃(宿流的 cursor 只向前,回退就是永久丢文本)", () => {
    const state = emptySession();
    applySnapshot(state, pageWith("e1", 10, "新文本"));
    expect(state.watermark).toBe(10);
    applySnapshot(state, pageWith("e1", 5, "旧文本"));
    expect(state.watermark).toBe(10);
    expect(state.turns["#0"]!.answer?.text).toBe("新文本");
  });

  test("换 epoch 允许把基线重设到更小的 seq", () => {
    const state = emptySession();
    applySnapshot(state, pageWith("e1", 10, "新文本"));
    applySnapshot(state, pageWith("e2", 3, "重启后文本"));
    expect(state.serverEpoch).toBe("e2");
    expect(state.watermark).toBe(3);
    expect(state.turns["#0"]!.answer?.text).toBe("重启后文本");
  });

  test("同 watermark 的重复权威页仍然应用(幂等重对齐不误伤)", () => {
    const state = emptySession();
    applySnapshot(state, pageWith("e1", 10, "第一版"));
    applySnapshot(state, pageWith("e1", 10, "第二版"));
    expect(state.turns["#0"]!.answer?.text).toBe("第二版");
  });

  test("缺 snapshot 的页不再清空已有转录", () => {
    const state = emptySession();
    applySnapshot(state, pageWith("e1", 10, "内容"));
    applySnapshot(state, { server_epoch: "e1" });
    expect(state.slots).toHaveLength(1);
    expect(state.turns["#0"]!.answer?.text).toBe("内容");
  });
});
