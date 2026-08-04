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
    version: 1,
    channel,
    seed,
    server_epoch: "epoch-1",
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

function sessionCreated(seed: string, commandId: string): RingingEventBatch {
  const created = batch(seed, "control", 1, {
    channel: "control",
    type: "session_state_changed",
    seed,
    state: "created",
  });
  created.envelopes[0].causation_id = commandId;
  return created;
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
    // 增量无损进入 Solid store；Ringing V1 UI 直接读取 store proxy 建立字段级依赖。
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

  it("returns the real seed from a causal SessionCreate event", async () => {
    vi.stubGlobal("window", { deepx: undefined });
    const monitor = createRingingMonitor();
    const waiting = monitor.waitForSessionCreated("cmd-create");

    monitor.handleBatch(sessionCreated("s-created", "cmd-create"));

    await expect(waiting).resolves.toBe("s-created");
  });

  it("keeps a causal create event that arrives before the waiter", async () => {
    vi.stubGlobal("window", { deepx: undefined });
    const monitor = createRingingMonitor();

    monitor.handleBatch(sessionCreated("s-early", "cmd-early"));

    await expect(monitor.waitForSessionCreated("cmd-early")).resolves.toBe("s-early");
  });
});
