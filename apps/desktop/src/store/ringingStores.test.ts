import { createStore, snapshot } from "solid-js";
import { describe, expect, it, vi } from "vitest";
import type { ConversationEvent, RingingEventEnvelope } from "../lib/types/ringing";
import {
  AppliedEventRegistry,
  applyConversationSnapshot,
  applyConversationEventToStore,
  applyEnvelope,
  applyEnvelopeUnchecked,
  conversationReducer,
  controlReducer,
  initialConversationState,
  initialControlState,
  initialRingingStores,
  initialToolState,
  toolReducer,
} from "./ringingStores";
import { selectRingingPresentation } from "./sessionPresentation";

function envelope(event: RingingEventEnvelope["event"], eventId = "e1"): RingingEventEnvelope {
  return {
    schema: "deepx.Ringing",
    version: 1,
    channel: event.channel,
    delivery: "reliable",
    server_epoch: "epoch-1",
    seed: "s1",
    stream_seq: 1,
    channel_seq: 1,
    session_seq: 1,
    event_id: eventId,
    event,
  };
}

describe("controlReducer", () => {
  it("tracks pending interaction request/resolve", () => {
    let state = initialControlState("s1");
    state = controlReducer(state, {
      type: "interaction_requested",
      interaction_id: "i1",
      turn_id: "t1",
      mode: "single",
      questions: [],
    });
    expect(state.activeAskPlan).toEqual({
      id: "i1",
      kind: "ask",
      turnId: "t1",
      mode: "single",
      questions: [],
    });
    state = controlReducer(state, { type: "interaction_resolved", interaction_id: "i1", resolution: "answered" });
    expect(state.activeAskPlan).toBeNull();
  });

  it("records failure and notice ids", () => {
    let state = initialControlState("s1");
    state = controlReducer(state, {
      type: "operation_failed",
      occurrence_id: "occ",
      scope: "tool",
      error: { error_id: "e-9", code: "x", message: "boom", retryable: false, dedupe_key: null },
      operation_id: null,
    });
    expect(state.lastFailureId).toBe("e-9");
  });

  it("keeps the full native dashboard snapshot without a legacy projection", () => {
    const state = controlReducer(initialControlState("s1"), {
      type: "dashboard_snapshot",
      snapshot: {
        seed: "s1",
        documents: [{ tag: "doc", path: "a.ts", turns_since_read: 0, is_stale: false }],
        recent_edits: ["edit: a.ts"],
        tasks: [{ id: "todo-1", subject: "Ship", description: "native", status: "in_progress" }],
        current_todo_id: "todo-1",
      },
    });
    expect(state.dashboardSnapshot?.tasks[0].subject).toBe("Ship");
    expect(state.dashboardSnapshot?.current_todo_id).toBe("todo-1");
  });
});

