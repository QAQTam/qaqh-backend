// Markdown final 渲染 Worker：接收 { id, raw, theme }，回传 { id, html | error }。
// 只做 marked + shiki（纯字符串管道）；katex（需要 DOM）由主线程收尾。

/// <reference lib="webworker" />

import {
  renderMarkdownInWorkerScope,
  type RenderTheme,
} from "../lib/markdown-render-core";

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

self.onmessage = (event: MessageEvent<RenderRequest>) => {
  const { id, raw, theme } = event.data;
  void renderMarkdownInWorkerScope(raw, theme)
    .then((html) => {
      const response: RenderResponse = { id, html };
      self.postMessage(response);
    })
    .catch((error) => {
      const response: RenderResponse = {
        id,
        error: error instanceof Error ? error.message : String(error),
      };
      self.postMessage(response);
    });
};
