// Ringing SSE 客户端（Electron main 进程）。
//
// 职责（PLAN 约束）：
// - main 持有 daemon token 与三条 SSE 连接；renderer 不得直接持有 token。
// - 使用支持 header 的 fetch stream，token 只经 Authorization header。
// - main→renderer 发送**完整 batch**，禁止展开为逐事件 IPC。
// - SSE 断开只表示该频道退化；Conversation/Tool 断开不得显示 daemon 全局断联。
// - Control SSE 断开也不立即撤销 session lease（TTL + renew 维护）。
// - 每频道独立重连、cursor、snapshot 与健康状态。

import type {
  RingingEventBatch,
  RingingResetRequired,
} from "../src/lib/types/ringing";
import {
  cursorFromSseId,
  envelopeToBatch,
  parseResetRequired,
  parseSseFrame,
} from "../src/runtime/ringingSse";

/** 单频道 SSE 连接状态。 */
export type ChannelStatus =
  | { state: "connecting" }
  | { state: "open"; serverEpoch: string; cursor: number }
  | { state: "reconnecting"; retryMs: number; lastCursor: number }
  | { state: "closed"; reason: string };

/** 已由控制客户端完成的 v2 open 协商结果。仅保存在 Electron main 内存。 */
export interface RingingSessionOpen {
  clientInstanceId: string;
  clientSessionId: string;
  serverEpoch: string;
  leaseTtlMs: number;
  renewIntervalMs: number;
}

/** 事件批次回调（整 batch 交付，禁止逐事件展开）。 */
export type BatchHandler = (batch: RingingEventBatch) => void;
export type StatusHandler = (status: ChannelStatus) => void;

const RETRY_BASE_MS = 1000;
const RETRY_MAX_MS = 30000;
const SSE_IDLE_TIMEOUT_MS = 45_000;
const MAX_RENEW_FAILURES = 2;

/** 单频道 SSE 连接。 */
export class RingingChannelStream {
  private controller: AbortController | null = null;
  private retryMs = RETRY_BASE_MS;
  private closed = false;
  cursor = 0;

  constructor(
    private readonly url: string,
    private readonly token: string,
    private readonly channel: "control" | "conversation" | "tool",
    private readonly onBatch: BatchHandler,
    private readonly onStatus: StatusHandler,
    private readonly getServerEpoch: () => string,
    private readonly getClientSessionId: () => string,
    private readonly onReset?: (reset: RingingResetRequired) => void,
  ) {}

  start(): void {
    this.closed = false;
    void this.connectLoop();
  }

  close(): void {
    this.closed = true;
    this.controller?.abort();
  }

  private async connectLoop(): Promise<void> {
    while (!this.closed) {
      try {
        await this.connectOnce();
      } catch (err) {
        if (this.closed) return;
        const message = err instanceof Error ? err.message : String(err);
        this.onStatus({ state: "reconnecting", retryMs: this.retryMs, lastCursor: this.cursor });
        await sleep(this.retryMs);
        this.retryMs = Math.min(this.retryMs * 2, RETRY_MAX_MS);
        void message;
      }
    }
  }

