import { describe, expect, it } from "vitest";
import { aggregateProcessItems, type ProcessItem } from "./processAggregation";

const tool = (id: string, family: string, status: "ok" | "error"): ProcessItem => ({
  kind: "tool", id, family, toolName: family, summary: id, status,
});

describe("process aggregation", () => {
  it("groups consecutive successful operations and leaves failures separate", () => {
    const items = aggregateProcessItems([
      tool("read-1", "read", "ok"),
      tool("read-2", "read", "ok"),
      tool("build", "exec", "error"),
    ]);
    expect(items[0]).toMatchObject({ kind: "group", family: "read" });
    expect(items[1]).toMatchObject({ kind: "tool", id: "build", status: "error" });
  });
});
