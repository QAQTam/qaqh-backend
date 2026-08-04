// 复现：resume 会话后 send，后端正常输出，前端 transcript 空白。
//
// 假设：resume 时拉取的 timeline 快照包含一个"同名占位 turn"（daemon 重启
// 时 orphan-sealer 把中断 turn 收尾为 Cancelled，或切换会话时残留的 running
// turn），而 worker 按消息存储计数复用了同一个 turn_id 处理新输入。
// mergeTimelinePresentation 中 timelineIds 已含该 turn_id → conversation
// store 的同名真实内容被 missing 过滤 → 新 turn 的流式内容被占位吞掉。

import { describe, expect, it } from "vitest";
import { createStore, flush } from "solid-js";
import {
  applyConversationEventToStore,
  initialRingingStores,
} from "./ringingStores";
import { selectRingingPresentation } from "./sessionPresentation";
import { mergeTimelinePresentation } from "./timelinePresentation";
import { createRawSessionState } from "./rawSession";
import type { TimelineSnapshot } from "./timelineProtocol";

/** conversation store：新输入产生的完整流式 turn。 */
function conversationWithStreamingTurn(seed: string) {
  const [stores, setStores] = createStore(initialRingingStores(seed));
  applyConversationEventToStore(setStores, {
    type: "turn_started",
    turn_id: "t12",
    user_text: "hi",
  });
  applyConversationEventToStore(setStores, {
    type: "round_delta",
    turn_id: "t12",
    round_num: 0,
    kind: "answering",
    delta: "Hello from backend",
  });
  applyConversationEventToStore(setStores, {
    type: "round_delta",
    turn_id: "t12",
    round_num: 0,
    kind: "answering",
    delta: " — still streaming",
  });
  // Solid 2 store 写是微任务批：投影前必须 flush 同步生效。
  flush();
  return stores;
}

