import { describe, expect, it } from "vitest";
import { createTimelineMonitor } from "./timelineMonitor";
import { selectTimelinePresentation } from "./timelinePresentation";
import { createRawSessionState } from "./rawSession";
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
});