  private async connectOnce(): Promise<void> {
    this.onStatus({ state: "connecting" });
    this.controller = new AbortController();
    const response = await fetch(this.url, {
      headers: {
        Authorization: `Bearer ${this.token}`,
        "X-DeepX-Client-Session-Id": this.getClientSessionId(),
        Accept: "text/event-stream",
        // Last-Event-ID 只回放该频道可靠 tail（epoch:channel:seq；epoch 不匹配
        // 服务端视为 0，因此必须带 open 协商出的 server_epoch）
        ...(this.cursor > 0 && this.getServerEpoch()
          ? { "Last-Event-ID": `${this.getServerEpoch()}:${this.channel}:${this.cursor}` }
          : {}),
      },
      signal: this.controller.signal,
    });
    if (!response.ok || !response.body) {
      throw new Error(`SSE ${this.channel} HTTP ${response.status}`);
    }
    this.onStatus({ state: "open", serverEpoch: this.getServerEpoch(), cursor: this.cursor });
    this.retryMs = RETRY_BASE_MS; // 连接成功：重置退避

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let idleTimer: ReturnType<typeof setTimeout> | null = null;
    const resetIdleTimer = () => {
      if (idleTimer) clearTimeout(idleTimer);
      idleTimer = setTimeout(() => this.controller?.abort(), SSE_IDLE_TIMEOUT_MS);
    };
    resetIdleTimer();

    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        resetIdleTimer(); // keepalive 也证明这条 TCP/SSE 路径仍然双端可达
        buffer += decoder.decode(value, { stream: true });
        // SSE 帧以空行分隔
        let sep: number;
        while ((sep = buffer.indexOf("\n\n")) >= 0) {
          const frame = buffer.slice(0, sep);
          buffer = buffer.slice(sep + 2);
          const parsed = parseSseFrame(frame);
          const frameCursor = parsed.id ? cursorFromSseId(parsed.id, this.channel) : null;
          if (!parsed.data.trim()) continue; // keepalive 注释帧
          if (parsed.eventType === "ringing.reset_required") {
            // cursor 超出保留窗口：客户端必须经 HTTP 读取权威 snapshot 后继续
            this.onReset?.(parseResetRequired(parsed.data.trim()));
          } else {
            // 服务端 `event:` 为 Ringing 事件类型（tool_started 等），全部按信封处理
            this.dispatch(parsed.data.trim(), frameCursor);
          }
        }
      }
    } finally {
      if (idleTimer) clearTimeout(idleTimer);
    }
    // 流结束：触发重连（不撤销 lease）
    if (!this.closed) throw new Error(`SSE ${this.channel} stream ended`);
  }

  private dispatch(payload: string, frameCursor: number | null): void {
    const batch = envelopeToBatch(this.channel, JSON.parse(payload));
    if (frameCursor !== null) {
      if (batch.server_epoch !== this.getServerEpoch()
        || batch.from_stream_seq !== frameCursor) {
        throw new Error("Ringing SSE cursor/envelope mismatch");
      }
    }
    // Only a fully parsed and accepted envelope advances Last-Event-ID.
    if (frameCursor !== null) this.cursor = frameCursor;
    this.onBatch(batch);
  }

  /** 强制 bootstrap 后按频道 baseline_stream_seq 重置本地 cursor。 */
  resetCursor(seq: number): void {
    this.cursor = seq;
  }
}

/** 客户端 open + lease renew。 */
export class RingingSession {
  clientSessionId: string | null = null;
  serverEpoch = "";
  leaseTtlMs = 0;
  /** open 请求携带的客户端实例 id（连接级 lease 校验字段）。 */
  readonly clientInstanceId: string;
  private renewTimer: ReturnType<typeof setInterval> | null = null;
  private renewFailures = 0;
  private closed = false;

  constructor(
    private readonly baseUrl: string,
    private readonly token: string,
    clientInstanceId = randomId(),
    private readonly onUnhealthy?: () => void,
  ) {
    this.clientInstanceId = clientInstanceId;
  }

