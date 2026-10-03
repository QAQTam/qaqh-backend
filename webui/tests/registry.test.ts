/** 工具注册表(spec §8.1):折叠行摘要的 header 优先级与入参回退。 */
import { describe, expect, test } from "bun:test";
import { primaryArgFull } from "../src/tools/registry";

describe("primaryArgFull:空 Other label 必须回退到入参", () => {
  test("MCP 的 header 只有空 label → 用 args 里的 url/query", () => {
    // 后端 qaqh-mcp/bridge.rs 刻意发 `Other{ label: "" }`(工具名 mcp__server__tool
    // 已是真相,header 只用来抑制 legacy 摘要)。早返回空串会让摘要整行空白。
    const display = { header: { kind: "other", label: "" } };
    expect(primaryArgFull('{"url":"https://example.a"}', display)).toBe("https://example.a");
    expect(primaryArgFull('{"query":"cats"}', display)).toBe("cats");
  });

  test("label 缺席或非字符串同样回退", () => {
    expect(primaryArgFull('{"path":"/tmp/a"}', { header: { kind: "other" } })).toBe("/tmp/a");
  });

  test("非空 Other label 仍优先于入参", () => {
    const display = { header: { kind: "other", label: "子代理" } };
    expect(primaryArgFull('{"path":"/ignored"}', display)).toBe("子代理");
  });

  test("shell/path header 不受影响", () => {
    expect(primaryArgFull('{"command":"ls"}', { header: { kind: "shell", command: "cargo test" } })).toBe("cargo test");
    expect(primaryArgFull('{"file":"x"}', { header: { kind: "path", path: "/y", op: "read" } })).toBe("/y");
  });
});
