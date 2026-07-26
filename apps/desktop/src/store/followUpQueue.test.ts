// @vitest-environment jsdom
import { expect, it } from "vitest";
import { createFollowUpQueue } from "./followUpQueue";

it("waits for turn end without a pending gate", async () => {
  const sent: string[] = [];
  const queue = createFollowUpQueue("seed", async text => { sent.push(text); });
  queue.enqueue("next");
  await queue.drainAfterTurnEnd({ hasPendingGate: true });
  expect(sent).toEqual([]);
  await queue.drainAfterTurnEnd({ hasPendingGate: false });
  expect(sent).toEqual(["next"]);
});

it("does not persist executable work", () => {
  const writes: string[] = [];
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: {
    getItem: () => null,
    setItem: (key: string) => writes.push(key),
  }});
  const queue = createFollowUpQueue("seed", async () => {});
  queue.enqueue("dangerous follow-up");
  expect(localStorage.getItem("deepx:follow-ups:seed")).toBeNull();
  expect(writes).toEqual([]);
});