  /** POST /ringing/v2/clients/open（能力协商）。 */
  async open(): Promise<void> {
    const response = await fetch(`${this.baseUrl}/ringing/v2/clients/open`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${this.token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        schema: "deepx.Ringing",
        version: 2,
        client_instance_id: this.clientInstanceId,
        capabilities: [
          "Ringing_v2",
          "Ringing_batch_v2",
          "Ringing_bootstrap_v2",
          "Ringing_command_status_v2",
        ],
      }),
    });
    if (!response.ok) throw new Error(`open failed: HTTP ${response.status}`);
    const result = (await response.json()) as {
      accepted: boolean;
      client_session_id: string;
      server_epoch: string;
      lease_ttl_ms: number;
      renew_interval_ms: number;
    };
    if (!result.accepted) throw new Error("Ringing not accepted by daemon");
    this.adoptOpen({
      clientInstanceId: this.clientInstanceId,
      clientSessionId: result.client_session_id,
      serverEpoch: result.server_epoch,
      leaseTtlMs: result.lease_ttl_ms,
      renewIntervalMs: result.renew_interval_ms,
    });
  }

  /** 复用控制客户端已经完成的 open，避免同一启动周期重复建 HTTP lease。 */
  adoptOpen(open: RingingSessionOpen): void {
    if (open.clientInstanceId !== this.clientInstanceId) {
      throw new Error("Ringing open client instance mismatch");
    }
    this.serverEpoch = open.serverEpoch;
    this.leaseTtlMs = open.leaseTtlMs;
    this.clientSessionId = open.clientSessionId;
    if (this.renewTimer) clearInterval(this.renewTimer);
    const renewInterval = Math.max(1000, Math.floor(open.renewIntervalMs / 2));
    this.renewTimer = setInterval(() => void this.renew(), renewInterval);
  }

  /** POST /ringing/v2/leases/renew。 */
  private async renew(): Promise<void> {
    if (this.closed) return;
    try {
      const response = await fetch(`${this.baseUrl}/ringing/v2/leases/renew`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.token}`,
          "X-DeepX-Client-Session-Id": this.clientSessionId ?? "",
        },
      });
      if (!response.ok) return this.recordRenewFailure(`HTTP ${response.status}`);
      this.renewFailures = 0;
    } catch (err) {
      this.recordRenewFailure(err instanceof Error ? err.message : String(err));
    }
  }

  private recordRenewFailure(reason: string): void {
    this.renewFailures += 1;
    console.warn(`[ringing] lease renew failed (${this.renewFailures}/${MAX_RENEW_FAILURES}): ${reason}`);
    if (this.renewFailures >= MAX_RENEW_FAILURES) this.onUnhealthy?.();
  }

  close(): void {
    this.closed = true;
    if (this.renewTimer) clearInterval(this.renewTimer);
  }
}

/** 三条独立 SSE 流（互不嵌套，各自独立重连）。 */
export class RingingClient {
  readonly streams: Record<"control" | "conversation" | "tool", RingingChannelStream>;
  readonly session: RingingSession;

  constructor(
    baseUrl: string,
    token: string,
    onBatch: BatchHandler,
    onStatus: (channel: "control" | "conversation" | "tool", status: ChannelStatus) => void,
    onReset?: (reset: RingingResetRequired) => void,
    private readonly openedSession?: RingingSessionOpen,
    onSessionUnhealthy?: () => void,
  ) {
    this.session = new RingingSession(
      baseUrl,
      token,
      openedSession?.clientInstanceId,
      onSessionUnhealthy,
    );
    this.streams = {
      control: new RingingChannelStream(
        `${baseUrl}/ringing/v2/events/control`, token, "control",
        (b) => onBatch(b), (s) => onStatus("control", s),
        () => this.session.serverEpoch, () => this.session.clientSessionId ?? "", onReset,
      ),
      conversation: new RingingChannelStream(
        `${baseUrl}/ringing/v2/events/conversation`, token, "conversation",
        (b) => onBatch(b), (s) => onStatus("conversation", s),
        () => this.session.serverEpoch, () => this.session.clientSessionId ?? "", onReset,
      ),
      tool: new RingingChannelStream(
        `${baseUrl}/ringing/v2/events/tool`, token, "tool",
        (b) => onBatch(b), (s) => onStatus("tool", s),
        () => this.session.serverEpoch, () => this.session.clientSessionId ?? "", onReset,
      ),
    };
  }

  async connect(): Promise<void> {
    if (this.openedSession) this.session.adoptOpen(this.openedSession);
    else await this.session.open();
    for (const key of ["control", "conversation", "tool"] as const) {
      this.streams[key].start();
    }
  }

  close(): void {
    this.session.close();
    for (const key of ["control", "conversation", "tool"] as const) {
      this.streams[key].close();
    }
  }
}

function randomId(): string {
  return `c-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
