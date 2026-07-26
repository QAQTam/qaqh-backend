import { describe, expect, it } from "vitest";
import { createRawSessionState } from "./sessionEventReducer";
import {
  activeInteraction,
  activeTurn,
  canLoadMore,
  failedPrompt,
  isSessionStreaming,
  sessionUsage,
} from "./sessionSelectors";

describe("sessionSelectors", () => {
  it("derives active state, gate, usage, retry text, and pagination", () => {
    const state = createRawSessionState("seed-a");
    state.turns.push({
      turnId: "t1", userText: "retry me", status: "failed", rounds: [], interactions: [],
    });
    state.turns.push({
      turnId: "t2", userText: "active", status: "waiting", rounds: [], interactions: [],
    });
    state.pendingInteractions.push({
      kind: "plan", id: "p1", turnId: "t2", content: "# Plan",
    });
    state.session.hasMore = true;
    state.session.usage = {
      prompt_tokens: 80, completion_tokens: 20, total_tokens: 100,
      prompt_cache_hit_tokens: 60, prompt_cache_miss_tokens: 20, reasoning_tokens: 5,
    };

    expect(activeTurn(state)?.turnId).toBe("t2");
    expect(isSessionStreaming(state)).toBe(true);
    expect(activeInteraction(state)?.id).toBe("p1");
    expect(sessionUsage(state)).toMatchObject({ contextTokens: 80, totalTokens: 100 });
    expect(failedPrompt(state)).toBe("retry me");
    expect(canLoadMore(state)).toBe(true);
  });
});
