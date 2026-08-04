// @vitest-environment jsdom
//
// 渲染层集成测试：冷启动 resume 场景下，事件到达后**不切换 session**，
// transcript 是否响应式更新（用户报告：resume 后必须切换 session 才刷新，
// 切换只刷新"那一瞬"——典型的响应式断链症状）。

import { createSignal, For } from "solid-js";
import { render } from "@solidjs/web";
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
    }],
  };
}

function turnStarted(turnId: string, userText: string) {
  return { channel: "conversation", type: "turn_started", turn_id: turnId, user_text: userText };
}
function roundDelta(turnId: string, delta: string) {
  return { channel: "conversation", type: "round_delta", turn_id: turnId, round_num: 0, kind: "answering", delta };
}

interface BridgeListeners { onBatch?: (b: RingingEventBatch) => void }

function installBridge(bootstrap: unknown): BridgeListeners {
  const listeners: BridgeListeners = {};
  const bridge = {
    ringing: {
      status: async () => ({ control: { state: "open" }, conversation: { state: "open" }, tool: { state: "open" } }),
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

describe("渲染层响应式更新（不切换 session）", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("resume 后 send：事件到达后 transcript DOM 自动更新（无需切换 session）", async () => {
    const seed = "s-reactive";
    const listeners = installBridge(bootstrapPayload(seed));
    const monitor = createRingingMonitor();
    const timelineMonitor = createTimelineMonitor();

    // ── resume（冷启动时序）──
    await monitor.activate(seed);
    timelineMonitor.handleSnapshot(timelineSnapshot(seed));

    // ── 渲染 transcript（等价 presentationFor，组件内追踪）──
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => {
      // 模拟 App.tsx: `const rawSession = () => presentationFor(entry);`
      const raw = () => {
        timelineMonitor.version();
        const stores = monitor.storesFor(seed);
        const fallback = stores
          ? selectRingingPresentation(seed, stores, createRawSessionState(seed), { includeTurns: true })
          : createRawSessionState(seed);
        const snapshot = timelineMonitor.snapshotFor(seed);
        return snapshot
          ? mergeTimelinePresentation(seed, snapshot, fallback, id => timelineMonitor.turnRevisionFor(seed, id))
          : fallback;
      };
      return (
        <div data-testid="transcript">
          <For each={raw().turns}>
            {(turn) => (
              <div data-turn={turn.turnId}>
                <span data-part="user">{turn.userText}</span>
                <span data-part="answer">{turn.rounds.map(r => r.answer ?? "").join("")}</span>
              </div>
            )}
          </For>
        </div>
      );
    }, host);

    // 历史 turn 已渲染
    expect(host.textContent).toContain("old answer");

    // ── send：事件到达（不切换 session）──
    monitor.handleBatch(batch(seed, "conversation", 6, turnStarted("t2", "new question")));
    monitor.handleBatch(batch(seed, "conversation", 7, roundDelta("t2", "streaming answer")));
    timelineMonitor.handleEntry(seed, { timeline_seq: 6, turn_id: "t2", event: { type: "turn_opened", user_text: "new question" } });
    timelineMonitor.handleEntry(seed, { timeline_seq: 7, turn_id: "t2", round_num: 0, event: { type: "block_opened", block: { block_id: "b1", block_order: 0, kind: "text", state: "open" } } });
    timelineMonitor.handleEntry(seed, { timeline_seq: 8, turn_id: "t2", round_num: 0, event: { type: "text_delta", block_id: "b1", fragment_seq: 0, delta: "streaming answer" } });

    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    // 不切换 session：DOM 必须自动出现新 turn 与流式内容
    const t2 = host.querySelector('[data-turn="t2"]');
    expect(t2, "新 turn 必须自动渲染（响应式更新）").not.toBeNull();
    expect(t2?.querySelector('[data-part="answer"]')?.textContent).toContain("streaming answer");
    dispose();
    host.remove();
  });

  it("resume 后 timeline 事件继续到达：同一 turn 的流式增量自动更新 DOM", async () => {
    const seed = "s-reactive-2";
    const listeners = installBridge(bootstrapPayload(seed));
    const monitor = createRingingMonitor();
    const timelineMonitor = createTimelineMonitor();

    await monitor.activate(seed);
    timelineMonitor.handleSnapshot(timelineSnapshot(seed));

    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => {
      const raw = () => {
        timelineMonitor.version();
        const stores = monitor.storesFor(seed);
        const fallback = stores
          ? selectRingingPresentation(seed, stores, createRawSessionState(seed), { includeTurns: true })
          : createRawSessionState(seed);
        const snapshot = timelineMonitor.snapshotFor(seed);
        return snapshot
          ? mergeTimelinePresentation(seed, snapshot, fallback, id => timelineMonitor.turnRevisionFor(seed, id))
          : fallback;
      };
      return <div data-testid="t"><For each={raw().turns}>{(t) => <div data-turn={t.turnId}>{t.rounds.map(r => r.answer ?? "").join("")}</div>}</For></div>;
    }, host);

    monitor.handleBatch(batch(seed, "conversation", 6, turnStarted("t2", "q2")));
    monitor.handleBatch(batch(seed, "conversation", 7, roundDelta("t2", "hello ")));
    await Promise.resolve();
    await Promise.resolve();
    expect(host.textContent).toContain("hello");

    // 后续增量（round_delta 继续）→ 自动更新
    monitor.handleBatch(batch(seed, "conversation", 8, roundDelta("t2", "world")));
    await Promise.resolve();
    await Promise.resolve();
    expect(host.textContent).toContain("hello world");
    dispose();
    host.remove();
  });
});
