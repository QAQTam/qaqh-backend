import { describe, expect, it } from "vitest";
import { createTimelineMonitor } from "./timelineMonitor";
import { mergeTimelinePresentation, selectTimelinePresentation } from "./timelinePresentation";
import { createRawSessionState } from "./rawSession";
import type { RawTurn } from "./rawSession";
import type { TimelineSnapshotResponse } from "./timelineProtocol";

function snapshot(): TimelineSnapshotResponse {
  return {
    schema: "deepx.Ringing",
    version: 1,
    server_epoch: "epoch",
    seed: "seed",
    snapshot: {
      watermark: 1,
      turns: [{
        turn_id: "turn",
        user_text: "hello",
        sealed: false,
        state: "running",
        rounds: [{
          round_num: 0,
          sealed: false,
          is_final: false,
          blocks: [{
            block_id: "answer",
            block_order: 0,
            kind: "text",
            state: "open",
            text: "partial",
          }],
        }],
      }],
    },
  };
}

describe("Ringing V1 timeline renderer monitor", () => {
  it("requires a contiguous cursor and renders text deltas before block seal", () => {
    const monitor = createTimelineMonitor();
    monitor.handleSnapshot(snapshot());
    const revisionFor = (turnId: string) => monitor.turnRevisionFor("seed", turnId);
    expect(revisionFor("turn")).toBe(0);
    expect(monitor.handleEntry("seed", {
      timeline_seq: 3,
      turn_id: "turn",
      round_num: 0,
      event: { type: "text_delta", block_id: "answer", fragment_seq: 1, delta: " lost" },
    })).toBe(false);
    expect(monitor.handleEntry("seed", {
      timeline_seq: 2,
      turn_id: "turn",
      round_num: 0,
      event: { type: "text_delta", block_id: "answer", fragment_seq: 1, delta: " text" },
    })).toBe(true);
    expect(revisionFor("turn")).toBe(1);

    const current = monitor.snapshotFor("seed")!;
    expect(selectTimelinePresentation("seed", current, createRawSessionState("seed"), revisionFor).turns[0].rounds[0].answer)
      .toBe("partial text");
    expect(monitor.handleEntry("seed", {
      timeline_seq: 3,
      turn_id: "turn",
      round_num: 0,
      event: { type: "block_sealed", block_id: "answer" },
    })).toBe(true);
    expect(selectTimelinePresentation("seed", monitor.snapshotFor("seed")!, createRawSessionState("seed"), revisionFor)
      .turns[0].rounds[0].answer).toBe("partial text");
  });

  it("keeps ordered reasoning, tool, and text blocks and rejects late text", () => {
    const monitor = createTimelineMonitor();
    const response = snapshot();
    response.snapshot.turns[0]!.rounds[0]!.blocks = [
      { block_id: "reasoning", block_order: 0, kind: "reasoning", state: "sealed", text: "think" },
      {
        block_id: "tool", block_order: 1, kind: "tool", state: "sealed",
        tool: { tool_call_id: "tool", name: "exec", state: "succeeded", args_json: "{}" },
      },
      { block_id: "answer", block_order: 2, kind: "text", state: "open", text: "answer" },
    ];
    monitor.handleSnapshot(response);
    const turn = selectTimelinePresentation("seed", monitor.snapshotFor("seed")!, createRawSessionState("seed")).turns[0]!;
    expect(turn.rounds[0]!.blocks.map(block => block.type)).toEqual(["reasoning", "tool", "text"]);
    expect(monitor.handleEntry("seed", {
      timeline_seq: 2,
      turn_id: "turn",
      round_num: 0,
      event: { type: "text_delta", block_id: "reasoning", fragment_seq: 0, delta: "late" },
    })).toBe(false);
  });

  it("preserves a terminal provider failure for presentation", () => {
    const monitor = createTimelineMonitor();
    const response = snapshot();
    response.snapshot.turns[0]!.sealed = true;
    response.snapshot.turns[0]!.state = "failed";
    response.snapshot.turns[0]!.failure = {
      code: "model_request_failed",
      message: "provider rejected the tool contract",
    };
    monitor.handleSnapshot(response);

    const turn = selectTimelinePresentation(
      "seed",
      monitor.snapshotFor("seed")!,
      createRawSessionState("seed"),
    ).turns[0]!;
    expect(turn.status).toBe("failed");
    expect(turn.failure).toEqual({
      code: "model_request_failed",
      message: "provider rejected the tool contract",
    });
  });

  it("stamps running turns with projected timestamps so stall detection can age them", () => {
    const monitor = createTimelineMonitor();
    const response = snapshot();
    const before = Date.now();
    monitor.handleSnapshot(response);
    const turn = selectTimelinePresentation(
      "seed",
      monitor.snapshotFor("seed")!,
      createRawSessionState("seed"),
    ).turns[0]!;
    expect(turn.status).toBe("running");
    expect(turn.startedAt).toBeGreaterThanOrEqual(before);
    expect(turn.lastActivityAt).toBeGreaterThanOrEqual(before);
  });

  it("backfills turns missing from a stale timeline snapshot from the conversation store", () => {
    // Regression: timeline persistence is a best-effort async checkpoint, so
    // after a daemon restart the snapshot can lag the authoritative message
    // store (whose turns are what refresh the session-list title). The merge
    // must keep those store-only turns visible instead of blanking them.
    const monitor = createTimelineMonitor();
    const response = snapshot();
    response.snapshot.turns[0]!.turn_id = "t1";
    monitor.handleSnapshot(response); // snapshot only knows t1

    const storeTurns: RawTurn[] = [
      { turnId: "t1", userText: "hello", status: "completed" as const, startedAt: 1, rounds: [], interactions: [] },
      { turnId: "t2", userText: "lost after restart", status: "completed" as const, startedAt: 2, rounds: [], interactions: [] },
      { turnId: "t3", userText: "still newer", status: "running" as const, startedAt: 3, rounds: [], interactions: [] },
    ];
    const fallback = { ...createRawSessionState("seed"), turns: storeTurns };

    const merged = mergeTimelinePresentation(
      "seed",
      monitor.snapshotFor("seed")!,
      fallback,
      turnId => monitor.turnRevisionFor("seed", turnId),
    );
    expect(merged.turns.map(turn => turn.turnId)).toEqual(["t1", "t2", "t3"]);
    expect(merged.turns[0]!.rounds[0]!.answer).toBe("partial");
    expect(merged.session.totalTurns).toBe(3);
  });

  it("keeps timeline turns authoritative when both sources know the turn", () => {
    const monitor = createTimelineMonitor();
    const response = snapshot();
    response.snapshot.turns[0]!.turn_id = "t1";
    monitor.handleSnapshot(response); // timeline t1 has block data
    const fallback = {
      ...createRawSessionState("seed"),
      turns: [{ turnId: "t1", userText: "hello", status: "running" as const, startedAt: 1, rounds: [], interactions: [] }],
    };
    const merged = mergeTimelinePresentation(
      "seed",
      monitor.snapshotFor("seed")!,
      fallback,
      turnId => monitor.turnRevisionFor("seed", turnId),
    );
    expect(merged.turns).toHaveLength(1);
    // Timeline projection wins: richer block content, no store stub.
    expect(merged.turns[0]!.rounds[0]!.answer).toBe("partial");
  });
});
