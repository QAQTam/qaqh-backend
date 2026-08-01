import { afterEach, describe, expect, it, vi } from "vitest";
import { createRingingMonitor } from "./ringingMonitor";
import type { RingingEventBatch } from "../lib/types/ringing";

function batch(
  seed: string,
  channel: "control" | "conversation" | "tool",
  seq: number,
  event: Record<string, unknown>,
): RingingEventBatch {
  return {
    schema: "deepx.Ringing",
    version: 2,
    channel,
    seed,
    server_epoch: "epoch-1",
    from_stream_seq: seq,
    to_stream_seq: seq,
    envelopes: [{
      schema: "deepx.Ringing",
      version: 2,
      channel,
      delivery: "reliable",
      server_epoch: "epoch-1",
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
  it("bumps ringingVersion on every applied batch and updates typed stores", async () => {
    vi.stubGlobal("window", { deepx: undefined });
    const monitor = createRingingMonitor();
    const before = monitor.ringingVersion();

    monitor.handleBatch(batch("s1", "conversation", 1, turnStarted("t1", "hi")));
    await Promise.resolve();
    const stores = monitor.storesFor("s1");
    expect(stores?.conversation.turns).toHaveLength(1);
    expect(stores?.conversation.turns[0].turnId).toBe("t1");
    expect(monitor.state().perSeed["s1"]?.applied).toBe(1);
    expect(monitor.ringingVersion()).toBe(before + 1);

    const before2 = monitor.ringingVersion();
    monitor.handleBatch(batch("s1", "conversation", 2, roundDelta("t1", "hello")));
    await Promise.resolve();
    expect(monitor.ringingVersion()).toBe(before2 + 1);
    // 增量无损进入 Solid store；v2 UI 直接读取 store proxy 建立字段级依赖。
    expect(JSON.stringify(monitor.storesFor("s1")!.conversation)).toContain("hello");
  });

  it("marks seed as ringing after bootstrap activation", async () => {
    vi.stubGlobal("window", {
      deepx: {
        ringing: {
          status: vi.fn(async () => ({
            control: { state: "connected" },
            conversation: { state: "connected" },
            tool: { state: "connected" },
          })),
          snapshot: vi.fn(async () => ({ ok: true })),
          onSnapshot: () => () => undefined,
        },
      },
    });
    const monitor = createRingingMonitor();
    const before = monitor.ringingVersion();
    await monitor.activate("s-bootstrap");
    expect(monitor.hasStores("s-bootstrap")).toBe(true);
    expect(monitor.ringingVersion()).toBeGreaterThan(before);
  });

  it("keeps session stores scoped to the monitor instance", () => {
    vi.stubGlobal("window", { deepx: undefined });
    const first = createRingingMonitor();
    first.handleBatch(batch("s-isolated", "conversation", 1, turnStarted("t1", "hello")));

    const second = createRingingMonitor();
    expect(second.hasStores("s-isolated")).toBe(false);
    expect(second.storesFor("s-isolated")).toBeUndefined();
  });
});