describe("conversationReducer", () => {
  it("assembles a turn from deltas and terminal", () => {
    let state = initialConversationState("s1");
    state = conversationReducer(state, { type: "turn_started", turn_id: "t1", user_text: "hi" });
    state = conversationReducer(state, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "thinking",
      delta: "think",
    });
    state = conversationReducer(state, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "answering",
      delta: "hello",
    });
    state = conversationReducer(state, {
      type: "round_completed",
      turn_id: "t1",
      round_num: 0,
      thinking: "think",
      answer: "hello",
      output_ref: null,
      is_final: true,
    });
    state = conversationReducer(state, { type: "turn_completed", turn_id: "t1", stop_reason: null, usage: null });
    expect(state.activeTurn?.status).toBe("completed");
    expect(state.activeTurn?.rounds[0].answer).toBe("hello");
    expect(state.activeTurn?.rounds[0].thinking).toBe("think");
  });

  it("retains the typed failure carried by turn_failed", () => {
    let state = initialConversationState("s1");
    state = conversationReducer(state, { type: "turn_started", turn_id: "t1", user_text: "hi" });
    state = conversationReducer(state, {
      type: "turn_failed",
      turn_id: "t1",
      error: {
        error_id: "failure-1",
        code: "model_request_failed",
        message: "provider rejected the request",
        retryable: false,
        dedupe_key: null,
      },
    });
    expect(state.activeTurn?.status).toBe("failed");
    expect(state.activeTurn?.failure).toEqual({
      code: "model_request_failed",
      message: "provider rejected the request",
    });
  });

  it("buffers deltas arriving before turn_started and replays them losslessly", () => {
    let state = initialConversationState("s1");
    // 乱序/快照间隙：turn_started 尚未到达
    state = conversationReducer(state, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "thinking",
      delta: "前",
    });
    state = conversationReducer(state, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "thinking",
      delta: "半",
    });
    expect(state.turns).toHaveLength(0);
    expect(state.pendingDeltas).toHaveLength(2);

    state = conversationReducer(state, { type: "turn_started", turn_id: "t1", user_text: "hi" });
    expect(state.activeTurn?.rounds[0].thinking).toBe("前半");
    expect(state.pendingDeltas).toHaveLength(0);
  });

  it("does not truncate a high-frequency stream that arrives before turn_started", () => {
    let state = initialConversationState("s1");
    const chunks = Array.from({ length: 750 }, (_, index) => String(index % 10));
    for (const delta of chunks) {
      state = conversationReducer(state, {
        type: "round_delta",
        turn_id: "t1",
        round_num: 0,
        kind: "answering",
        delta,
      });
    }

    state = conversationReducer(state, { type: "turn_started", turn_id: "t1", user_text: "hi" });
    expect(state.activeTurn?.rounds[0].answer).toBe(chunks.join(""));
    expect(state.pendingDeltas).toHaveLength(0);
  });

  it("uses the authoritative snapshot to repair an already-created streaming turn", () => {
    let state = initialConversationState("s1");
    // 流式现场：活动 turn 已有部分内容，快照合并时不得覆盖
    state = conversationReducer(state, { type: "turn_started", turn_id: "t-live", user_text: "live" });
    state = conversationReducer(state, {
      type: "round_delta",
      turn_id: "t-live",
      round_num: 0,
      kind: "answering",
      delta: "live partial",
    });

    state = applyConversationSnapshot(
      state,
      [
        { turn_id: "t-old", user_text: "old", rounds: [{ round_num: 0, is_final: true, thinking: "plan", answer: "done" }] },
        { turn_id: "t-live", user_text: "live", rounds: [{ round_num: 0, is_final: false, answer: "live complete" }] },
      ],
      "t-live",
    );

    expect(state.turns.map((t) => t.turnId)).toEqual(["t-live", "t-old"]);
    // 已存在的活动 turn 也必须以权威快照修复，否则 reset 期间漏掉的
    // delta 会永久留在 UI 中。
    const live = state.turns.find((t) => t.turnId === "t-live")!;
    expect(live.rounds[0].answer).toBe("live complete");
    expect(live.status).toBe("running");
    // 快照补齐的历史 turn 以 completed 进入
    const old = state.turns.find((t) => t.turnId === "t-old")!;
    expect(old.rounds[0].thinking).toBe("plan");
    expect(old.status).toBe("completed");
  });

  it("restores model and context limit from the snapshot for the Info panel", () => {
    let state = initialConversationState("s1");
    state = applyConversationSnapshot(
      state,
      [],
      null,
      { total_tokens: 42, prompt_tokens: 30, completion_tokens: 12, prompt_cache_hit_tokens: 0, prompt_cache_miss_tokens: 0, reasoning_tokens: 0, cache_usage_reported: false },
      null,
      undefined,
      undefined,
      undefined,
      undefined,
      "deepseek-chat",
      200_000,
    );
    expect(state.lastUsage?.model).toBe("deepseek-chat");
    expect(state.lastUsage?.contextLimit).toBe(200_000);
    expect(state.lastUsage?.usage.total_tokens).toBe(42);
  });

  it("keeps an existing live model when the snapshot omits it", () => {
    let state = initialConversationState("s1");
    state = conversationReducer(state, {
      type: "usage_updated",
      turn_id: "t1",
      round_num: 0,
      usage: { total_tokens: 5, prompt_tokens: 5, completion_tokens: 0, prompt_cache_hit_tokens: 0, prompt_cache_miss_tokens: 0, reasoning_tokens: 0, cache_usage_reported: false } as any,
      context_limit: 1000,
      model: "live-model",
    });
    state = applyConversationSnapshot(
      state,
      [],
      null,
      { total_tokens: 7, prompt_tokens: 7, completion_tokens: 0, prompt_cache_hit_tokens: 0, prompt_cache_miss_tokens: 0, reasoning_tokens: 0, cache_usage_reported: false },
    );
    expect(state.lastUsage?.model).toBe("live-model");
    expect(state.lastUsage?.contextLimit).toBe(1000);
    expect(state.lastUsage?.usage.total_tokens).toBe(7);
  });
});

