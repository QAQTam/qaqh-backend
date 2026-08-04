import { describe, expect, it } from "vitest";
import {
  conversationReducer,
  controlReducer,
  initialRingingStores,
  toolReducer,
} from "./ringingStores";
import { selectRingingPresentation } from "./sessionPresentation";
import { createRawSessionState } from "./rawSession";

describe("selectRingingPresentation", () => {
  it("projects Ringing conversation data without legacy usage arguments", () => {
    const stores = initialRingingStores("seed-presentation");
    let conversation = conversationReducer(stores.conversation, {
      type: "turn_started",
      turn_id: "turn-1",
      user_text: "hello",
    });
    conversation = conversationReducer(conversation, {
      type: "round_delta",
      turn_id: "turn-1",
      round_num: 0,
      kind: "answering",
      delta: "world",
    });
    conversation = conversationReducer(conversation, {
      type: "usage_updated",
      turn_id: "turn-1",
      round_num: 0,
      usage: {
        prompt_tokens: 2,
        completion_tokens: 3,
        total_tokens: 5,
        prompt_cache_hit_tokens: 0,
        prompt_cache_miss_tokens: 2,
        reasoning_tokens: 0,
      },
      context_limit: 1000,
      model: "test-model",
    });
    stores.conversation = conversation;

    const presentation = selectRingingPresentation("seed-presentation", stores);

    expect(presentation.session.model).toBe("test-model");
    expect(presentation.session.contextLimit).toBe(1000);
    expect(presentation.session.usage?.total_tokens).toBe(5);
    expect(presentation.turns[0]?.rounds[0]?.answer).toBe("world");
    expect(presentation.telemetry).toEqual([]);
  });

  it("keeps an empty conversation projection stable", () => {
    const presentation = selectRingingPresentation(
      "seed-empty",
      initialRingingStores("seed-empty"),
    );

    expect(presentation.seed).toBe("seed-empty");
    expect(presentation.turns).toEqual([]);
    expect(presentation.pendingInteractions).toEqual([]);
  });

  it("preserves non-Ringing fallback fields while projecting Ringing usage", () => {
    const stores = initialRingingStores("seed-preserve");
    const fallback = createRawSessionState("seed-preserve");
    fallback.providerRetry = { turnId: "t", roundNum: 1, attempt: 1, maxRetries: 2, delaySecs: 1 };
    fallback.telemetry = [{ ts: 1, prompt_tokens: 1, completion_tokens: 1, total_tokens: 2, reasoning_tokens: 0, cache_hit: 0, cache_miss: 0, cache_available: false, sample_key: "x" }];
    const presentation = selectRingingPresentation("seed-preserve", stores, fallback);
    expect(presentation.providerRetry).toEqual(fallback.providerRetry);
    expect(presentation.telemetry).toEqual(fallback.telemetry);
  });

  it("keeps tool rounds, progress, and ask/plan payloads in the presentation", () => {
    const stores = initialRingingStores("seed-details");
    stores.conversation = conversationReducer(stores.conversation, {
      type: "turn_started",
      turn_id: "turn-1",
      user_text: "hello",
    });
    stores.conversation = conversationReducer(stores.conversation, {
      type: "round_delta",
      turn_id: "turn-1",
      round_num: 2,
      kind: "answering",
      delta: "working",
    });
    stores.tool = toolReducer(stores.tool, {
      type: "tool_call_prepared",
      tool_call_id: "call-1",
      turn_id: "turn-1",
      round_num: 2,
      name: "exec",
      args_so_far: "{\"command\":\"pwd\"}",
    });
    stores.tool = toolReducer(stores.tool, {
      type: "tool_progress",
      tool_call_id: "call-1",
      turn_id: "turn-1",
      round_num: 2,
      stream: "stdout",
      seq_start: 0,
      seq_end: 3,
      chunk: "ok\n",
      dropped_bytes: 0,
      truncated: false,
    });
    stores.control = controlReducer(stores.control, {
      type: "interaction_requested",
      interaction_id: "ask-1",
      turn_id: "turn-1",
      mode: "batch",
      questions: [{ id: "q1", question: "Continue?", options: ["yes"], allow_custom: true }],
    });

    const presentation = selectRingingPresentation("seed-details", stores);
    const round = presentation.turns[0]?.rounds[0];
    expect(round?.toolCalls[0]).toMatchObject({
      id: "call-1",
      args_json: "{\"command\":\"pwd\"}",
    });
    expect(round?.progress["call-1"]?.chunks[0]?.chunk).toBe("ok\n");
    expect(presentation.pendingInteractions[0]).toMatchObject({
      id: "ask-1",
      kind: "ask",
      mode: "batch",
      questions: [{ id: "q1", question: "Continue?" }],
    });
  });

  it("reuses stable projections for unchanged turns across streaming deltas", () => {
    const seed = "seed-cache";
    const stores = initialRingingStores(seed);
    // 历史 turn t1 完成
    stores.conversation = conversationReducer(stores.conversation, {
      type: "turn_started",
      turn_id: "t1",
      user_text: "first",
    });
    stores.conversation = conversationReducer(stores.conversation, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "answering",
      delta: "first answer",
    });

    const p1 = selectRingingPresentation(seed, stores);
    const t1a = p1.turns.find(t => t.turnId === "t1");
    expect(t1a).toBeDefined();

    // 新 turn t2 开始流式：t1 未变化，投影必须复用同一对象（引用稳定 →
    // Solid 跳过未变化子树，不重建 ProcessItem/Markdown 内容）。
    stores.conversation = conversationReducer(stores.conversation, {
      type: "turn_started",
      turn_id: "t2",
      user_text: "second",
    });
    const p2 = selectRingingPresentation(seed, stores);
    expect(p2.turns.find(t => t.turnId === "t1")).toBe(t1a);

    // t2 流式 delta：t1 仍稳定；t2 自身是新建对象（内容变化）
    stores.conversation = conversationReducer(stores.conversation, {
      type: "round_delta",
      turn_id: "t2",
      round_num: 0,
      kind: "answering",
      delta: "streaming...",
    });
    const p3 = selectRingingPresentation(seed, stores);
    expect(p3.turns.find(t => t.turnId === "t1")).toBe(t1a);
    const t2b = p2.turns.find(t => t.turnId === "t2");
    expect(p3.turns.find(t => t.turnId === "t2")).not.toBe(t2b);
  });

  it("rebuilds only the affected round when tool progress updates", () => {
    const seed = "seed-tool-cache";
    const stores = initialRingingStores(seed);
    stores.conversation = conversationReducer(stores.conversation, {
      type: "turn_started",
      turn_id: "t1",
      user_text: "run",
    });
    stores.conversation = conversationReducer(stores.conversation, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "answering",
      delta: "running tool",
    });
    stores.tool = toolReducer(stores.tool, {
      type: "tool_call_prepared",
      tool_call_id: "call-1",
      turn_id: "t1",
      round_num: 0,
      name: "exec",
      args_so_far: "{}",
    });

    const p1 = selectRingingPresentation(seed, stores);
    const roundBefore = p1.turns[0]?.rounds[0];
    expect(roundBefore?.toolCalls[0]?.id).toBe("call-1");

    // tool_progress 到达：conversation 未变，但该 round 的 progress 必须更新
    stores.tool = toolReducer(stores.tool, {
      type: "tool_progress",
      tool_call_id: "call-1",
      turn_id: "t1",
      round_num: 0,
      stream: "stdout",
      seq_start: 0,
      seq_end: 5,
      chunk: "out",
      dropped_bytes: 0,
      truncated: false,
    });
    const p2 = selectRingingPresentation(seed, stores);
    const roundAfter = p2.turns[0]?.rounds[0];
    expect(roundAfter?.progress["call-1"]?.chunks[0]?.chunk).toBe("out");
    // 变化 round 重建；未变化的 turn 对象保持稳定
    expect(roundAfter).not.toBe(roundBefore);
  });
});
