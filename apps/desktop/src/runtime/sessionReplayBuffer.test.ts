import { expect, it } from "vitest";
import type { Agent2Ui } from "../lib/types";
import { createSessionReplayBuffer } from "./sessionReplayBuffer";

it("applies typed replay before live events and removes one exact overlap", () => {
  const buffer = createSessionReplayBuffer();
  const applied: Agent2Ui["type"][] = [];
  const apply = (event: Agent2Ui) => applied.push(event.type);
  const overlap: Agent2Ui = { type: "done" };

  buffer.begin("seed-a");
  buffer.handleLive("seed-a", overlap, apply);
  buffer.handleLive("seed-a", { type: "cancelled" }, apply);
  expect(applied).toEqual([]);

  buffer.complete("seed-a", [
    { type: "ready" },
    overlap,
  ], apply);

  expect(applied).toEqual(["ready", "done", "cancelled"]);
});

it("drains buffered live events when replay is unavailable", () => {
  const buffer = createSessionReplayBuffer();
  const applied: string[] = [];
  buffer.begin("seed-a");
  buffer.handleLive("seed-a", { type: "done" }, event => {
    applied.push(event.type);
  });
  buffer.abort("seed-a", event => applied.push(event.type));
  expect(applied).toEqual(["done"]);
});

it("compares replay overlap containing bigint protocol fields", () => {
  const buffer = createSessionReplayBuffer();
  const applied: Agent2Ui[] = [];
  const progress: Agent2Ui = {
    type: "exec_progress",
    tool_call_id: "call-1",
    stream: "stdout",
    seq: 1n,
    chunk: "ok",
  };
  buffer.begin("seed-a");
  buffer.handleLive("seed-a", progress, event => applied.push(event));
  expect(() => buffer.complete("seed-a", [progress], event => applied.push(event))).not.toThrow();
  expect(applied).toEqual([progress]);
});