describe("toolReducer", () => {
  it("covers progress tail by identity and keeps terminal", () => {
    let state = initialToolState("s1");
    state = toolReducer(state, {
      type: "tool_call_prepared",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      name: "exec",
      args_so_far: "",
    });
    state = toolReducer(state, { type: "tool_started", tool_call_id: "c1", turn_id: "t1", round_num: 0, name: "exec" });
    state = toolReducer(state, {
      type: "tool_progress",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      stream: "stdout",
      seq_start: 0,
      seq_end: 1,
      chunk: "a",
      dropped_bytes: 0,
      truncated: false,
    });
    state = toolReducer(state, {
      type: "tool_progress",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      stream: "stdout",
      seq_start: 1,
      seq_end: 2,
      chunk: "b",
      dropped_bytes: 0,
      truncated: false,
    });
    state = toolReducer(state, {
      type: "tool_finished",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      result: {
        status: "ok",
        summary: "ok",
        data: {},
        model: { text: "ok", truncated: false, total_tokens: 1 },
        output_ref: null,
      },
    });
    const card = state.cards.find((c) => c.toolCallId === "c1");
    expect(card?.status).toBe("finished");
    expect(card?.progressTail).toBe("ab");
  });

  it("permission requested sets pending flag", () => {
    let state = initialToolState("s1");
    state = toolReducer(state, {
      type: "tool_permission_requested",
      tool_call_id: "c2",
      turn_id: "t",
      round_num: 0,
      tool_name: "write",
      reason: "r",
      paths: [],
      category: "write",
      level: 3,
      risk: "high",
      consequence: "w",
    });
    expect(state.cards[0].pendingPermission).toBe(true);
  });
});

describe("applyEnvelope + idempotency", () => {
  it("applies each event_id exactly once", () => {
    const stores = initialRingingStores("s1");
    const applied = new AppliedEventRegistry();
    const ev = {
      type: "turn_started",
      turn_id: "t1",
      user_text: "hi",
    } as const;
    const env = envelope({ channel: "conversation", ...ev });
    expect(applyEnvelope(stores, env, applied)).toBe(true);
    // 幂等：相同 event_id 第二次应用被拒绝
    expect(applyEnvelope(stores, env, applied)).toBe(false);
    expect(stores.conversation.activeTurn?.turnId).toBe("t1");
  });

  it("unchecked apply handles all three channels", () => {
    const stores = initialRingingStores("s1");
    applyEnvelopeUnchecked(stores, { channel: "control", type: "agent_lifecycle_changed", state: "ready" });
    applyEnvelopeUnchecked(stores, { channel: "conversation", type: "turn_started", turn_id: "t1", user_text: "hi" });
    applyEnvelopeUnchecked(stores, {
      channel: "tool",
      type: "tool_started",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      name: "exec",
    });
    expect(stores.control.agentLifecycle).toBe("ready");
    expect(stores.conversation.activeTurn?.turnId).toBe("t1");
    expect(stores.tool.cards[0].status).toBe("running");
  });

  it("restores usage counters from snapshot and does not count duplicate live envelopes", () => {
    const stores = initialRingingStores("s1");
    stores.conversation = applyConversationSnapshot(
      stores.conversation,
      [],
      null,
      {
        prompt_tokens: 10, completion_tokens: 2, total_tokens: 12,
        prompt_cache_hit_tokens: 0, prompt_cache_miss_tokens: 10,
        reasoning_tokens: 0, cache_usage_reported: true,
      },
      {
        prompt_tokens: 100, completion_tokens: 20, total_tokens: 120,
        prompt_cache_hit_tokens: 40, prompt_cache_miss_tokens: 60,
        reasoning_tokens: 5, cache_usage_reported: true,
      },
      3,
      2,
      7,
      false,
    );
    expect(stores.conversation.usageRequestCount).toBe(3);
    expect(stores.conversation.cacheReportedRequestCount).toBe(2);
    expect(stores.conversation.usageTotals.total_tokens).toBe(120);

    const applied = new AppliedEventRegistry();
    const usageEvent = {
      channel: "conversation" as const,
      type: "usage_updated" as const,
      turn_id: "t1",
      round_num: 0,
      usage: {
        prompt_tokens: 1, completion_tokens: 1, total_tokens: 2,
        prompt_cache_hit_tokens: 1, prompt_cache_miss_tokens: 0,
        reasoning_tokens: 0, cache_usage_reported: true,
      },
      context_limit: 100,
      model: "m",
    };
    const live = envelope(usageEvent, "usage-1");
    expect(applyEnvelope(stores, live, applied)).toBe(true);
    expect(applyEnvelope(stores, live, applied)).toBe(false);
    expect(stores.conversation.usageRequestCount).toBe(4);
    expect(stores.conversation.cacheReportedRequestCount).toBe(3);
    expect(stores.conversation.usageTotals.total_tokens).toBe(122);
  });
});

