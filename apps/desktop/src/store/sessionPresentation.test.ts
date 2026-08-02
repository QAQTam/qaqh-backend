import { describe, expect, it } from "vitest";
import {
  conversationReducer,
  controlReducer,
  initialRingingStores,
  toolReducer,
} from "./ringingStores";
import { selectRingingPresentation } from "./sessionPresentation";

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
});
