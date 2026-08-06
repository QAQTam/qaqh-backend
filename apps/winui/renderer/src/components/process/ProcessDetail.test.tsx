// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { describe, expect, it } from "vitest";
import ProcessDetail from "./ProcessDetail";
import type { ProcessItem } from "../../presentation/processAggregation";

describe("ProcessDetail 增量文本渲染", () => {
  it("追加式更新 exec 输出时只追加文本节点，不整体替换", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [item, setItem] = createSignal<ProcessItem>({
      kind: "tool",
      id: "call-1",
      family: "exec",
      toolName: "exec",
      summary: "exec",
      progress: [{ stream: "stdout", seq: 0, chunk: "line1\n" }],
      status: undefined,
    });
    const dispose = render(() => <ProcessDetail item={item()} />, host);

    const pre = host.querySelector("pre")!;
    await Promise.resolve();
    expect(pre.textContent).toContain("line1");
    // 首次渲染：一个文本节点
    const nodesAfterFirst = pre.childNodes.length;

    // 流式追加：chunk2
    setItem({
      kind: "tool",
      id: "call-1",
      family: "exec",
      toolName: "exec",
      summary: "exec",
      progress: [
        { stream: "stdout", seq: 0, chunk: "line1\n" },
        { stream: "stdout", seq: 1, chunk: "line2\n" },
      ],
      status: undefined,
    });
    await Promise.resolve();
    expect(pre.textContent).toContain("line1\nline2");
    // 增量：只新增一个文本节点（旧节点保留）
    expect(pre.childNodes.length).toBe(nodesAfterFirst + 1);

    // 再追加：继续增量
    setItem({
      kind: "tool",
      id: "call-1",
      family: "exec",
      toolName: "exec",
      summary: "exec",
      progress: [
        { stream: "stdout", seq: 0, chunk: "line1\n" },
        { stream: "stdout", seq: 1, chunk: "line2\n" },
        { stream: "stdout", seq: 2, chunk: "line3\n" },
      ],
      status: undefined,
    });
    await Promise.resolve();
    expect(pre.textContent).toContain("line3");
    expect(pre.childNodes.length).toBe(nodesAfterFirst + 2);
    dispose();
    host.remove();
  });

  it("内容跳变（换块/回退）时整体替换", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [item, setItem] = createSignal<ProcessItem>({
      kind: "reasoning",
      id: "r1",
      content: "first reasoning",
      state: "open",
    });
    const dispose = render(() => <ProcessDetail item={item()} />, host);
    const pre = host.querySelector("pre")!;
    await Promise.resolve();
    expect(pre.textContent).toBe("first reasoning");

    // 跳变：内容不以旧内容为前缀（reasoning 块被替换）
    setItem({ kind: "reasoning", id: "r1", content: "completely different", state: "open" });
    await Promise.resolve();
    expect(pre.textContent).toBe("completely different");
    // 跳变后节点数回到 1（整体替换）
    expect(pre.childNodes.length).toBe(1);
    dispose();
    host.remove();
  });

  it("流式 exec 输出不做 JSON 重排（避免大 JSON 每 delta parse）", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [item, setItem] = createSignal<ProcessItem>({
      kind: "tool",
      id: "call-json",
      family: "exec",
      toolName: "exec",
      summary: "curl",
      progress: [{ stream: "stdout", seq: 0, chunk: '{"a": 1}' }],
      status: undefined,
    });
    const dispose = render(() => <ProcessDetail item={item()} />, host);
    const pre = host.querySelector("pre")!;
    await Promise.resolve();
    // 流式中保持原文（不格式化），且 data-format=text
    expect(pre.textContent).toBe('{"a": 1}');
    expect(pre.getAttribute("data-format")).toBe("text");
    dispose();
    host.remove();
  });
});