describe("newly dual-emitted domain events", () => {
  it("dashboard_updated stores dashboard state", () => {
    let state = initialControlState("s1");
    state = controlReducer(state, {
      type: "dashboard_updated",
      hp_connected: true,
      session_seed: "s1",
      tool_calls_total: 3,
      tool_failures: 1,
      current_phase: "working",
      streaming: true,
    });
    expect(state.dashboard).toEqual({
      hpConnected: true,
      sessionSeed: "s1",
      toolCallsTotal: 3,
      toolFailures: 1,
      currentPhase: "working",
      streaming: true,
    });
  });

  it("usage_updated and provider_tool_status are stored", () => {
    let state = initialConversationState("s1");
    state = conversationReducer(
      state,
      {
        type: "usage_updated",
        turn_id: "t1",
        round_num: 0,
        usage: { total_tokens: 10 },
        context_limit: 1000,
        model: "m",
      } as never,
    );
    expect(state.lastUsage?.contextLimit).toBe(1000);
    expect((state.lastUsage?.usage as { total_tokens: number }).total_tokens).toBe(10);
    state = conversationReducer(state, {
      type: "provider_tool_status",
      turn_id: "t1",
      round_num: 0,
      call_id: "c1",
      tool_kind: "web_search",
      state: "completed",
    });
    expect(state.lastProviderToolStatus).toEqual({
      callId: "c1",
      toolKind: "web_search",
      state: "completed",
    });
  });

  it("tool_notice and audit_recorded are appended with a bound", () => {
    let state = initialToolState("s1");
    for (let i = 0; i < 55; i++) {
      state = toolReducer(state, {
        type: "tool_notice",
        tool_call_id: null,
        level: "info",
        message: `n${i}`,
      });
    }
    expect(state.notices).toHaveLength(50);
    expect(state.notices[49].message).toBe("n54");
    state = toolReducer(state, {
      type: "audit_recorded",
      tool_name: "exec",
      result_summary: "ok",
      success: true,
      time: "t",
      args_ref: null,
    });
    expect(state.audits).toHaveLength(1);
    expect(state.audits[0]).toEqual({
      toolName: "exec",
      resultSummary: "ok",
      success: true,
      time: "t",
    });
  });
});

