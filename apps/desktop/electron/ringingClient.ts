// Ringing SSE 客户端（Electron main 进程）。
//
// 职责（PLAN 约束）：
// - main 持有 daemon token 与三条 SSE 连接；renderer 不得直接持有 token。
// - 使用支持 header 的 fetch stream，token 只经 Authorization header。
// - main→renderer 发送**完整 batch**，禁止展开为逐事件 IPC。
// - SSE 断开只表示该频道退化；Conversation/Tool 断开不得显示 daemon 全局断联。
// - Control SSE 断开也不立即撤销 session lease（TTL + renew 维护）。
// - 每频道独立重连、cursor、snapshot 与健康状态。

import type { RingingEventBatch, RingingEventEnvelope } from "../src/lib/types/ringing";

/** 单频道 SSE 连接状态。 */
export type ChannelStatus =
  | { state: "connecting" }
  | { state: "open"; serverEpoch: string; cursor: number }
  | { state: "reconnecting"; retryMs: number; lastCursor: number }
  | { state: "closed"; reason: string };

/** 事件批次回调（整 batch 交付，禁止逐事件展开）。 */
export type BatchHandler = (batch: RingingEventBatch) => void;
export type StatusHandler = (status: ChannelStatus) => void;

const RETRY_BASE_MS = 1000;
const RETRY_MAX_MS = 30000;

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
        Accept: "text/event-stream",
        // Last-Event-ID 只回放该频道可靠 tail（epoch:channel:seq）
        ...(this.cursor > 0 ? { "Last-Event-ID": `:${this.channel}:${this.cursor}` } : {}),
      },
      signal: this.controller.signal,
    });
    if (!response.ok || !response.body) {
      throw new Error(`SSE ${this.channel} HTTP ${response.status}`);
    }
    this.onStatus({ state: "open", serverEpoch: "", cursor: this.cursor });
    this.retryMs = RETRY_BASE_MS; // 连接成功：重置退避

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let eventId = "";
    let eventType = "";
    let data = "";

    const flush = () => {
      if (eventType === "message" && data.trim()) {
        this.dispatch(data.trim());
      }
      eventId = "";
      eventType = "";
      data = "";
    };

    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      // SSE 帧以空行分隔
      let sep: number;
      while ((sep = buffer.indexOf("\n\n")) >= 0) {
        const frame = buffer.slice(0, sep);
        buffer = buffer.slice(sep + 2);
        for (const line of frame.split("\n")) {
          if (line.startsWith(":")) continue; // 注释（keepalive）
          if (line.startsWith("id:")) {
            eventId = line.slice(3).trim();
            this.cursor = parseCursorSeq(eventId);
          } else if (line.startsWith("event:")) {
            eventType = line.slice(6).trim();
          } else if (line.startsWith("data:")) {
            data += line.slice(5).trim();
          }
        }
        flush();
      }
    }
    // 流结束：触发重连（不撤销 lease）
    if (!this.closed) throw new Error(`SSE ${this.channel} stream ended`);
  }

  private dispatch(payload: string): void {
    const envelope = JSON.parse(payload) as RingingEventEnvelope;
    const batch: RingingEventBatch = {
      channel: this.channel,
      seed: envelope.seed,
      from_stream_seq: envelope.stream_seq,
      to_stream_seq: envelope.stream_seq,
      state_revision: envelope.state_revision ?? null,
      events: [envelope.event],
    };
    this.onBatch(batch);
  }
}

/** 从 `id: epoch:channel:seq` 提取 seq。 */
function parseCursorSeq(eventId: string): number {
  const parts = eventId.split(":");
  const seq = Number(parts[parts.length - 1]);
  return Number.isFinite(seq) ? seq : 0;
}

/** 客户端 open + lease renew。 */
export class RingingSession {
  clientSessionId: string | null = null;
  serverEpoch = "";
  leaseTtlMs = 0;
  /** open 请求携带的客户端实例 id（cutover/命令端点 lease 校验字段）。 */
  readonly clientInstanceId = randomId();
  private renewTimer: ReturnType<typeof setInterval> | null = null;
  private closed = false;

  constructor(
    private readonly baseUrl: string,
    private readonly token: string,
  ) {}

  /** POST /ringing/v1/clients/open（能力协商）。 */
  async open(): Promise<void> {
    const response = await fetch(`${this.baseUrl}/ringing/v1/clients/open`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${this.token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        schema: "deepx.Ringing",
        version: 1,
        client_instance_id: this.clientInstanceId,
        capabilities: ["Ringing_v1", "Ringing_session_cutover_v1", "Ringing_batch_v1"],
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
    this.serverEpoch = result.server_epoch;
    this.leaseTtlMs = result.lease_ttl_ms;
    this.clientSessionId = result.client_session_id;
    const renewInterval = Math.max(1000, Math.floor(result.renew_interval_ms / 2));
    this.renewTimer = setInterval(() => void this.renew(), renewInterval);
  }

  /** POST /ringing/v1/leases/renew。 */
  private async renew(): Promise<void> {
    if (this.closed) return;
    try {
      const response = await fetch(`${this.baseUrl}/ringing/v1/leases/renew`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.token}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ client_session_id: this.clientSessionId }),
      });
      if (!response.ok) {
        // lease 续租失败不触发全局断联（控制面在 open/命令时显式报错）
        console.warn(`[ringing] lease renew failed: HTTP ${response.status}`);
      }
    } catch (err) {
      console.warn("[ringing] lease renew error", err);
    }
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
  ) {
    this.session = new RingingSession(baseUrl, token);
    this.streams = {
      control: new RingingChannelStream(
        `${baseUrl}/ringing/v1/events/control`, token, "control",
        (b) => onBatch(b), (s) => onStatus("control", s),
      ),
      conversation: new RingingChannelStream(
        `${baseUrl}/ringing/v1/events/conversation`, token, "conversation",
        (b) => onBatch(b), (s) => onStatus("conversation", s),
      ),
      tool: new RingingChannelStream(
        `${baseUrl}/ringing/v1/events/tool`, token, "tool",
        (b) => onBatch(b), (s) => onStatus("tool", s),
      ),
    };
  }

  async connect(): Promise<void> {
    await this.session.open();
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
