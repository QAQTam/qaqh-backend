import { describe, expect, it } from "vitest";
import {
  mergeSessionActivitySnapshot,
  parseSessionActivity,
  upsertSessionActivity,
  type SessionActivityMap,
} from "./sessionActivityStore";

const activity = (seed: string, state: "idle" | "working", seq: number) => ({
  seed,
  state,
  seq,
  updated_at: seq,
});

describe("session activity store", () => {
  it("keeps a newer live event when an older refresh snapshot arrives", () => {
    let current: SessionActivityMap = {};
    current = upsertSessionActivity(current, activity("a", "working", 3));
    current = mergeSessionActivitySnapshot(current, [activity("a", "idle", 2)]);

    expect(current.a.state).toBe("working");
    expect(current.a.seq).toBe(3);
  });

  it("merges activity for sessions that are not currently open", () => {
    const current = mergeSessionActivitySnapshot({}, [
      activity("a", "working", 1),
      activity("b", "idle", 1),
    ]);

    expect(Object.keys(current)).toEqual(["a", "b"]);
  });

  it("rejects malformed or future activity states at the bridge boundary", () => {
    expect(parseSessionActivity(activity("a", "working", 1)).state).toBe("working");
    expect(() => parseSessionActivity({ seed: "a", state: "future", seq: 1, updated_at: 1 }))
      .toThrow("invalid session activity state");
  });
});