describe("applyConversationEventToStore (path updates match conversationReducer)", () => {
  function streamingSequence(): ConversationEvent[] {
    return [
      { type: "turn_started", turn_id: "t1", user_text: "hi" },
      { type: "round_delta", turn_id: "t1", round_num: 0, kind: "thinking", delta: "let me " },
      { type: "round_delta", turn_id: "t1", round_num: 0, kind: "thinking", delta: "think" },
      { type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: "Hello" },
      { type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: " world" },
      {
        type: "round_completed",
        turn_id: "t1",
        round_num: 0,
        thinking: "let me think",
        answer: "Hello world",
        output_ref: null,
        is_final: true,
      },
      {
        type: "usage_updated",
        turn_id: "t1",
        round_num: 0,
        usage: {
          prompt_tokens: 5,
          completion_tokens: 3,
          total_tokens: 8,
          prompt_cache_hit_tokens: 0,
          prompt_cache_miss_tokens: 5,
          reasoning_tokens: 0,
        },
        context_limit: 1000,
        model: "m",
      },
      {
        type: "provider_tool_status",
        turn_id: "t1",
        round_num: 0,
        call_id: "c1",
        tool_kind: "web_search",
        state: "in_progress",
      },
      { type: "turn_completed", turn_id: "t1", stop_reason: null, usage: null },
      // 乱序：delta 先于 turn_started 到达（缓冲合并路径）
      { type: "round_delta", turn_id: "t2", round_num: 1, kind: "answering", delta: "buffered " },
      { type: "round_delta", turn_id: "t2", round_num: 1, kind: "answering", delta: "text" },
      { type: "turn_started", turn_id: "t2", user_text: "second" },
      // round 缺失时 round_completed 直接创建
      {
        type: "round_completed",
        turn_id: "t2",
        round_num: 2,
        thinking: "",
        answer: "final",
        output_ref: null,
        is_final: true,
      },
      {
        type: "turn_failed",
        turn_id: "t2",
        error: { error_id: "e-1", code: "boom", message: "failed", retryable: false, dedupe_key: null },
      },
      { type: "conversation_cancelled", turn_id: null },
      { type: "compact_finished", compact_id: "c-1", status: "completed" },
    ];
  }

  it("converges with conversationReducer across a full streaming sequence", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-08-04T00:00:00.000Z"));
    try {
      const seq = streamingSequence();

      // reducer 链（语义基准）
      let reduced = initialConversationState("s-path");
      for (const event of seq) reduced = conversationReducer(reduced, event);

      // path 链（生产路径）
      const [stores, setStores] = createStore(initialRingingStores("s-path"));
      for (const event of seq) applyConversationEventToStore(setStores, event);

      expect(JSON.parse(JSON.stringify(snapshot(stores.conversation)))).toEqual(
        JSON.parse(JSON.stringify(reduced)),
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps unchanged turns stable through the projection when using path updates", async () => {
    const [stores, setStores] = createStore(initialRingingStores("s-proj"));
    applyConversationEventToStore(setStores, { type: "turn_started", turn_id: "t1", user_text: "first" });
    applyConversationEventToStore(setStores, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "answering",
      delta: "answer 1",
    });
    // Solid 2 store 写入是微任务批处理：同栈读取仍为旧值，先 flush 再投影。
    await Promise.resolve();
    const p1 = selectRingingPresentation("s-proj", stores);
    const t1a = p1.turns.find(t => t.turnId === "t1");
    expect(t1a).toBeDefined();

    // t2 开始流式：t1 未变化 → RawTurn 引用必须稳定（投影缓存命中）
    applyConversationEventToStore(setStores, { type: "turn_started", turn_id: "t2", user_text: "second" });
    await Promise.resolve();
    const p2 = selectRingingPresentation("s-proj", stores);
    expect(p2.turns.find(t => t.turnId === "t1")).toBe(t1a);

    // t2 delta：t1 仍稳定，t2 自身重建（内容变化）
    applyConversationEventToStore(setStores, {
      type: "round_delta",
      turn_id: "t2",
      round_num: 0,
      kind: "answering",
      delta: "...",
    });
    await Promise.resolve();
    const p3 = selectRingingPresentation("s-proj", stores);
    expect(p3.turns.find(t => t.turnId === "t1")).toBe(t1a);
    expect(p3.turns.find(t => t.turnId === "t2")).not.toBe(
      p2.turns.find(t => t.turnId === "t2"),
    );
  });
});
