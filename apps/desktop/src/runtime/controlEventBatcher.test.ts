import { describe, expect, it } from "vitest";
import { ControlEventBatcher } from "./controlEventBatcher";

function delta(seq: number, text: string) {
  return {
    type: "event",
    seq,
    seed: "seed",
    event: {
      type: "round_delta",
      turn_id: "turn",
      round_num: 1,
      kind: "answering",
      delta: text,
    },
  };
}

describe("ControlEventBatcher", () => {
  it("coalesces adjacent text revisions", () => {
    const batcher = new ControlEventBatcher();
    expect(batcher.push(delta(1, "hel"))).toEqual([]);
    expect(batcher.push(delta(2, "lo"))).toEqual([]);
    const [message] = batcher.flush();
    expect((message.event as { delta: string }).delta).toBe("hello");
    expect(message.seq).toBe(2);
  });

  it("keeps streams with different metadata separate", () => {
    const batcher = new ControlEventBatcher();
    batcher.push(delta(1, "answer"));
    batcher.push({
      ...delta(2, "thought"),
      event: { ...delta(2, "thought").event, kind: "thinking" },
    });
    expect(batcher.flush()).toHaveLength(2);
  });

  it("does not reorder interleaved stream kinds", () => {
    const batcher = new ControlEventBatcher();
    batcher.push(delta(1, "before"));
    batcher.push({
      type: "event",
      seq: 2,
      seed: "seed",
      event: { type: "tool_call_preview", turn_id: "turn", id: "call", args_so_far: "{}" },
    });
    batcher.push(delta(3, "after"));
    expect(batcher.flush().map(message => message.seq)).toEqual([1, 2, 3]);
  });

  it("flushes pending text before a reliable completion", () => {
    const batcher = new ControlEventBatcher();
    batcher.push(delta(1, "text"));
    const output = batcher.push({
      type: "event",
      seq: 2,
      seed: "seed",
      event: { type: "round_complete", turn_id: "turn" },
    });
    expect(output.map(message => (message.event as { type: string }).type))
      .toEqual(["round_delta", "round_complete"]);
  });

  it("replaces intermediate tool previews", () => {
    const batcher = new ControlEventBatcher();
    for (const [seq, args] of [[1, "{"], [2, "{\"path\""], [3, "{\"path\":\"x\"}"]] as const) {
      batcher.push({
        type: "event",
        seq,
        seed: "seed",
        event: { type: "tool_call_preview", turn_id: "turn", id: "call", args_so_far: args },
      });
    }
    const [message] = batcher.flush();
    expect((message.event as { args_so_far: string }).args_so_far).toBe("{\"path\":\"x\"}");
  });

  it("concatenates exec chunks instead of dropping command output", () => {
    const batcher = new ControlEventBatcher();
    for (const [seq, chunk] of [[1, "one"], [2, "two"], [3, "three"]] as const) {
      batcher.push({
        type: "event",
        seq,
        seed: "seed",
        event: { type: "exec_progress", tool_call_id: "call", stream: "stdout", seq, chunk },
      });
    }
    const [message] = batcher.flush();
    expect((message.event as { chunk: string }).chunk).toBe("onetwothree");
  });
});
