/** 流式 Markdown 分块器(§7.5:已闭合块缓存、只重解析尾块)。 */
import { describe, expect, test } from "bun:test";
import { splitBlocks } from "../src/markdown/split";

describe("splitBlocks", () => {
  test("段落以空行分界;尾块未闭合", () => {
    const blocks = splitBlocks("para one\n\npara two and counting");
    expect(blocks).toHaveLength(2);
    expect(blocks[0]).toEqual({ content: "para one", complete: true });
    expect(blocks[1]).toEqual({ content: "para two and counting", complete: false });
  });

  test("围栏代码块整体闭合;未收口的围栏保持未闭合", () => {
    const closed = splitBlocks("intro\n\n```js\nconst a = 1;\n```\n\nafter");
    expect(closed.map((block) => block.complete)).toEqual([true, true, false]);
    expect(closed[1]!.content).toBe("```js\nconst a = 1;\n```");

    const open = splitBlocks("```py\nprint(1)");
    expect(open).toHaveLength(1);
    expect(open[0]!.complete).toBe(false);
  });

  test("标题行自闭合", () => {
    const blocks = splitBlocks("# Head\nbody text");
    expect(blocks[0]!.complete).toBe(true);
    expect(blocks[0]!.content).toBe("# Head");
  });

  test("空行后的列表行视为松散列表延续,不切分(保序号)", () => {
    const blocks = splitBlocks("1. one\n\n2. two\n\nplain tail");
    expect(blocks[0]!.content).toBe("1. one\n\n2. two");
    expect(blocks[0]!.complete).toBe(true);
    expect(blocks[1]!.content).toBe("plain tail");
  });

  test("增量追加时闭合块稳定(append-only)", () => {
    const first = splitBlocks("aaa\n\nbbb");
    const grown = splitBlocks("aaa\n\nbbb and more");
    expect(first[0]!.content).toBe(grown[0]!.content);
    expect(first[0]!.complete).toBe(grown[0]!.complete);
  });
});
