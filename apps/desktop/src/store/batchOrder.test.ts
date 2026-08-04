// 回归：同一 batch 内多个事件 / 快速连续的 handleBatch 不得互相覆盖。
//
// 根因：dispatchToStores 在 setStores 函数式更新外基于旧 state 计算
// next，同一轮排队的多个 setter 后一个覆盖前一个（事件丢失）。resume
// 后 send 的 turn_started + round_delta 常被 main 合并进同一 batch，
// 导致 transcript 空白——必须切换 session 强制重渲染才显示残缺内容。

import { afterEach, describe, expect, it, vi } from "vitest";
import { createRingingMonitor } from "./ringingMonitor";
import type { RingingEventBatch } from "../lib/types/ringing";

function batch(
  seed: string,
  channel: "control" | "conversation" | "tool",
  events: Array<{ seq: number; event: Record<string, unknown> }>,
): RingingEventBatch {
  const from = events[0]!.seq;
  const to = events[events.length - 1]!.seq;
  return {
    schema: "deepx.Ringing",
    version: 1,
    channel,
    seed,
    server_epoch: "epoch-2",
    from_stream_seq: from,
    to_stream_seq: to,
    envelopes: events.map(({ seq, event }) => ({
      schema: "deepx.Ringing",
      version: 1,
      channel,
      delivery: "reliable",
      server_epoch: "epoch-2",
      seed,
      stream_seq: seq,
      channel_seq: seq,
      session_seq: seq,
      event_id: `${seed}-${channel}-${seq}`,
      state_revision: null,
      event: event as never,
    })),
  };
}

function turnStarted(turnId: string, userText: string) {
  return { channel: "conversation", type: "turn_started", turn_id: turnId, user_text: userText };
}
function roundDelta(turnId: string, roundNum: number, delta: string) {
  return { channel: "conversation", type: "round_delta", turn_id: turnId, round_num: roundNum, kind: "answering", delta };
}
function turnCompleted(turnId: string) {
  return { channel: "conversation", type: "turn_completed", turn_id: turnId };
}

function stubBridge() {
  vi.stubGlobal("window", { deepx: { ringing: {
    onBatch: () => () => {}, onStatus: () => () => {}, onSnapshot: () => () => {},
  } } });
}

afterEach(() => vi.unstubAllGlobals());

describe("同一 batch 多事件 / 快速连续事件不覆盖", () => {
  it("同一 batch 内 turn_started + round_delta + round_delta 全部应用", async () => {
    stubBridge();
    const monitor = createRingingMonitor();
    const seed = "s-multi";
    monitor.handleBatch(batch(seed, "conversation", [
      { seq: 1, event: turnStarted("t1", "hi") },
      { seq: 2, event: roundDelta("t1", 0, "hello ") },
      { seq: 3, event: roundDelta("t1", 0, "world") },
    ]));
    await Promise.resolve();
    await Promise.resolve(); // Solid store flush
    const conversation = monitor.storesFor(seed)!.conversation;
    expect(conversation.turns.length).toBe(1);
    const turn = conversation.turns[0]!;
    expect(turn.userText).toBe("hi");
    // 两次 round_delta 都应用（追加语义），无覆盖
    expect(turn.rounds[0]?.answer).toBe("hello world");
  });

  it("快速连续 handleBatch（无微任务间隔）不覆盖", async () => {
    stubBridge();
    const monitor = createRingingMonitor();
    const seed = "s-rapid";
    monitor.handleBatch(batch(seed, "conversation", [{ seq: 1, event: turnStarted("t1", "q") }]));
    monitor.handleBatch(batch(seed, "conversation", [{ seq: 2, event: roundDelta("t1", 0, "a") }]));
    monitor.handleBatch(batch(seed, "conversation", [{ seq: 3, event: roundDelta("t1", 0, "b") }]));
    monitor.handleBatch(batch(seed, "conversation", [{ seq: 4, event: turnCompleted("t1") }]));
    await Promise.resolve();
    await Promise.resolve(); // Solid store flush
    const conversation = monitor.storesFor(seed)!.conversation;
    expect(conversation.turns.length).toBe(1);
    expect(conversation.turns[0]?.rounds[0]?.answer).toBe("ab");
  });

  it("多事件 batch 后渲染层能看到完整内容（resume 场景回归）", async () => {
    stubBridge();
    const monitor = createRingingMonitor();
    const seed = "s-render";
    // 模拟 resume 后 send：main 把 turn_started + 首批 round_delta 合并进一个 batch
    monitor.handleBatch(batch(seed, "conversation", [
      { seq: 11, event: turnStarted("t12", "继续") },
      { seq: 12, event: roundDelta("t12", 0, "流式输出") },
    ]));
    await Promise.resolve();
    await Promise.resolve();
    const conversation = monitor.storesFor(seed)!.conversation;
    expect(conversation.turns.length).toBe(1);
    expect(conversation.turns[0]?.rounds[0]?.answer).toBe("流式输出");
  });
});
