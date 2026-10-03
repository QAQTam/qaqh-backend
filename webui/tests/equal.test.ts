/** 结构相等深比较(spec §1.3 允许的纯逻辑单测)。 */
import { describe, expect, test } from "bun:test";
import { deepEqual } from "../src/lib/equal";

describe("deepEqual", () => {
  test("undefined 与键不存在等价(实时路径与快照路径的可选字段键集合本就不同)", () => {
    expect(deepEqual({ a: 1, startedAt: undefined }, { a: 1 })).toBe(true);
    expect(deepEqual({ a: 1 }, { a: 1, startedAt: 2 })).toBe(false);
    expect(deepEqual({ a: undefined }, { a: 1 })).toBe(false);
  });

  test("嵌套对象与数组按内容比较,顺序敏感", () => {
    expect(deepEqual({ steps: [{ id: "b1", out: { text: "x" } }] }, { steps: [{ id: "b1", out: { text: "x" } }] })).toBe(true);
    expect(deepEqual({ steps: [{ id: "b1" }] }, { steps: [{ id: "b2" }] })).toBe(false);
    expect(deepEqual([1, 2], [2, 1])).toBe(false);
    expect(deepEqual([1, 2], [1, 2, 3])).toBe(false);
  });

  test("容器种类不同即不等;null 与非 null 不等", () => {
    expect(deepEqual({ a: 1 }, [1])).toBe(false);
    expect(deepEqual(null, {})).toBe(false);
    expect(deepEqual(null, null)).toBe(true);
    expect(deepEqual("x", "x")).toBe(true);
  });
});
