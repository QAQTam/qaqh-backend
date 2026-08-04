// Browser 调试桥：daemon `/debug/` 页在纯浏览器运行时的适配层。
//
// 背景：Electron 的 preload 只存在于 Electron 环境。浏览器直接打开
// `http://127.0.0.1:<port>/debug/` 时 `window.deepx` 不存在，需要注入
// 浏览器实现。daemon 的 Ringing 端点全部走 HTTP/SSE（fetch 可带
// Authorization header），因此 Ringing 通道在浏览器完全可用；
// legacy WS 需要自定义 header（浏览器 WebSocket 不支持），且 RPC 请求
// 映射工作量大——debug 模式定位为**只读观察**：SSE 三频道 + snapshot +
// 面板展示，backend.request/desktop 降级为 no-op。
//
// 注入点：main.tsx 最先调用 installBrowserBridge()；仅当
// `window.__DEEPX_DEBUG__`（daemon 内联 token）存在时生效。

import type {
  RingingEventBatch,
  RingingEventEnvelope,
  RingingResetRequired,
} from "../lib/types/ringing";
import {
  cursorFromSseId,
  envelopeToBatch,
  parseResetRequired,
  parseSseFrame,
} from "./ringingSse";

declare global {
  interface Window {
    __DEEPX_DEBUG__?: { token: string; nonce: string };
  }
}

type ChannelName = "control" | "conversation" | "tool";

// ── 模块级单例状态（SSE 三频道只启动一次） ──────────────────────────────
let sseStarted = false;
let sseBase = "";
let sseToken = "";
let sseSessionId = "";
let sseServerEpoch = "";
const batchListeners = new Set<(batch: RingingEventBatch) => void>();
const statusListeners = new Set<(u: { channel: string; status: unknown }) => void>();
const snapshotListeners = new Set<
  (u: { seed: string; channel: string; snapshot: unknown }) => void
>();

function emitStatus(channel: string, status: unknown): void {
  for (const listener of statusListeners) listener({ channel, status });
}

async function connectSse(ch: ChannelName): Promise<void> {
  let cursor = 0;
  for (;;) {
    try {
      emitStatus(ch, { state: "connecting" });
      const response = await fetch(`${sseBase}/ringing/v1/events/${ch}`, {
        headers: {
          Authorization: `Bearer ${sseToken}`,
          "X-DeepX-Client-Session-Id": sseSessionId,
          Accept: "text/event-stream",
        },
      });
      if (!response.ok || !response.body) {
        throw new Error(`HTTP ${response.status}`);
      }
      emitStatus(ch, { state: "open", cursor: 0 });
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let sep: number;
        while ((sep = buffer.indexOf("\n\n")) >= 0) {
          const frame = buffer.slice(0, sep);
          buffer = buffer.slice(sep + 2);
          const parsed = parseSseFrame(frame);
          const frameCursor = parsed.id ? cursorFromSseId(parsed.id, ch) : null;
          if (handleFrame(ch, parsed.eventType, parsed.data, frameCursor)) {
            if (frameCursor !== null) cursor = frameCursor;
          }
        }
      }
      emitStatus(ch, { state: "reconnecting", reason: "stream ended", lastCursor: cursor });
      throw new Error("stream ended");
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      emitStatus(ch, { state: "reconnecting", reason: message });
      await new Promise((r) => setTimeout(r, 2000));
    }
  }
}

function handleFrame(
  ch: ChannelName,
  eventType: string,
  data: string,
  frameCursor: number | null,
): boolean {
  if (!data.trim()) return false; // keepalive 注释帧
  if (eventType === "ringing.reset_required") {
    void handleReset(parseResetRequired(data.trim()));
    return false;
  }
  try {
    const envelope = JSON.parse(data) as RingingEventEnvelope;
    const batch = envelopeToBatch(ch, envelope, sseServerEpoch);
    if (frameCursor !== null
      && (batch.server_epoch !== sseServerEpoch || batch.from_stream_seq !== frameCursor)) {
      return false;
    }
    for (const listener of batchListeners) listener(batch);
    return true;
  } catch {
    // 忽略畸形帧
    return false;
  }
}

/** cursor 超出保留窗口：经 HTTP 拉取权威 snapshot 并通知 renderer。 */
async function handleReset(reset: RingingResetRequired): Promise<void> {
  try {
    const response = await fetch(
      `${sseBase}/ringing/v1/sessions/${encodeURIComponent(reset.seed)}/bootstrap`,
      { headers: { Authorization: `Bearer ${sseToken}`, "X-DeepX-Client-Session-Id": sseSessionId } },
    );
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const snapshot = await response.json();
    for (const listener of snapshotListeners) {
      listener({ seed: reset.seed, channel: reset.channel, snapshot });
    }
  } catch (err) {
    console.warn("[ringing][browser] reset snapshot failed", err);
  }
}

