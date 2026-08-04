// Markdown final 渲染的 Worker 客户端：单例 Worker + 请求/响应 + 崩溃/超时兜底。
// 无 Worker 环境（测试 / 降级）由调用方回退主线程。

import type { RenderTheme } from "./markdown-render-core";

interface RenderRequest {
  id: number;
  raw: string;
  theme: RenderTheme;
}

interface RenderResponse {
  id: number;
  html?: string;
  error?: string;
}

const REQUEST_TIMEOUT_MS = 60_000;

let worker: Worker | null = null;
let nextRequestId = 0;
const pending = new Map<number, {
  resolve: (html: string) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}>();

function getWorker(): Worker {
  if (!worker) {
    const created = new Worker(
      new URL("../workers/markdown.worker.ts", import.meta.url),
      { type: "module" },
    );
    created.onmessage = (event: MessageEvent<RenderResponse>) => {
      const entry = pending.get(event.data.id);
      if (!entry) return;
      pending.delete(event.data.id);
      clearTimeout(entry.timer);
      if (event.data.error !== undefined) {
        entry.reject(new Error(event.data.error));
      } else if (typeof event.data.html === "string") {
        entry.resolve(event.data.html);
      } else {
        entry.reject(new Error("markdown worker returned no html"));
      }
    };
    created.onerror = (event) => {
      // Worker 崩溃：拒绝全部在途请求（调用方回退主线程），随后重建。
      const error = new Error(event.message || "markdown worker crashed");
      for (const entry of pending.values()) {
        clearTimeout(entry.timer);
        entry.reject(error);
      }
      pending.clear();
      created.terminate();
      worker = null;
    };
    worker = created;
  }
  return worker;
}

/**
 * 在 Worker 中渲染 markdown（marked + shiki），返回未做 katex 的 HTML。
 * 调用方负责 renderMath 收尾与主线程回退。
 */
export function renderMarkdownInWorker(raw: string, theme: RenderTheme): Promise<string> {
  if (typeof Worker === "undefined") {
    return Promise.reject(new Error("Worker unavailable in this environment"));
  }
  const id = ++nextRequestId;
  return new Promise<string>((resolve, reject) => {
    const timer = setTimeout(() => {
      if (pending.delete(id)) {
        reject(new Error("markdown worker request timed out"));
      }
    }, REQUEST_TIMEOUT_MS);
    pending.set(id, {
      resolve,
      reject,
      timer,
    });
    try {
      getWorker().postMessage({ id, raw, theme } satisfies RenderRequest);
    } catch (error) {
      pending.delete(id);
      clearTimeout(timer);
      reject(error instanceof Error ? error : new Error(String(error)));
    }
  });
}
