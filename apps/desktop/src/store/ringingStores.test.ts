import { describe, expect, it } from "vitest";
import type { RingingEventEnvelope } from "../lib/types/ringing";
import {
  AppliedEventRegistry,
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

function envelope(event: RingingEventEnvelope["event"], eventId = "e1"): RingingEventEnvelope {
  return {
    schema: "deepx.Ringing",
    version: 1,
    channel: event.channel,
    delivery: "reliable",
    server_epoch: "epoch-1",
    seed: "s1",
    stream_seq: 1n,
    channel_seq: 1n,
    session_seq: 1n,
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
    expect(state.activeAskPlan).toEqual({ id: "i1", kind: "ask", turnId: "t1" });
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
      seq_start: 0n,
      seq_end: 1n,
      chunk: "a",
      dropped_bytes: 0n,
      truncated: false,
    });
    state = toolReducer(state, {
      type: "tool_progress",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      stream: "stdout",
      seq_start: 1n,
      seq_end: 2n,
      chunk: "ab",
      dropped_bytes: 0n,
      truncated: false,
    });
    state = toolReducer(state, {
      type: "tool_finished",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      result: { success: true, summary: "ok", output_ref: null },
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
});
