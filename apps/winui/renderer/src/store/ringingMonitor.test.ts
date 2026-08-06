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

  it("bootstrap restores activity and clears ghost interactions/tools", async () => {
    vi.stubGlobal("window", {
      deepx: {
        ringing: {
          status: vi.fn(async () => ({
            control: { state: "connected" },
            conversation: { state: "connected" },
            tool: { state: "connected" },
          })),
          bootstrap: vi.fn(async () => ({
            control: {
              state: {
                agent_lifecycle: "ready",
                session_state: "resumed",
                activity: "working",
                // 权威快照无挂起交互 → 本地幽灵 ask 面板必须被清除
                pending_interaction: null,
              },
              baseline_stream_seq: 10,
            },
            conversation: { state: {}, baseline_stream_seq: 10 },
            tool: {
              state: { pending_permission: null },
              baseline_stream_seq: 10,
            },
          })),
          onSnapshot: () => () => undefined,
        },
      },
    });
    const monitor = createRingingMonitor();
    // 重放/历史事件先于 bootstrap 到达：幽灵 ask 面板 + 幽灵授权卡片
    monitor.handleBatch(batch("s-ghost", "control", 1, {
      channel: "control",
      type: "interaction_requested",
      interaction_id: "i1",
      turn_id: "t1",
      mode: "single",
      questions: [],
    }));
    monitor.handleBatch(batch("s-ghost", "tool", 2, {
      channel: "tool",
      type: "tool_permission_requested",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      tool_name: "exec",
      reason: "r",
      paths: [],
      category: "exec",
      level: 3,
      risk: "high",
      consequence: "run",
    }));
    await Promise.resolve();
    expect(monitor.storesFor("s-ghost")?.control.activeAskPlan).not.toBeNull();
    expect(monitor.storesFor("s-ghost")?.tool.cards[0].pendingPermission).toBe(true);

    await monitor.activate("s-ghost");

    // 快照权威收敛：activity 恢复、幽灵 ask 清除、幽灵授权卡片清除
    expect(monitor.storesFor("s-ghost")?.control.activity).toBe("working");
    expect(monitor.storesFor("s-ghost")?.control.agentLifecycle).toBe("ready");
    expect(monitor.storesFor("s-ghost")?.control.activeAskPlan).toBeNull();
    expect(monitor.storesFor("s-ghost")?.tool.cards[0].pendingPermission).toBe(false);
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

  it("reloads the authoritative bootstrap snapshot after compact_finished (completed)", async () => {
    const bootstrap = vi.fn(async () => ({
      conversation: {
        state: {
          turns: [
            { turn_id: "compact-summary", user_text: "[Compacted 1 turns]\nsummary", rounds: [] },
            { turn_id: "t2", user_text: "kept", rounds: [{ round_num: 0, is_final: true, thinking: "", answer: "ok" }] },
          ],
          total_turns: 2,
          active_turn: null,
          compact_status: "completed",
        },
        baseline_stream_seq: 0,
      },
    }));
    vi.stubGlobal("window", {
      deepx: {
        ringing: {
          status: vi.fn(async () => ({ conversation: { state: "connected" } })),
          bootstrap,
        },
      },
    });
    const monitor = createRingingMonitor();
    // 本地已有两个 turn（t1 将被压缩移除、t2 保留）
    monitor.handleBatch(batch("s-compact", "conversation", 1, turnStarted("t1", "old")));
    monitor.handleBatch(batch("s-compact", "conversation", 2, turnStarted("t2", "kept")));
    await Promise.resolve();
    expect(monitor.storesFor("s-compact")!.conversation.turns.map((t) => t.turnId)).toEqual(["t1", "t2"]);

    // compact_finished(completed) → 触发权威重拉（替换语义）
    monitor.handleBatch(batch("s-compact", "conversation", 3, {
      channel: "conversation",
      type: "compact_finished",
      compact_id: "c1",
      status: "completed",
      turns_compacted: 1,
    }));
    await vi.waitFor(() => {
      expect(bootstrap).toHaveBeenCalledWith("s-compact");
    });
    await vi.waitFor(() => {
      const ids = monitor.storesFor("s-compact")!.conversation.turns.map((t) => t.turnId);
      expect(ids).toEqual(["compact-summary", "t2"]);
    });
  });

  it("does not reload on compact_finished with a non-completed status", async () => {
    const bootstrap = vi.fn(async () => ({ conversation: { state: { turns: [] } } }));
    vi.stubGlobal("window", {
      deepx: {
        ringing: {
          status: vi.fn(async () => ({ conversation: { state: "connected" } })),
          bootstrap,
        },
      },
    });
    const monitor = createRingingMonitor();
    monitor.handleBatch(batch("s-fail", "conversation", 1, {
      channel: "conversation",
      type: "compact_finished",
      compact_id: "c2",
      status: "failed",
      turns_compacted: null,
    }));
    await Promise.resolve();
    expect(bootstrap).not.toHaveBeenCalled();
  });
