import { describe, expect, it } from "vitest";
import type { Agent2Ui } from "../lib/types";
import {
  createRawSessionState,
  reduceAgentEvent,
  removeTurnFromSession,
  resolvePendingInteraction,
} from "./sessionEventReducer";

describe("sessionEventReducer", () => {
  it("retains final-round, code-delta, and ordered exec facts", () => {
    let state = createRawSessionState("seed-a");
    state = reduceAgentEvent(state, {
      type: "turn_start", turn_id: "t1", user_text: "go",
    }, 100);
    state = reduceAgentEvent(state, {
      type: "round_complete",
      turn_id: "t1",
      round_num: 1,
      answer: "done",
      tool_calls: [{ id: "exec-1", name: "exec_run", args_display: "pnpm test", args_json: "{}" }],
      blocks: [{ type: "text", content: "done" }],
      is_final: true,
    }, 200);
    state = reduceAgentEvent(state, {
      type: "exec_progress", tool_call_id: "exec-1", stream: "stdout", seq: 2n, chunk: "B",
    }, 205);
    state = reduceAgentEvent(state, {
      type: "exec_progress", tool_call_id: "exec-1", stream: "stderr", seq: 1n, chunk: "E",
    }, 206);
    state = reduceAgentEvent(state, {
      type: "code_delta", lines_added: 7, lines_removed: 2,
      files_created: 0, files_deleted: 0, file: "src/App.tsx",
    }, 210);

    expect(state.turns[0].rounds[0].isFinal).toBe(true);
    expect(state.turns[0].rounds[0].progress["exec-1"].chunks.map(item => item.seq)).toEqual([1, 2]);
    expect(state.environment.linesAdded).toBe(7);
  });

  it("restores final round facts and pending permission risk", () => {
    let state = createRawSessionState("seed-a");
    state = reduceAgentEvent(state, {
      type: "session_restored",
      seed: "seed-a",
      turns: [{
        turn_id: "t1",
        user_text: "restore",
        rounds: [{ round_num: 0, is_final: true, thinking: null, answer: "restored", tool_calls: [], tool_results: [] }],
      }],
      tokens_used: 10,
      cache_hit_pct: 0,
      total_turns: 1,
      has_more: false,
    }, 100);
    state = reduceAgentEvent(state, {
      type: "permission_request",
      tool_call_id: "danger-1",
      tool_name: "exec_run",
      reason: "Run command",
      paths: [],
      category: "exec",
      level: 1,
      risk: "high",
      consequence: "May execute arbitrary actions.",
    }, 110);

    expect(state.turns[0].rounds[0].isFinal).toBe(true);
    expect(state.pendingInteractions[0]?.kind).toBe("permission");
    if (state.pendingInteractions[0]?.kind === "permission") {
      expect(state.pendingInteractions[0].risk).toBe("high");
    }
  });

  it("accepts every generated event variant without dropping session identity", () => {
    const lifecycleEvents: Agent2Ui[] = [
      { type: "ready" },
      { type: "pong" },
      { type: "done" },
      { type: "shutdown_ack" },
    ];
    const final = lifecycleEvents.reduce(
      (state, event) => reduceAgentEvent(state, event, 1),
      createRawSessionState("seed-a"),
    );
    expect(final.seed).toBe("seed-a");
  });

  it("tracks plan review as a waiting interaction and resolves it", () => {
    let state = createRawSessionState("seed-a");
    state = reduceAgentEvent(state, {
      type: "turn_start", turn_id: "t-plan", user_text: "plan",
    }, 100);
    state = reduceAgentEvent(state, {
      type: "plan_submitted", call_id: "plan-1", plan_content: "# Plan",
    }, 110);

    expect(state.turns[0].status).toBe("waiting");
    expect(state.pendingInteractions[0]).toEqual({
      kind: "plan", id: "plan-1", turnId: "t-plan", content: "# Plan",
    });

    state = reduceAgentEvent(state, {
      type: "plan_resolved", call_id: "plan-1", approved: true,
    }, 120);

    expect(state.turns[0].status).toBe("running");
    expect(state.pendingInteractions).toEqual([]);
    expect(state.turns[0].interactions[state.turns[0].interactions.length - 1]).toMatchObject({
      id: "plan-1", kind: "plan", resolution: "approved",
    });
  });

  it("does not duplicate consecutive notices when lifecycle events are replayed", () => {
    let state = createRawSessionState("seed-a");
    const event = { type: "error" as const, message: "agent exited" };
    state = reduceAgentEvent(state, event, 100);
    state = reduceAgentEvent(state, event, 110);
    expect(state.notices).toHaveLength(1);
  });

  it("queues interactions without overwriting an earlier gate", () => {
    let state = reduceAgentEvent(createRawSessionState("seed-a"), {
      type: "turn_start", turn_id: "t1", user_text: "run",
    }, 1);
    state = reduceAgentEvent(state, {
      type: "permission_request", tool_call_id: "perm-1", tool_name: "exec",
      reason: "run", paths: [], category: "exec", level: 4,
      risk: "medium", consequence: "runs a process",
    }, 2);
    state = reduceAgentEvent(state, {
      type: "ask_user", turn_id: "t1", round_num: 0, ask_id: "ask-1",
      mode: "single", questions: [{ id: "q1", question: "Continue?", options: [], allow_custom: true }],
    }, 3);

    expect(state.pendingInteractions.map(item => item.id)).toEqual(["perm-1", "ask-1"]);
    state = resolvePendingInteraction(state, "perm-1", "approved", 4);
    expect(state.pendingInteractions.map(item => item.id)).toEqual(["ask-1"]);
    expect(state.turns[0].status).toBe("waiting");
  });

  it("preserves streamed text and previews when round_complete omits optional fields", () => {
    let state = reduceAgentEvent(createRawSessionState("seed-a"), {
      type: "turn_start", turn_id: "t1", user_text: "run",
    }, 1);
    state = reduceAgentEvent(state, {
      type: "round_delta", turn_id: "t1", round_num: 0, kind: "thinking", delta: "think",
    }, 2);
    state = reduceAgentEvent(state, {
      type: "tool_call_preview", turn_id: "t1", round_num: 0, index: 0,
      id: "call-1", name: "exec", args_so_far: "{\"cmd\":\"dir\"}",
    }, 3);
    state = reduceAgentEvent(state, {
      type: "round_complete", turn_id: "t1", round_num: 0, is_final: false,
    }, 4);

    expect(state.turns[0].rounds[0].thinking).toBe("think");
    expect(state.turns[0].rounds[0].toolCalls[0].id).toBe("call-1");
  });

  it("maps usage, dashboard, audit, compact completion, and undo", () => {
    let state = reduceAgentEvent(createRawSessionState("seed-a"), {
      type: "turn_start", turn_id: "t1", user_text: "run",
    }, 10);
    state = reduceAgentEvent(state, {
      type: "turn_end", turn_id: "t1", usage: {
        prompt_tokens: 100, completion_tokens: 20, total_tokens: 120,
        prompt_cache_hit_tokens: 80, prompt_cache_miss_tokens: 20, reasoning_tokens: 5,
      },
    }, 20);
    state = reduceAgentEvent(state, {
      type: "dashboard", hp_connected: true, session_seed: "seed-a",
      tool_calls_total: 1, tool_failures: 0, current_phase: "done", streaming: false,
      dsml_compat_count: 0, tasks: [], recent_edits: ["src/a.ts"],
      session_title: "Title", context_limit: 200000, model: "model-a",
    }, 21);
    state = reduceAgentEvent(state, {
      type: "audit_record", tool_name: "exec", result_summary: "ok", success: true,
      time: "2026-07-16T00:00:00Z", args: "{}",
    }, 22);
    state = reduceAgentEvent(state, { type: "compact_start", turns_total: 4, turns_keeping: 2 }, 23);
    state = reduceAgentEvent(state, { type: "compact_delta", delta: "summary" }, 24);
    state = reduceAgentEvent(state, { type: "compact_end", summary_chars: 7, turns_compacted: 2, turns_removed: 2 }, 25);

    expect(state.session.usage?.total_tokens).toBe(120);
    expect(state.dashboard.recentEdits).toEqual(["src/a.ts"]);
    expect(state.dashboard.activity[0].toolName).toBe("exec");
    expect(state.compact).toMatchObject({ active: false, turnsCompacted: 2 });
    expect(state.telemetry[state.telemetry.length - 1]?.prompt_tokens).toBe(100);
    expect(removeTurnFromSession(state, "t1").turns).toHaveLength(1); // compact summary turn remains
  });

  it("deduplicates the Dashboard and TurnEnd copies of one usage snapshot", () => {
    const usage = {
      prompt_tokens: 100, completion_tokens: 20, total_tokens: 120,
      prompt_cache_hit_tokens: 0, prompt_cache_miss_tokens: 0, reasoning_tokens: 0,
    };
    let state = reduceAgentEvent(createRawSessionState("seed-a"), {
      type: "turn_start", turn_id: "t1", user_text: "run",
    }, 10);
    state = reduceAgentEvent(state, {
      type: "dashboard", hp_connected: true, session_seed: "seed-a", tool_calls_total: 0,
      tool_failures: 0, current_phase: "answering", streaming: true, dsml_compat_count: 0,
      tasks: [], recent_edits: [], session_title: "Title", context_limit: 200000, model: "model-a", usage,
    }, 20);
    state = reduceAgentEvent(state, { type: "turn_end", turn_id: "t1", usage }, 30);

    expect(state.telemetry).toHaveLength(1);
    expect(state.telemetry[0]).toMatchObject({ prompt_tokens: 100, cache_available: false });
    expect(state.telemetry[0].ts).toBe(30);
  });

  it("treats done as a terminal fallback and resets a newly created seed", () => {
    let state = reduceAgentEvent(createRawSessionState("old"), {
      type: "turn_start", turn_id: "t1", user_text: "run",
    }, 1);
    state = reduceAgentEvent(state, { type: "done" }, 2);
    expect(state.turns[0].status).toBe("completed");

    state = reduceAgentEvent(state, { type: "session_created", seed: "new" }, 3);
    expect(state.seed).toBe("new");
    expect(state.turns).toEqual([]);
    expect(state.session.ready).toBe(true);
  });

  it("is idempotent for replayed lifecycle and pagination events", () => {
    let state = reduceAgentEvent(createRawSessionState("seed-a"), {
      type: "session_created", seed: "seed-a",
    }, 1);
    state = reduceAgentEvent(state, { type: "turn_start", turn_id: "t1", user_text: "run" }, 2);
    state = reduceAgentEvent(state, { type: "turn_end", turn_id: "t1" }, 3);
    const completed = state;
    state = reduceAgentEvent(state, { type: "turn_end", turn_id: "t1" }, 4);
    expect(state).toBe(completed);

    const older = { turn_id: "older", user_text: "old", rounds: [] };
    state = reduceAgentEvent(state, { type: "more_turns", turns: [older], has_more: false }, 5);
    state = reduceAgentEvent(state, { type: "more_turns", turns: [older], has_more: false }, 6);
    expect(state.turns.filter(turn => turn.turnId === "older")).toHaveLength(1);
  });

  it("drops stale skills snapshots by operation revision", () => {
    const snapshot = (revision: bigint, state: string): Agent2Ui => ({
      type: "skills_changed", available: [], active: [], catalog_revision: "cat",
      context_epoch: revision, operation_revision: revision,
      token_budget: 100, token_usage: 10,
      runtime: [{ name: "alpha", description: "A", state, source: "model", token_count: 10 }],
      diagnostics: [],
    });
    let state = reduceAgentEvent(createRawSessionState("seed"), snapshot(4n, "active"), 1);
    const current = state;
    state = reduceAgentEvent(state, snapshot(3n, "catalog"), 2);
    expect(state).toBe(current);
    expect(state.skills.runtime[0].state).toBe("active");
  });

  it("replaces compacted turns with summary turn on compact_end", () => {
    let state = createRawSessionState("seed-a");
    // Build 3 turns
    state = reduceAgentEvent(state, {
      type: "turn_start", turn_id: "t1", user_text: "hello",
    }, 1);
    state = reduceAgentEvent(state, { type: "turn_end", turn_id: "t1" }, 2);
    state = reduceAgentEvent(state, {
      type: "turn_start", turn_id: "t2", user_text: "world",
    }, 3);
    state = reduceAgentEvent(state, { type: "turn_end", turn_id: "t2" }, 4);
    state = reduceAgentEvent(state, {
      type: "turn_start", turn_id: "t3", user_text: "keep me",
    }, 5);
    state = reduceAgentEvent(state, { type: "turn_end", turn_id: "t3" }, 6);
    expect(state.turns).toHaveLength(3);

    // Start compact
    state = reduceAgentEvent(state, {
      type: "compact_start", turns_total: 3, turns_keeping: 1,
    }, 7);
    expect(state.compact.active).toBe(true);

    // Stream summary deltas
    state = reduceAgentEvent(state, {
      type: "compact_delta", delta: "Summarized 2 turns.",
    }, 8);
    expect(state.compact.text).toBe("Summarized 2 turns.");

    // Finish compact: remove first 2 turns, prepend summary
    state = reduceAgentEvent(state, {
      type: "compact_end",
      summary_chars: 21,
      turns_compacted: 2,
      turns_removed: 2,
    }, 9);
    expect(state.compact.active).toBe(false);
    expect(state.compact.turnsCompacted).toBe(2);
    expect(state.compact.text).toBe("Summarized 2 turns."); // preserved
    expect(state.turns).toHaveLength(2);
    // First turn is the compact summary
    expect(state.turns[0].turnId).toMatch(/^compact-/);
    expect(state.turns[0].userText).toBe("Summarized 2 turns.");
    expect(state.turns[0].status).toBe("completed");
    expect(state.turns[0].rounds).toEqual([]);
    // Second turn is the kept turn
    expect(state.turns[1].turnId).toBe("t3");
    expect(state.turns[1].userText).toBe("keep me");
  });

  it("handles compact_end with zero turns_removed gracefully", () => {
    let state = createRawSessionState("seed-b");
    state = reduceAgentEvent(state, {
      type: "turn_start", turn_id: "t1", user_text: "only turn",
    }, 1);
    state = reduceAgentEvent(state, { type: "turn_end", turn_id: "t1" }, 2);
    state = reduceAgentEvent(state, {
      type: "compact_start", turns_total: 1, turns_keeping: 1,
    }, 3);
    state = reduceAgentEvent(state, {
      type: "compact_delta", delta: "No-op compact.",
    }, 4);
    state = reduceAgentEvent(state, {
      type: "compact_end",
      summary_chars: 14,
      turns_compacted: 0,
      turns_removed: 0,
    }, 5);
    expect(state.compact.active).toBe(false);
    expect(state.turns).toHaveLength(1); // unchanged
    expect(state.turns[0].turnId).toBe("t1");
  });
});