describe("resume 会话后 send 的流式内容（timeline + conversation 双源 merge）", () => {
  it("对照组：timeline 快照不含新 turn 时，conversation store 内容可见", () => {
    const seed = "s-normal";
    const stores = conversationWithStreamingTurn(seed);
    const fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), {
      includeTurns: true,
    });
    // timeline 快照只有历史 turn（t1..t11），没有 t12
    const snapshot: TimelineSnapshot = {
      watermark: 11,
      turns: [
        {
          turn_id: "t11",
          user_text: "previous",
          sealed: true,
          state: "completed",
          rounds: [],
        },
      ],
    };
    const merged = mergeTimelinePresentation(seed, snapshot, fallback);
    const turn = merged.turns.find((t) => t.turnId === "t12");
    expect(turn).toBeDefined();
    expect(turn?.userText).toBe("hi");
    expect(turn?.rounds[0]?.answer).toContain("Hello from backend");
  });

  it("BUG 复现：timeline 快照含 cancelled 占位 t12（daemon 重启孤儿收尾）时，新 turn 内容被吞", () => {
    const seed = "s-cancelled-placeholder";
    const stores = conversationWithStreamingTurn(seed);
    const fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), {
      includeTurns: true,
    });
    // daemon 重启后 orphan-sealer 把未完成 t12 收尾为 Cancelled；worker 按
    // 消息存储计数复用了 t12（6a092fd 修复后这是合法场景）。但 timeline
    // 的 turn_opened 重放事件若未到达（gap/拒绝），快照占位一直保留。
    const snapshot: TimelineSnapshot = {
      watermark: 12,
      turns: [
        {
          turn_id: "t12",
          user_text: "previous interrupted",
          sealed: true,
          state: "cancelled",
          rounds: [],
        },
      ],
    };
    const merged = mergeTimelinePresentation(seed, snapshot, fallback);
    const turn = merged.turns.find((t) => t.turnId === "t12");
    // 期望：新输入的内容可见（后端已正常工作）
    expect(turn?.rounds[0]?.answer ?? "").toContain("Hello from backend");
  });

  it("BUG 复现：timeline 快照含 running 空占位 t12（切换会话残留）时，新 turn 内容被吞", () => {
    const seed = "s-running-placeholder";
    const stores = conversationWithStreamingTurn(seed);
    const fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), {
      includeTurns: true,
    });
    // 切换会话（SessionResume 触发 prepare_session_switch）时旧 worker 的
    // running turn 未 seal；切回后 get_or_spawn 直接返回（实例存活），
    // seal_orphan 不执行 → timeline 残留 running 空 turn。
    const snapshot: TimelineSnapshot = {
      watermark: 12,
      turns: [
        {
          turn_id: "t12",
          user_text: "previous interrupted",
          sealed: false,
          state: "running",
          rounds: [],
        },
      ],
    };
    const merged = mergeTimelinePresentation(seed, snapshot, fallback);
    const turn = merged.turns.find((t) => t.turnId === "t12");
    expect(turn?.rounds[0]?.answer ?? "").toContain("Hello from backend");
  });

  it("BUG 复现：resume 后 send，timeline 残留 daemon 重启中断标记（有部分内容）不再显示失败提示", () => {
    const seed = "s-restart-interrupted";
    const stores = conversationWithStreamingTurn(seed);
    const fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), {
      includeTurns: true,
    });
    // daemon 重启：orphan-sealer 把上次中断的 turn 收尾为 Cancelled 并写入
    // daemon_restart_interrupted failure；该 turn 有部分已 seal 内容（不是
    // 空占位）。worker 按消息存储计数复用了同一 turn_id（t12）处理新输入，
    // conversation store 已有新 running turn；timeline 事件到达前旧标记
    // 会遮蔽新内容并显示错误提示。
    const snapshot: TimelineSnapshot = {
      watermark: 12,
      turns: [
        {
          turn_id: "t12",
          user_text: "previous interrupted question",
          sealed: true,
          state: "cancelled",
          failure: {
            code: "daemon_restart_interrupted",
            message: "Daemon restarted while this turn was running; the turn was interrupted and the session is ready for new input.",
          },
          rounds: [
            {
              round_num: 0,
              sealed: true,
              is_final: false,
              blocks: [
                { block_id: "b1", block_order: 0, kind: "text", state: "sealed", text: "partial old answer" },
              ],
            },
          ],
        },
      ],
    };
    const merged = mergeTimelinePresentation(seed, snapshot, fallback);
    const turn = merged.turns.find((t) => t.turnId === "t12");
    // 新 turn 内容可见（未被旧内容遮蔽）
    expect(turn?.userText).toBe("hi");
    expect(turn?.rounds[0]?.answer ?? "").toContain("Hello from backend");
    // 旧 failure 标记不再显示（没有 daemon 重启错误提示）
    expect(turn?.failure).toBeUndefined();
    expect(turn?.status).not.toBe("cancelled");
  });

  it("用户主动取消的 turn（无 daemon 重启标记）保持 timeline 展示", () => {
    const seed = "s-user-cancelled";
    const [stores, setStores] = createStore(initialRingingStores(seed));
    // store 同名 turn 也是 cancelled（用户手动取消，非重启）
    applyConversationEventToStore(setStores, {
      type: "turn_started",
      turn_id: "t9",
      user_text: "do it",
    });
    applyConversationEventToStore(setStores, {
      type: "conversation_cancelled",
      turn_id: "t9",
    });
    flush();
    const fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), {
      includeTurns: true,
    });
    const snapshot: TimelineSnapshot = {
      watermark: 9,
      turns: [
        {
          turn_id: "t9",
          user_text: "do it",
          sealed: true,
          state: "cancelled",
          rounds: [],
        },
      ],
    };
    const merged = mergeTimelinePresentation(seed, snapshot, fallback);
    const turn = merged.turns.find((t) => t.turnId === "t9");
    // store 版本也是 cancelled → 保留 timeline 展示（无失败标记也不替换）
    expect(turn?.status).toBe("cancelled");
  });
});
