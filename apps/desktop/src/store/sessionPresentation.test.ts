import { describe, expect, it } from "vitest";
import {
  conversationReducer,
  controlReducer,
  initialRingingStores,
  toolReducer,
} from "./ringingStores";
import {
  emptySkillsPresentation,
  selectRingingPresentation,
  selectSkillsPresentation,
} from "./sessionPresentation";
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

  it("keeps every tool card when a round has multiple tool calls", () => {
    const seed = "seed-multi-tool";
    const stores = initialRingingStores(seed);
    stores.conversation = conversationReducer(stores.conversation, {
      type: "turn_started",
      turn_id: "t1",
      user_text: "go",
    });
    // RawRound 投影以 conversation 的 round 为宿主：先建 round 0
    stores.conversation = conversationReducer(stores.conversation, {
      type: "round_delta",
      turn_id: "t1",
      round_num: 0,
      kind: "answering",
      delta: "",
    });
    // 同一 round 两个工具：call-1 先到（流式），call-2 后到
    stores.tool = toolReducer(stores.tool, {
      type: "tool_call_prepared",
      tool_call_id: "call-1",
      turn_id: "t1",
      round_num: 0,
      name: "exec",
      args_so_far: "{}",
    });
    const p1 = selectRingingPresentation(seed, stores);
    expect(p1.turns[0]?.rounds[0]?.toolCalls.map(call => call.id)).toEqual(["call-1"]);

    stores.tool = toolReducer(stores.tool, {
      type: "tool_call_prepared",
      tool_call_id: "call-2",
      turn_id: "t1",
      round_num: 0,
      name: "write",
      args_so_far: "{}",
    });
    const p2 = selectRingingPresentation(seed, stores);
    // 回归：旧实现缓存单 card，第二个工具到达时挤掉第一个
    expect(p2.turns[0]?.rounds[0]?.toolCalls.map(call => call.id)).toEqual(["call-1", "call-2"]);

    // 结果同样保留：call-1 finished 进入 toolResults；call-2 未完成不进入
    stores.tool = toolReducer(stores.tool, {
      type: "tool_finished",
      tool_call_id: "call-1",
      turn_id: "t1",
      round_num: 0,
      result: {
        status: "ok",
        summary: "done",
        data: null,
        model: { text: "done", truncated: false, total_tokens: 1 },
      },
    });
    const p3 = selectRingingPresentation(seed, stores);
    const round = p3.turns[0]?.rounds[0];
    expect(round?.toolCalls.map(call => call.id)).toEqual(["call-1", "call-2"]);
    expect(Object.keys(round?.toolResults ?? {})).toEqual(["call-1"]);
  });

  it("keeps the round stable once tool cards stop changing", () => {
    const seed = "seed-multi-settle";
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
      delta: "",
    });
    for (const call of ["call-1", "call-2"]) {
      stores.tool = toolReducer(stores.tool, {
        type: "tool_call_prepared",
        tool_call_id: call,
        turn_id: "t1",
        round_num: 0,
        name: call === "call-1" ? "exec" : "write",
        args_so_far: "{}",
      });
    }
    const p1 = selectRingingPresentation(seed, stores);
    const round1 = p1.turns[0]?.rounds[0];
    expect(round1?.toolCalls.map(call => call.id)).toEqual(["call-1", "call-2"]);

    // 之后只有 conversation 事件（cards 引用不变）→ 该 round 必须复用同一对象
    stores.conversation = conversationReducer(stores.conversation, {
      type: "usage_updated",
      turn_id: "t1",
      round_num: 0,
      usage: {
        prompt_tokens: 1,
        completion_tokens: 1,
        total_tokens: 2,
        prompt_cache_hit_tokens: 0,
        prompt_cache_miss_tokens: 1,
        reasoning_tokens: 0,
      },
      context_limit: 1000,
      model: "m",
    });
    const p2 = selectRingingPresentation(seed, stores);
    expect(p2.turns[0]?.rounds[0]).toBe(round1);
  });

  it("projects the skills domain independently of turns", () => {
    const seed = "seed-skills";
    const stores = initialRingingStores(seed);
    stores.control = controlReducer(stores.control, {
      type: "skills_updated",
      available: [
        { name: "frontend-design", description: "UI", source: "catalog", revision: "r1" },
      ],
      active: ["frontend-design"],
      catalog_revision: "r1",
      operation_revision: 3,
      context_epoch: 2,
      token_budget: 10000,
      token_usage: 500,
      runtime: [
        { name: "frontend-design", description: "UI", source: "catalog", state: "active", token_count: 100 },
      ],
      diagnostics: ["ok"],
    });

    // 域投影：不经过 selectRingingPresentation（不投影 turns）
    const skills = selectSkillsPresentation(stores);
    expect(skills?.active).toEqual(["frontend-design"]);
    expect(skills?.catalogRevision).toBe("r1");
    expect(skills?.operationRevision).toBe(3);
    expect(skills?.contextEpoch).toBe(2);
    expect(skills?.tokenBudget).toBe(10000);
    expect(skills?.tokenUsage).toBe(500);
    expect(skills?.runtime?.[0]?.state).toBe("active");

    // 与全量投影的 skills 域一致（抽离不改变行为）
    const full = selectRingingPresentation(seed, stores);
    expect(JSON.parse(JSON.stringify(skills))).toEqual(
      JSON.parse(JSON.stringify(full.skills)),
    );
  });

  it("returns null without a skills snapshot and provides an empty fallback", () => {
    const stores = initialRingingStores("seed-no-skills");
    expect(selectSkillsPresentation(stores)).toBeNull();
    expect(emptySkillsPresentation()).toEqual({
      available: [],
      active: [],
      catalogRevision: "",
      contextEpoch: 0,
      operationRevision: 0,
      tokenBudget: 0,
      tokenUsage: 0,
      runtime: [],
      diagnostics: [],
    });
  });
});
