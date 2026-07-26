import { describe, expect, it } from "vitest";
import { aggregateProcessItems, type ProcessItem } from "./processAggregation";

const tool = (id: string, family: string, success: boolean): ProcessItem => ({
  kind: "tool", id, family, toolName: family, summary: id, success,
});

describe("process aggregation", () => {
  it("groups consecutive successful operations and leaves failures separate", () => {
    const items = aggregateProcessItems([
      tool("read-1", "read", true),
      tool("read-2", "read", true),
      tool("build", "exec", false),
    ]);
    expect(items[0]).toMatchObject({ kind: "group", family: "read" });
    expect(items[1]).toMatchObject({ kind: "tool", id: "build", success: false });
  });
});
