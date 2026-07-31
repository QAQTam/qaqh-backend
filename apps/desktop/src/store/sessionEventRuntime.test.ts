import { describe, expect, it } from "vitest";
import type { RawSessionState } from "./rawSession";
import { createRawSessionState } from "./sessionEventReducer";
import {
  createSessionEventRuntime,
  loadReloadSnapshot,
  type ReloadStorage,
} from "./sessionEventRuntime";

class MemoryStorage implements ReloadStorage {
  private values = new Map<string, string>();
  writeCount = 0;
  getItem(key: string) { return this.values.get(key) ?? null; }
  setItem(key: string, value: string) { this.writeCount += 1; this.values.set(key, value); }
  removeItem(key: string) { this.values.delete(key); }
}

describe("sessionEventRuntime", () => {
  it("commits every event immediately and persists only low-frequency terminals", () => {
    const storage = new MemoryStorage();
    const commits: string[] = [];
    const runtime = createSessionEventRuntime({
      initialState: createRawSessionState("seed-a"),
      commit: state => commits.push(state.turns[0]?.rounds[0]?.answer ?? ""),
      storage,
      now: () => 100,
    });

    // Non-terminal events commit immediately but never persist
    runtime.push({ type: "turn_start", turn_id: "t1", user_text: "go" });
    expect(commits).toHaveLength(1);
    expect(storage.writeCount).toBe(0);

    // Streaming deltas also commit immediately (SolidJS batches internally)
    runtime.push({ type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: "A" });
    expect(commits).toHaveLength(2);
    expect(commits[1]).toBe("A");

    runtime.push({ type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: "B" });
    expect(commits).toHaveLength(3);
    expect(commits[2]).toBe("AB");
    // Streaming deltas do NOT persist
    expect(storage.writeCount).toBe(0);

    // Terminal events commit; persistence is throttled (now()=100 is inside
    // the initial throttle window), so nothing is written yet.
    runtime.push({ type: "turn_end", turn_id: "t1" });
    expect(runtime.current().turns[0].status).toBe("completed");
    expect(commits).toHaveLength(4);
    expect(storage.writeCount).toBe(0);
  });

  it("throttles snapshot persistence to low-frequency events with a 3s window", () => {
    const storage = new MemoryStorage();
    let t = 0;
    const runtime = createSessionEventRuntime({
      initialState: createRawSessionState("seed-a"),
      commit: () => {},
      storage,
      now: () => t,
    });

    // Round-level burst events never persist.
    runtime.push({ type: "turn_start", turn_id: "t1", user_text: "go" });
    runtime.push({ type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: "A" });
    runtime.push({
      type: "round_complete", turn_id: "t1", round_num: 0, answer: "A",
      tool_calls: [], blocks: [], is_final: true,
    });
    expect(storage.writeCount).toBe(0);

    // First qualifying terminal at t=5000 persists.
    t = 5000;
    runtime.push({ type: "turn_end", turn_id: "t1" });
    expect(storage.writeCount).toBe(1);

    // A second terminal inside the 3s window is skipped.
    t = 6000;
    runtime.push({ type: "done" });
    expect(storage.writeCount).toBe(1);

    // After the window elapses the next terminal persists again.
    t = 9000;
    runtime.push({ type: "done" });
    expect(storage.writeCount).toBe(2);
  });

  it("coalesces bursty text only until the next display frame", () => {
    const storage = new MemoryStorage();
    const commits: string[] = [];
    const frames: FrameRequestCallback[] = [];
    const runtime = createSessionEventRuntime({
      initialState: createRawSessionState("seed-a"),
      commit: state => commits.push(state.turns[0]?.rounds[0]?.answer ?? ""),
      storage,
      now: () => 100,
      scheduleFrame: callback => { frames.push(callback); return frames.length; },
      cancelFrame: () => {},
    });

    runtime.push({ type: "turn_start", turn_id: "t1", user_text: "go" });
    runtime.push({ type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: "A" });
    runtime.push({ type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: "B" });
    expect(commits).toHaveLength(1);
    expect(frames).toHaveLength(1);

    frames[0]!(116);
    expect(commits).toEqual(["", "AB"]);
  });

  it("keeps a 1000-delta burst complete while committing it once per frame", () => {
    const storage = new MemoryStorage();
    const commits: string[] = [];
    const frames: FrameRequestCallback[] = [];
    const runtime = createSessionEventRuntime({
      initialState: createRawSessionState("seed-a"),
      commit: state => commits.push(state.turns[0]?.rounds[0]?.answer ?? ""),
      storage,
      scheduleFrame: callback => { frames.push(callback); return frames.length; },
      cancelFrame: () => {},
    });

    runtime.push({ type: "turn_start", turn_id: "t1", user_text: "go" });
    for (let index = 0; index < 1000; index++) {
      runtime.push({ type: "round_delta", turn_id: "t1", round_num: 0, kind: "answering", delta: "x" });
    }

    expect(frames).toHaveLength(1);
    expect(commits).toEqual([""]);
    frames[0]!(116);
    expect(commits).toEqual(["", "x".repeat(1000)]);
  });

  it("restores the last twenty turns on dispose", () => {
    const storage = new MemoryStorage();
    const state = createRawSessionState("seed-a");
    state.turns = Array.from({ length: 25 }, (_, index) => ({
      turnId: `t${index}`,
      userText: `${index}`,
      status: "completed" as const,
      rounds: [],
      interactions: [],
    }));
    const runtime = createSessionEventRuntime({
      initialState: state,
      commit: () => {},
      storage,
    });

    // Trigger a low-frequency terminal event to persist the snapshot
    runtime.push({ type: "turn_start", turn_id: "t25", user_text: "go" });
    runtime.push({ type: "turn_end", turn_id: "t25" });
    const restored = loadReloadSnapshot(storage, "seed-a");
    expect(restored?.turns).toHaveLength(20);
    expect(restored?.turns[0].turnId).toBe("t6");
  });

  it("rejects corrupt or wrong-seed snapshots", () => {
    const storage = new MemoryStorage();
    storage.setItem("deepx:reload:v3:seed-a", "not-json");
    expect(loadReloadSnapshot(storage, "seed-a")).toBeUndefined();

    storage.setItem("deepx:reload:v3:seed-a", JSON.stringify({
      version: 3,
      state: { ...createRawSessionState("seed-b"), seed: "seed-b" },
    }));
    expect(loadReloadSnapshot(storage, "seed-a")).toBeUndefined();
  });

  it("removes legacy snapshots and commits when persistence throws", () => {
    const values = new Map<string, string>();
    values.set("deepx:reload:v1:seed-a", JSON.stringify({
      version: 1, state: createRawSessionState("seed-a"),
    }));
    values.set("deepx:reload:v2:seed-a", JSON.stringify({
      version: 2, state: createRawSessionState("seed-a"),
    }));
    values.set("deepx:reload:v3:seed-a", JSON.stringify({
      version: 3, state: createRawSessionState("seed-a"),
    }));
    const commits: RawSessionState[] = [];
    let writeAttempts = 0;
    const storage: ReloadStorage = {
      getItem: key => values.get(key) ?? null,
      setItem: () => { writeAttempts += 1; throw new Error("quota"); },
      removeItem: key => { values.delete(key); },
    };
    expect(loadReloadSnapshot(storage, "seed-a")).toBeUndefined();
    expect(values.has("deepx:reload:v1:seed-a")).toBe(false);
    expect(values.has("deepx:reload:v2:seed-a")).toBe(false);
    expect(values.has("deepx:reload:v3:seed-a")).toBe(false);

    const runtime = createSessionEventRuntime({
      initialState: createRawSessionState("seed-a"),
      commit: state => commits.push(state),
      storage,
      now: () => 5000,
    });
    // Low-frequency terminal event triggers persistence attempt (which throws, disabling further persistence)
    runtime.push({ type: "turn_start", turn_id: "t1", user_text: "go" });
    runtime.push({ type: "turn_end", turn_id: "t1" });
    expect(commits).toHaveLength(2);
    expect(writeAttempts).toBe(1);

    // Second terminal event should not attempt persistence (disabled)
    runtime.push({ type: "session_created", seed: "seed-a" });
    expect(commits).toHaveLength(3);
    expect(writeAttempts).toBe(1);
  });
});
