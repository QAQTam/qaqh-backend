import { describe, expect, it } from "vitest";
import {
  extractToolResultText,
  toolArgsSummary,
  toolStatusLabel,
  toolVerb,
} from "./toolSemantics";

describe("toolArgsSummary（label 语义化）", () => {
  it("read 提取路径与行范围", () => {
    expect(toolArgsSummary("read", '{"path":"src/main.rs","start_line":10,"end_line":20}'))
      .toBe("src/main.rs L10-20");
    expect(toolArgsSummary("read", '{"path":"src/main.rs","line":5}')).toBe("src/main.rs L5");
    expect(toolArgsSummary("read", '{"path":"src/main.rs"}')).toBe("src/main.rs");
  });

  it("exec 提取命令", () => {
    expect(toolArgsSummary("exec", '{"command":"npm test"}')).toBe("npm test");
    expect(toolArgsSummary("exec", '{"argv":["cargo","build"]}')).toBe("cargo build");
  });

  it("web/search 提取查询词，write/edit 提取路径", () => {
    expect(toolArgsSummary("web_search", '{"query":"solidjs signals"}')).toBe("solidjs signals");
    expect(toolArgsSummary("search", '{"pattern":"TODO","path":"src"}')).toBe("TODO");
    expect(toolArgsSummary("edit", '{"path":"src/a.ts"}')).toBe("src/a.ts");
    expect(toolArgsSummary("write", '{"file":"README.md"}')).toBe("README.md");
  });

  it("无有效参数时回退到第一个字符串参数或空串", () => {
    expect(toolArgsSummary("read", "{}")).toBe("");
    expect(toolArgsSummary("exec", "")).toBe("");
    expect(toolArgsSummary("unknown_tool", '{"foo":"bar"}')).toBe("foo=bar");
  });

  it("超长命令截断", () => {
    const long = "x".repeat(200);
    expect(toolArgsSummary("exec", JSON.stringify({ command: long })).length).toBeLessThan(80);
  });
});

describe("toolStatusLabel（状态词）", () => {
  it("ok → 已读取/已执行", () => {
    expect(toolStatusLabel("ok", "read", "src/main.rs")).toBe("已读取 src/main.rs");
    expect(toolStatusLabel("backgrounded", "exec", "npm test")).toBe("已执行 npm test");
  });
  it("error → 修改失败", () => {
    expect(toolStatusLabel("error", "edit", "src/a.ts")).toBe("修改失败 src/a.ts");
  });
  it("进行中 → 读取中/执行中", () => {
    expect(toolStatusLabel(undefined, "read", "src/main.rs")).toBe("读取中 src/main.rs");
    expect(toolStatusLabel("running", "exec", "npm test")).toBe("执行中 npm test");
  });
  it("无摘要时只显示动词", () => {
    expect(toolStatusLabel("ok", "read", "")).toBe("已读取");
    expect(toolVerb("delete")).toBe("删除");
  });
});

describe("extractToolResultText（结果 JSON 过滤/提取）", () => {
  it("非 JSON（diff/普通文本）原样返回", () => {
    const diff = "--- a/src/a.ts\n+++ b/src/a.ts\n@@ -8 +8 @@\n-old\n+new";
    expect(extractToolResultText(diff)).toBe(diff);
  });

  it("exec 结果：丢弃 output 大字段，保留 exit_code 等关键信息", () => {
    const execResult = JSON.stringify({
      status: "ok",
      command: "npm test",
      exit_code: 0,
      output: "very long output ".repeat(1000),
      wall_time_seconds: 3.2,
      truncated: false,
      timed_out: false,
      cancelled: false,
      stdout_bytes: 12000,
      stderr_bytes: 0,
      ui_dropped_bytes: 0,
      process_id: 42,
    });
    const extracted = extractToolResultText(execResult);
    expect(extracted).toContain('"exit_code": 0');
    expect(extracted).toContain('"command": "npm test"');
    expect(extracted).toContain('"wall_time_seconds"');
    expect(extracted).not.toContain("very long output");
  });

  it("read 结果：保留 path/统计，丢弃 content 内容", () => {
    const readResult = JSON.stringify({
      path: "src/main.rs",
      line_count: 120,
      content: "line1\nline2\n".repeat(500),
      hash: "abc123",
    });
    const extracted = extractToolResultText(readResult);
    expect(extracted).toContain('"path": "src/main.rs"');
    expect(extracted).toContain('"line_count": 120');
    expect(extracted).not.toContain("line1");
  });

  it("全部字段都是噪音时回退到 summary/message 或体积摘要", () => {
    expect(extractToolResultText(JSON.stringify({ output: "big", stdout: "bigger" })))
      .toContain("fields");
    expect(extractToolResultText(JSON.stringify({ output: "big", message: "not found" })))
      .toBe("not found");
  });

  it("数组结果：短数组完整显示，长数组给条目数 + 样例", () => {
    expect(extractToolResultText('[{"path":"a"},{"path":"b"}]')).toContain('"path": "a"');
    const long = JSON.stringify(Array.from({ length: 10 }, (_, i) => ({ path: `f${i}` })));
    const extracted = extractToolResultText(long);
    expect(extracted).toContain("10 items");
  });

  it("嵌套小对象保留，错误对象保留 message", () => {
    const result = JSON.stringify({
      status: "error",
      error: { code: "NOT_FOUND", message: "file missing", retryable: false },
      stdout: "noise",
    });
    const extracted = extractToolResultText(result);
    expect(extracted).toContain("NOT_FOUND");
    expect(extracted).not.toContain("noise");
  });
});
