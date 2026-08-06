// 集成：resume 会话后 send 的完整前端数据链（daemon 重启场景）。
//
// 模拟真实事件流：
// 1. resumeSession：session.resume 命令（省略）→ ringingMonitor.activate
//    （bootstrap 快照：历史 turns + baseline_stream_seq）→ timeline.activate
//    （timeline 快照：watermark）
// 2. send：conversation 事件（turn_started + round_delta）经 handleBatch 应用；
//    timeline 事件（turn_opened + text_delta）经 handleEntry 应用
// 3. presentationFor（selectRingingPresentation + mergeTimelinePresentation）
//    断言新 turn 的流式内容最终出现在 transcript 投影里。

import { afterEach, describe, expect, it, vi } from "vitest";
import { createRingingMonitor } from "./ringingMonitor";
import { createTimelineMonitor } from "./timelineMonitor";
import { selectRingingPresentation } from "./sessionPresentation";
import { mergeTimelinePresentation } from "./timelinePresentation";
import { createRawSessionState } from "./rawSession";
import type { RingingEventBatch } from "../lib/types/ringing";
import type { TimelineSnapshotResponse } from "./timelineProtocol";

function batch(
  seed: string,
  channel: "control" | "conversation" | "tool",
  seq: number,
  event: Record<string, unknown>,
): RingingEventBatch {
  return {
    schema: "deepx.Ringing",
    version: 1,
    channel,
    seed,
    server_epoch: "epoch-2",
    from_stream_seq: seq,
    to_stream_seq: seq,
    envelopes: [{
      delivery: "reliable",
      seed,
      stream_seq: seq,
      channel_seq: seq,
      session_seq: seq,
      event_id: `${seed}-${channel}-${seq}`,
      state_revision: null,
      event: event as never,
    }],
  };
}

function turnStarted(turnId: string, userText: string) {
  return { channel: "conversation", type: "turn_started", turn_id: turnId, user_text: userText };
}
function roundDelta(turnId: string, roundNum: number, delta: string) {
  return { channel: "conversation", type: "round_delta", turn_id: turnId, round_num: roundNum, kind: "answering", delta };
}

interface BridgeListeners {
  onBatch?: (b: RingingEventBatch) => void;
}

function installBridge(bootstrap: unknown): BridgeListeners {
  const listeners: BridgeListeners = {};
  const bridge = {
    ringing: {
      status: async () => ({
        control: { state: "open" },
        conversation: { state: "open" },
        tool: { state: "open" },
      }),
      bootstrap: async () => bootstrap,
      onBatch: (fn: (b: RingingEventBatch) => void) => { listeners.onBatch = fn; return () => {}; },
      onStatus: () => () => {},
      onSnapshot: () => () => {},
    },
  };
  vi.stubGlobal("window", { deepx: bridge });
  return listeners;
}

function bootstrapPayload(seed: string) {
  return {
    schema: "deepx.Ringing",
    version: 1,
    seed,
    control: { state: { agent_lifecycle: "ready", session_state: "resumed" }, baseline_stream_seq: 5 },
    conversation: {
      state: {
        turns: [{
          turn_id: "t1",
          user_text: "old question",
          rounds: [{ round_num: 0, is_final: true, thinking: null, answer: "old answer" }],
        }],
        active_turn: null,
        total_turns: 1,
      },
      baseline_stream_seq: 5,
    },
    tool: { state: {}, baseline_stream_seq: 3 },
  };
}

function timelineSnapshot(seed: string): TimelineSnapshotResponse {
  return {
    schema: "deepx.Ringing",
    version: 1,
    server_epoch: "epoch-2",
    seed,
    snapshot: {
      watermark: 5,
      turns: [{
        turn_id: "t1",
        user_text: "old question",
        sealed: true,
        state: "completed",
        rounds: [{
          round_num: 0,
          sealed: true,
          is_final: true,
          blocks: [{ block_id: "b1", block_order: 0, kind: "text", state: "sealed", text: "old answer" }],
        }],
      }],
    },
  };
}

describe("resume 会话后 send 的完整前端数据链", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("daemon 重启后 resume → send → conversation+timeline 事件全部应用，transcript 显示新 turn 流式内容", async () => {
    const seed = "s-resume-e2e";
    const listeners = installBridge(bootstrapPayload(seed));
    const monitor = createRingingMonitor();
    const timelineMonitor = createTimelineMonitor();

    // ── resume ──
    await monitor.activate(seed);
    timelineMonitor.handleSnapshot(timelineSnapshot(seed));

    // 历史 turn 已恢复
    let stores = monitor.storesFor(seed)!;
    let fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), { includeTurns: true });
    let merged = mergeTimelinePresentation(seed, timelineMonitor.snapshotFor(seed)!, fallback, id => timelineMonitor.turnRevisionFor(seed, id));
    expect(merged.turns.find(t => t.turnId === "t1")?.rounds[0]?.answer).toContain("old answer");

    // ── send：conversation 事件（seq 6/7 > baseline 5）──
    listeners.onBatch?.(batch(seed, "conversation", 6, turnStarted("t2", "new question")));
    listeners.onBatch?.(batch(seed, "conversation", 7, roundDelta("t2", 0, "streaming answer")));
    // ── timeline 事件 ──
    timelineMonitor.handleEntry(seed, { timeline_seq: 6, turn_id: "t2", event: { type: "turn_opened", user_text: "new question" } });
    timelineMonitor.handleEntry(seed, { timeline_seq: 7, turn_id: "t2", round_num: 0, event: { type: "block_opened", block: { block_id: "b1", block_order: 0, kind: "text", state: "open" } } });
    timelineMonitor.handleEntry(seed, { timeline_seq: 8, turn_id: "t2", round_num: 0, event: { type: "text_delta", block_id: "b1", fragment_seq: 0, delta: "streaming answer" } });

    // ── 投影 ──
    stores = monitor.storesFor(seed)!;
    fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), { includeTurns: true });
    merged = mergeTimelinePresentation(seed, timelineMonitor.snapshotFor(seed)!, fallback, id => timelineMonitor.turnRevisionFor(seed, id));

    const t2 = merged.turns.find(t => t.turnId === "t2");
    expect(t2, "新 turn 必须出现在投影中").toBeDefined();
    expect(t2?.userText).toBe("new question");
    // 流式内容可见（conversation store 或 timeline 任一源）
    const answer = t2?.rounds.flatMap(r => r.answer ?? "").join("");
    expect(answer).toContain("streaming answer");
    // 历史 turn 保留
    expect(merged.turns.find(t => t.turnId === "t1")).toBeDefined();
  });

  it("resume 后未发送时，历史 turn 正常显示且无失败标记", async () => {
    const seed = "s-resume-idle";
    installBridge(bootstrapPayload(seed));
    const monitor = createRingingMonitor();
    const timelineMonitor = createTimelineMonitor();

    await monitor.activate(seed);
    timelineMonitor.handleSnapshot(timelineSnapshot(seed));

    const stores = monitor.storesFor(seed)!;
    const fallback = selectRingingPresentation(seed, stores, createRawSessionState(seed), { includeTurns: true });
    const merged = mergeTimelinePresentation(seed, timelineMonitor.snapshotFor(seed)!, fallback, id => timelineMonitor.turnRevisionFor(seed, id));

    expect(merged.turns.length).toBe(1);
    expect(merged.turns[0]?.userText).toBe("old question");
    expect(merged.turns[0]?.failure).toBeUndefined();
  });
});
