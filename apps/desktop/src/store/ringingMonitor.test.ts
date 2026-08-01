import { afterEach, describe, expect, it, vi } from "vitest";
import { createRingingMonitor } from "./ringingMonitor";
import type { RingingEventBatch } from "../lib/types/ringing";

function batch(
  seed: string,
  channel: "control" | "conversation" | "tool",
  seq: bigint,
  event: Record<string, unknown>,
): RingingEventBatch {
  return {
    channel,
    seed,
    from_stream_seq: seq,
    to_stream_seq: seq,
    state_revision: null,
    events: [event as never],
  };
}

function turnStarted(turnId: string, userText: string) {
  return {
    channel: "conversation",
    type: "turn_started",
    turn_id: turnId,
    user_text: userText,
  };
}

function roundDelta(turnId: string, delta: string) {
  return {
    channel: "conversation",
    type: "round_delta",
    turn_id: turnId,
    round_num: 0,
    kind: "answering",
    delta,
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("createRingingMonitor reactivity", () => {
  it("bumps ringingVersion on every applied batch and updates shadow stores", async () => {
    vi.stubGlobal("window", { deepx: undefined });
    const monitor = createRingingMonitor();
    const before = monitor.ringingVersion();

    monitor.handleBatch(batch("s1", "conversation", 1n, turnStarted("t1", "hi")));
    await Promise.resolve();
    const shadow = monitor.shadowOf("s1");
    expect(shadow?.conversation.turns).toHaveLength(1);
    expect(shadow?.conversation.turns[0].turnId).toBe("t1");
    expect(monitor.state().perSeed["s1"]?.applied).toBe(1);
    expect(monitor.ringingVersion()).toBe(before + 1);

    const before2 = monitor.ringingVersion();
    monitor.handleBatch(batch("s1", "conversation", 2n, roundDelta("t1", "hello")));
    await Promise.resolve();
    expect(monitor.ringingVersion()).toBe(before2 + 1);
    // 增量无损进入 store（切流后 ChatView 依赖版本信号重投影能看到它）
    expect(JSON.stringify(monitor.shadowOf("s1")!.conversation)).toContain("hello");
  });

  it("marks seed as ringing after cutover (reactive set)", async () => {
    vi.stubGlobal("window", {
      deepx: {
        ringing: {
          cutoverEvents: vi.fn(async () => ({ ok: true })),
          onSnapshot: () => () => undefined,
        },
      },
    });
    const monitor = createRingingMonitor();
    const before = monitor.ringingVersion();
    await monitor.cutover("s-cutover", "tool");
    expect(monitor.isRinging("s-cutover")).toBe(true);
    expect(monitor.ringingVersion()).toBeGreaterThan(before);
  });
});
