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

    const current = monitor.snapshotFor("seed")!;
    expect(selectTimelinePresentation("seed", current, createRawSessionState("seed")).turns[0].rounds[0].answer)
      .toBe("partial text");
    expect(monitor.handleEntry("seed", {
      timeline_seq: 3,
      turn_id: "turn",
      round_num: 0,
      event: { type: "block_sealed", block_id: "answer" },
    })).toBe(true);
    expect(selectTimelinePresentation("seed", monitor.snapshotFor("seed")!, createRawSessionState("seed"))
      .turns[0].rounds[0].answer).toBe("partial text");
  });
});
