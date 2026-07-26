import { expect, it } from "vitest";
import { parseAgentEvent } from "./agentEventBoundary";

it("accepts a generated event and rejects malformed or unknown discriminants", () => {
  expect(parseAgentEvent({ type: "ready" })).toEqual({ type: "ready" });
  expect(() => parseAgentEvent(null)).toThrow("agent event must be an object");
  expect(() => parseAgentEvent({ type: "future_event" })).toThrow("unknown Agent2Ui event type");
});
