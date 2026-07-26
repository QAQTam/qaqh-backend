import { expect, it, vi } from "vitest";
import type { Agent2Ui } from "../lib/types";
import { dispatchAgentEvent, type AgentEventEffects } from "./agentEventDispatcher";

function effects(): AgentEventEffects {
  return {
    onSessionCreated: vi.fn(), onSessionRestored: vi.fn(), onDashboard: vi.fn(),
    onError: vi.fn(), onCancelled: vi.fn(), onInteractionSettled: vi.fn(),
    onReducerError: vi.fn(),
  };
}

it("pushes every event before running its explicit side effect", () => {
  const target = { push: vi.fn() };
  const fx = effects();
  const created: Agent2Ui = { type: "session_created", seed: "seed-a" };
  dispatchAgentEvent(created, target, fx);
  expect(target.push).toHaveBeenCalledWith(created);
  expect(fx.onSessionCreated).toHaveBeenCalledWith("seed-a");

  const error: Agent2Ui = { type: "error", message: "lost" };
  dispatchAgentEvent(error, target, fx);
  expect(fx.onError).toHaveBeenCalledWith("lost");

  dispatchAgentEvent({ type: "ask_rejected", ask_id: "ask-1", message: "retry" }, target, fx);
  expect(fx.onInteractionSettled).toHaveBeenCalledWith("ask-1");
  expect(fx.onError).toHaveBeenCalledWith("retry");

  dispatchAgentEvent({ type: "ready" }, target, fx);
  expect(fx.onDashboard).not.toHaveBeenCalled();
});

it("isolates reducer failure and skips side effects for the failed event", () => {
  const failure = new Error("bad payload");
  const target = { push: vi.fn(() => { throw failure; }) };
  const fx = effects();
  const event: Agent2Ui = {
    type: "session_restored", seed: "seed-a", turns: [], tokens_used: 0,
    cache_hit_pct: 0, total_turns: 0, has_more: false,
  };
  dispatchAgentEvent(event, target, fx);
  expect(fx.onReducerError).toHaveBeenCalledWith(event, failure);
  expect(fx.onSessionRestored).not.toHaveBeenCalled();
});
