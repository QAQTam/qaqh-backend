// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import type { renderMarkdownInWorker as RenderMarkdownInWorker } from "./markdownWorkerClient";

interface PostedMessage {
  id: number;
  raw: string;
  theme: string;
}

interface FakeResponse {
  data: { id: number; html?: string; error?: string };
}

class FakeWorker {
  static instances: FakeWorker[] = [];
  onmessage: ((event: FakeResponse) => void) | null = null;
  onerror: ((event: { message?: string }) => void) | null = null;
  posted: PostedMessage[] = [];
  terminated = false;

  constructor() {
    FakeWorker.instances.push(this);
  }

  postMessage(message: PostedMessage): void {
    this.posted.push(message);
  }

  terminate(): void {
    this.terminated = true;
  }
}

// 客户端持有模块级 worker 单例：每个测试用 resetModules 重新加载，
// 避免单例跨测试泄漏（复用旧实例导致实例数组断言失准）。
async function loadClient(): Promise<{ renderMarkdownInWorker: typeof RenderMarkdownInWorker }> {
  vi.resetModules();
  return await import("./markdownWorkerClient");
}

function lastPosted(worker: FakeWorker): PostedMessage {
  const message = worker.posted[worker.posted.length - 1];
  if (!message) throw new Error("no message posted");
  return message;
}

function respond(worker: FakeWorker, response: { html?: string; error?: string }): void {
  worker.onmessage?.({ data: { id: lastPosted(worker).id, ...response } });
}

describe("markdownWorkerClient", () => {
  afterEach(() => {
    FakeWorker.instances = [];
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("resolves with the worker-rendered html", async () => {
    vi.stubGlobal("Worker", FakeWorker);
    const { renderMarkdownInWorker } = await loadClient();
    const promise = renderMarkdownInWorker("# hi", "github-light");
    const worker = FakeWorker.instances[0];
    expect(worker.posted[0].raw).toBe("# hi");
    expect(worker.posted[0].theme).toBe("github-light");
    respond(worker, { html: "<h1>hi</h1>" });
    await expect(promise).resolves.toBe("<h1>hi</h1>");
  });

  it("rejects on worker-reported error and keeps the worker for the next request", async () => {
    vi.stubGlobal("Worker", FakeWorker);
    const { renderMarkdownInWorker } = await loadClient();
    const p1 = renderMarkdownInWorker("a", "github-light");
    const worker = FakeWorker.instances[0];
    respond(worker, { error: "parse boom" });
    await expect(p1).rejects.toThrow("parse boom");

    const p2 = renderMarkdownInWorker("b", "github-dark");
    expect(FakeWorker.instances.length).toBe(1); // 复用，不重建
    respond(worker, { html: "<p>b</p>" });
    await expect(p2).resolves.toBe("<p>b</p>");
  });

  it("rejects all in-flight requests on worker crash and recreates the worker", async () => {
    vi.stubGlobal("Worker", FakeWorker);
    const { renderMarkdownInWorker } = await loadClient();
    const p1 = renderMarkdownInWorker("a", "github-light");
    const p2 = renderMarkdownInWorker("b", "github-light");
    const worker = FakeWorker.instances[0];
    worker.onerror?.({ message: "crashed" });
    await expect(p1).rejects.toThrow("crashed");
    await expect(p2).rejects.toThrow("crashed");

    // 崩溃后重建
    const p3 = renderMarkdownInWorker("c", "github-light");
    expect(FakeWorker.instances.length).toBe(2);
    const replacement = FakeWorker.instances[1];
    respond(replacement, { html: "<p>c</p>" });
    await expect(p3).resolves.toBe("<p>c</p>");
  });

  it("rejects immediately when Worker is unavailable", async () => {
    // jsdom 默认不提供 Worker（未 stub）→ 客户端必须拒绝，调用方回退主线程。
    const { renderMarkdownInWorker } = await loadClient();
    await expect(renderMarkdownInWorker("x", "github-light")).rejects.toThrow(/unavailable/i);
  });

  it("rejects on request timeout", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("Worker", FakeWorker);
    const { renderMarkdownInWorker } = await loadClient();
    const promise = renderMarkdownInWorker("x", "github-light");
    vi.advanceTimersByTime(60_001);
    await expect(promise).rejects.toThrow(/timed out/i);
  });
});
