import { describe, expect, it } from "vitest";
import {
  cursorSeqFromId,
  envelopeToBatch,
  parseResetRequired,
  parseSseFrame,
} from "./ringingSse";
import type { RingingEventEnvelope } from "../lib/types/ringing";

function envelope(streamSeq: number, eventId: string): RingingEventEnvelope {
  return {
    delivery: "reliable",
    seed: "s1",
    stream_seq: streamSeq,
    channel_seq: 1,
    session_seq: 1,
    event_id: eventId,
    state_revision: 3,
    event: {
      channel: "tool",
      type: "tool_started",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      name: "exec",
    } as unknown as RingingEventEnvelope["event"],
  };
}

describe("parseSseFrame", () => {
  it("parses id/event/data from a typed Ringing frame", () => {
    const parsed = parseSseFrame(
      "id: epoch-1:tool:7\nevent: tool_started\ndata: {\"seed\":\"s1\"}\n\n",
    );
    expect(parsed.id).toBe("epoch-1:tool:7");
    expect(parsed.eventType).toBe("tool_started");
    expect(parsed.data).toBe('{"seed":"s1"}');
  });

  it("ignores keepalive comment frames", () => {
    const parsed = parseSseFrame(": keepalive\n\n");
    expect(parsed.id).toBe("");
    expect(parsed.eventType).toBe("");
    expect(parsed.data).toBe("");
  });

  it("parses reset_required frames", () => {
    const parsed = parseSseFrame(
      'event: ringing.reset_required\ndata: {"channel":"tool","seed":"s1","earliest_available_seq":2}\n\n',
    );
    expect(parsed.eventType).toBe("ringing.reset_required");
    const reset = parseResetRequired(parsed.data);
    expect(reset.channel).toBe("tool");
    expect(reset.seed).toBe("s1");
    expect(Number(reset.earliest_available_seq)).toBe(2);
  });
});

describe("cursorSeqFromId", () => {
  it("extracts the last segment", () => {
    expect(cursorSeqFromId("epoch-1:tool:42")).toBe(42);
  });
  it("returns 0 for malformed ids", () => {
    expect(cursorSeqFromId("garbage")).toBe(0);
  });
});

describe("envelopeToBatch", () => {
  it("builds a whole-batch payload without expanding events", () => {
    const batch = envelopeToBatch("tool", envelope(5, "e5"), "epoch-1");
    expect(batch.channel).toBe("tool");
    expect(batch.seed).toBe("s1");
    expect(batch.from_stream_seq).toBe(5);
    expect(batch.to_stream_seq).toBe(5);
    expect(batch.envelopes).toHaveLength(1);
    expect(batch.envelopes[0].event).toMatchObject({ type: "tool_started", tool_call_id: "c1" });
  });

  it("rejects malformed envelope metadata at the transport boundary", () => {
    expect(() => envelopeToBatch("tool", { ...envelope(5, "e5"), seed: "" }, "epoch-1")).toThrow(
      "invalid Ringing stream sequence",
    );
    expect(() => envelopeToBatch("tool", {
      ...envelope(5, "e5"),
      event: { ...envelope(5, "e5").event, channel: "conversation" } as never,
    }, "epoch-1")).toThrow("invalid Ringing stream sequence");
    expect(() => envelopeToBatch("tool", {
      ...envelope(Number.MAX_SAFE_INTEGER + 1, "e5"),
    }, "epoch-1")).toThrow("invalid Ringing stream sequence");
  });
});