async function ensureSse(base: string, token: string): Promise<void> {
  if (sseStarted) return;
  sseStarted = true;
  sseBase = base;
  sseToken = token;
  const response = await fetch(`${base}/ringing/v1/clients/open`, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
    body: JSON.stringify({
      schema: "deepx.Ringing",
      version: 1,
      client_instance_id: `browser-${crypto.randomUUID()}`,
      capabilities: ["Ringing_v1", "Ringing_batch_v1", "Ringing_bootstrap_v1", "Ringing_command_status_v1"],
    }),
  });
  if (!response.ok) throw new Error(`Ringing open failed: HTTP ${response.status}`);
  const opened = await response.json() as { client_session_id?: string; server_epoch?: string };
  if (!opened.client_session_id) throw new Error("Ringing open did not return a client session");
  sseSessionId = opened.client_session_id;
  sseServerEpoch = opened.server_epoch ?? "";
  for (const ch of ["control", "conversation", "tool"] as ChannelName[]) {
    void connectSse(ch);
  }
}

/** 安装浏览器桥。返回是否生效（true = 处于 debug 只读模式）。 */
export function installBrowserBridge(): boolean {
  if (window.deepx || !window.__DEEPX_DEBUG__) return false;
  const { token } = window.__DEEPX_DEBUG__;
  const base = window.location.origin;

  const http = async (path: string): Promise<unknown> => {
    const response = await fetch(`${base}${path}`, {
      headers: {
        Authorization: `Bearer ${token}`,
        ...(sseSessionId ? { "X-DeepX-Client-Session-Id": sseSessionId } : {}),
      },
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
  };

  const noop = (): void => undefined;
  const rejectReadOnly = (): Promise<never> =>
    Promise.reject(new Error("browser debug 模式只读：操作请使用桌面应用"));

  const bridge = {
    backend: {
      connect: noop as () => Promise<void>,
      request: ((method: string) => {
        // 只读观察：会话列表/活动查询返回空（避免控制台红字）；
        // 其余 RPC 明确拒绝。
        if (method === "session.list") return Promise.resolve([]);
        if (method === "session.activity") return Promise.resolve([]);
        if (method === "session.meta") return Promise.resolve({});
        return rejectReadOnly();
      }) as (m: string, p: Record<string, unknown>) => Promise<unknown>,
      attach: noop,
      detach: noop,
      status: () => Promise.resolve({ connected: true }),
      onMessage: () => () => undefined,
      onStatus: () => () => undefined,
    },
    ringing: {
      status: () => Promise.resolve({}),
      snapshot: async (seed: string, channel: string) => {
        const bootstrap = await fetch(
          `${base}/ringing/v1/sessions/${encodeURIComponent(seed)}/bootstrap`,
          { headers: { Authorization: `Bearer ${token}`, "X-DeepX-Client-Session-Id": sseSessionId } },
        );
        if (!bootstrap.ok) throw new Error(`HTTP ${bootstrap.status}`);
        const payload = await bootstrap.json() as Record<string, unknown>;
        return payload[channel];
      },
      command: rejectReadOnly,
      query: (path: string, params?: Record<string, string | undefined>) =>
        http(`/ringing/v1/queries/${path}${params ? `?${new URLSearchParams(params as Record<string, string>).toString()}` : ""}`),
      onBatch: (listener: (batch: RingingEventBatch) => void) => {
        batchListeners.add(listener);
        void ensureSse(base, token).catch((error) => console.warn("[browser-bridge] Ringing open failed", error));
        return () => { batchListeners.delete(listener); };
      },
      onStatus: (listener: (u: { channel: string; status: unknown }) => void) => {
        statusListeners.add(listener);
        void ensureSse(base, token).catch((error) => console.warn("[browser-bridge] Ringing open failed", error));
        return () => { statusListeners.delete(listener); };
      },
      onSnapshot: (listener: (u: { seed: string; channel: string; snapshot: unknown }) => void) => {
        snapshotListeners.add(listener);
        return () => { snapshotListeners.delete(listener); };
      },
    },
    desktop: {
      openDialog: rejectReadOnly,
      confirm: rejectReadOnly,
      openPath: noop,
      togglePet: noop as () => Promise<boolean>,
      getPetStatus: noop as () => Promise<boolean>,
      checkUpdate: () => Promise.resolve(null),
      stageUpdate: rejectReadOnly,
      applyUpdate: rejectReadOnly,
      openDevTools: noop,
      setBackgroundMaterial: noop,
      onUpdateAvailable: () => () => undefined,
      onUpdateFailed: () => () => undefined,
      openImageDialog: rejectReadOnly,
      readFileBase64: rejectReadOnly,
      readTextFile: rejectReadOnly,
    },
  };

  (window as unknown as { deepx: typeof bridge }).deepx = bridge;
  console.info("[browser-bridge] debug 只读模式已启用（Ringing SSE 观察）");
  return true;
}
