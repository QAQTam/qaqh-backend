// Ringing 会话管理层（Electron main 进程）。
//
// 职责：
// - 持有全局唯一的 RingingClient（三条 SSE 流，app 生命周期）；
// - 维护连接级 v2 session，不按 seed/channel 切换协议；
// - 向 renderer 转发 batch（整批 IPC）与频道连接状态。
//
// 一个 daemon 连接只使用 Ringing v2 或 legacy backend；Ringing v2 本身
// 不暴露 per-seed/channel mode。

import { createHash, randomUUID } from "node:crypto";
import { RingingClient, type ChannelStatus, type RingingSessionOpen } from "./ringingClient";
import { TimelineClient, type TimelineStatus } from "./timelineClient";
import type { RingingEventBatch, RingingResetRequired } from "../src/lib/types/ringing";
import type { TimelineEntry, TimelineSnapshotResponse } from "../src/store/timelineProtocol";

export type ChannelName = "control" | "conversation" | "tool";

const MAX_BOOTSTRAP_QUEUE_ENVELOPES = 8192;
const MAX_BOOTSTRAP_QUEUE_BYTES = 8 * 1024 * 1024;
const RECOVERY_BASE_MS = 1_000;
const RECOVERY_MAX_MS = 30_000;

export class RingingManager {
  private client: RingingClient | null = null;
  private timelineClient: TimelineClient | null = null;
  private timelineStatus: TimelineStatus | null = null;
  private baseUrl = "";
  private token = "";
  private lastConnectError: string | null = null;
  private readonly pendingBatches = new Map<string, {
    batch: RingingEventBatch;
    bytes: number;
    timer: ReturnType<typeof setTimeout>;
  }>();
  /** bootstrap ACK 前到达的 live batch 队列；ACK 后按原序释放到 renderer。 */
  private readonly bootstrapQueues = new Map<string, RingingEventBatch[]>();
  private readonly bootstrapQueueStats = new Map<string, { envelopes: number; bytes: number }>();
  private readonly bootstrapOverflow = new Set<string>();
  private readonly bootstrapping = new Set<string>();
  private readonly channelStatus: Record<ChannelName, ChannelStatus | null> = {
    control: null,
    conversation: null,
    tool: null,
  };
  private recoveryTimer: ReturnType<typeof setTimeout> | null = null;
  private recoveryAttempt = 0;

  constructor(
    private readonly onBatch: (batch: RingingEventBatch) => void,
    private readonly onStatus: (channel: ChannelName, status: ChannelStatus) => void,
    private readonly onSnapshot?: (
      seed: string,
      channel: ChannelName,
      snapshot: unknown,
    ) => void,
    private readonly onRecoveryRequired?: () => Promise<void>,
    private readonly onTimelineEntry?: (seed: string, entry: TimelineEntry) => void,
    private readonly onTimelineStatus?: (status: TimelineStatus) => void,
    private readonly onTimelineSnapshot?: (snapshot: TimelineSnapshotResponse) => void,
  ) {}

  /** backend connect 成功后调用；重复调用幂等（同一 daemon 不重建）。 */
  async ensureConnected(
    baseUrl: string,
    token: string,
    openedSession?: RingingSessionOpen,
  ): Promise<void> {
    if (
      this.client &&
      this.baseUrl === baseUrl &&
      (!openedSession || this.client.session.clientSessionId === openedSession.clientSessionId)
    ) return;
    this.close();
    this.baseUrl = baseUrl;
    this.token = token;
    const client = new RingingClient(
      baseUrl,
      token,
      (batch) => this.enqueueBatch(batch),
      (channel, status) => {
        this.channelStatus[channel] = status;
        this.onStatus(channel, status);
      },
      (reset) => void this.handleReset(reset),
      openedSession,
      () => this.scheduleRecovery(),
    );
    this.client = client;
    try {
      await client.connect();
      this.lastConnectError = null;
      console.log("[ringing] connected to daemon", baseUrl);
    } catch (error) {
      this.lastConnectError = error instanceof Error ? error.message : String(error);
      console.warn("[ringing] v2 stream setup failed", error);
      this.client = null;
    }
  }

  close(): void {
    if (this.recoveryTimer) clearTimeout(this.recoveryTimer);
    this.recoveryTimer = null;
    this.flushBatches();
    this.bootstrapQueues.clear();
    this.bootstrapQueueStats.clear();
    this.bootstrapOverflow.clear();
    this.bootstrapping.clear();
    this.timelineClient?.close("ringing connection closed");
    this.timelineClient = null;
    this.timelineStatus = null;
    this.client?.close();
    this.client = null;
  }

  /** lease 连续两次未获确认：重新 open、重建三条 SSE，保持 Ringing v2。 */
  private scheduleRecovery(): void {
    if (this.recoveryTimer || !this.onRecoveryRequired) return;
    const delay = Math.min(RECOVERY_BASE_MS * 2 ** this.recoveryAttempt, RECOVERY_MAX_MS);
    this.recoveryAttempt += 1;
    this.recoveryTimer = setTimeout(() => {
      this.recoveryTimer = null;
      this.client?.close();
      this.client = null;
      void this.onRecoveryRequired!()
        .then(() => { this.recoveryAttempt = 0; })
        .catch((error) => {
          this.lastConnectError = error instanceof Error ? error.message : String(error);
          this.scheduleRecovery();
        });
    }, delay);
  }

  connected(): boolean {
    return this.client !== null;
  }

  /** 当前连接的 daemon baseUrl（用于检测 daemon 重启后端口变化）。 */
  connectedBaseUrl(): string {
    return this.baseUrl;
  }

  /** 返回已连接客户端；未连接时抛出带原因的明确错误。 */
  private requireClient(): RingingClient {
    if (!this.client) {
      const reason = this.lastConnectError
        ? ` (last connect error: ${this.lastConnectError})`
        : "";
      throw new Error(`ringing not connected${reason}`);
    }
    return this.client;
  }

  status(): Record<ChannelName, ChannelStatus | null> {
    return { ...this.channelStatus };
  }

  timelineConnectionStatus(): TimelineStatus | null {
    return this.timelineStatus;
  }

  /**
   * Activates the native transcript for one renderer-selected session. A
   * snapshot watermark becomes the sole SSE cursor; any entry emitted after
   * the snapshot is replayed by the server before live delivery begins.
   */
  async activateTimeline(seed: string): Promise<TimelineSnapshotResponse> {
    const client = this.requireClient();
    const response = await this.getJson(
      `/ringing/v3/sessions/${encodeURIComponent(seed)}/timeline`,
      "timeline snapshot",
    ) as TimelineSnapshotResponse;
    if (
      response.schema !== "deepx.Timeline"
      || response.version !== 3
      || response.seed !== seed
      || !Number.isSafeInteger(response.snapshot?.watermark)
    ) throw new Error("invalid Timeline v3 snapshot");

    this.timelineClient?.close("session changed");
    const stream = new TimelineClient(
      this.baseUrl,
      this.token,
      seed,
      () => client.session.serverEpoch,
      () => client.session.clientSessionId ?? "",
      entry => this.onTimelineEntry?.(seed, entry),
      status => {
        this.timelineStatus = status;
        this.onTimelineStatus?.(status);
      },
      response.snapshot.watermark,
    );
    this.timelineClient = stream;
    this.onTimelineSnapshot?.(response);
    stream.start();
    return response;
  }

  /** 拉取频道领域快照（reload 后重建前端状态）。 */
  async snapshot(seed: string, channel: ChannelName): Promise<unknown> {
    const bootstrap = await this.bootstrapSession(seed);
    const selected = channel === "control"
      ? "control"
      : channel === "conversation" ? "conversation" : "tool";
    return (bootstrap as Record<string, unknown>)[selected];
  }

  /** 拉取一次完整 session bootstrap；三频道共享同一基线和 live queue。 */
  async bootstrapSession(seed: string): Promise<unknown> {
    this.requireClient();
    const keys = (["control", "conversation", "tool"] as ChannelName[])
      .map((channel) => `${seed}:${channel}`);
    for (const key of keys) this.bootstrapping.add(key);
    try {
      for (;;) {
        const bootstrap = await this.bootstrap(seed);
        // Any overflow invalidates the full session baseline. Re-bootstrap
        // while all channels remain queued; never fall back to legacy.
        let overflowed = false;
        for (const key of keys) {
          if (this.bootstrapOverflow.delete(key)) overflowed = true;
        }
        if (overflowed) {
          for (const key of keys) {
            this.bootstrapQueues.delete(key);
            this.bootstrapQueueStats.delete(key);
          }
          continue;
        }
        return bootstrap;
      }
    } finally {
      const queued = keys.flatMap((key) => {
        const batches = this.bootstrapQueues.get(key) ?? [];
        this.bootstrapQueues.delete(key);
        this.bootstrapQueueStats.delete(key);
        this.bootstrapping.delete(key);
        return batches;
      });
      for (const batch of queued) this.enqueueBatch(batch);
    }
  }

  async bootstrap(seed: string): Promise<unknown> {
    this.requireClient();
    return this.getJson(`/ringing/v2/sessions/${encodeURIComponent(seed)}/bootstrap`, "bootstrap");
  }

  /** Ringing 命令（POST /ringing/v2/commands/{channel}）。
   *
   * daemon 负责 lease + 幂等校验；`client_instance_id` 缺省时由 main 进程
   * 填充当前连接的实例 id（renderer 不持有该标识）。
   */
  async command(
    seed: string,
    channel: ChannelName,
    envelope: {
      command_id: string;
      command: unknown;
      seed?: string | null;
      expected_revision?: number | null;
      client_instance_id?: string;
    },
  ): Promise<unknown> {
    this.requireClient();
    const payload = {
      schema: "deepx.Ringing",
      version: 2,
      channel,
      command_id: envelope.command_id,
      client_instance_id: envelope.client_instance_id ?? this.instanceId(),
      client_session_id: this.requireClient().session.clientSessionId,
      // SessionCreate 是唯一允许省略 seed 的命令；其他命令必须带会话 seed。
      seed: envelope.seed ?? (seed || null),
      expected_revision: envelope.expected_revision ?? null,
      command: envelope.command,
    };
    try {
      return await this.postJson(`/ringing/v2/commands/${channel}`, payload, "command");
    } catch (error) {
      // The POST response may be lost after daemon acceptance. Resolve the
      // uncertainty with the same command id before surfacing the error.
      try {
        const status = await this.commandStatus(envelope.command_id) as {
          state?: string;
          error_code?: string | null;
        };
        if (status.state === "failed" || status.state === "rejected") {
          return {
            command_id: envelope.command_id,
            status: "rejected",
            code: status.error_code ?? status.state,
          };
        }
        return {
          command_id: envelope.command_id,
          status: "accepted",
          receipt_state: status.state,
        };
      } catch {
        throw error;
      }
    }
  }

  /** Ringing typed query（POST /ringing/v2/queries/{name}）。 */
  async query(path: string, params?: Record<string, string | undefined>): Promise<unknown> {
    this.requireClient();
    return this.postJson(`/ringing/v2/queries/${path}`, params ?? {}, "query");
  }

  /** 连接级辅助 action（git/workspace/config/skills/plan/todo 等）。 */
  async action(name: string, params: Record<string, unknown> = {}): Promise<unknown> {
    this.requireClient();
    const actionId = randomUUID();
    const fingerprint = createHash("sha256")
      .update(JSON.stringify({ method: name, params }))
      .digest("hex");
    return this.postJson(`/ringing/v2/actions/${name}`, {
      ...params,
      action_id: actionId,
      fingerprint,
    }, "action");
  }

  /** 查询命令 receipt；断线重试使用同一 command_id。 */
  async commandStatus(commandId: string): Promise<unknown> {
    this.requireClient();
    return this.getJson(`/ringing/v2/commands/${encodeURIComponent(commandId)}`, "command status");
  }

  /** Electron main 读取本地文件后上传；renderer 不接触 content token/HTTP。 */
  async uploadContent(seed: string, mediaType: string, bytes: Uint8Array): Promise<unknown> {
    this.requireClient();
    const form = new FormData();
    form.append("seed", seed);
    form.append("media_type", mediaType);
    const buffer = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
    form.append("content", new Blob([buffer], { type: mediaType }));
    const response = await fetch(`${this.baseUrl}/ringing/v2/content`, {
      method: "POST",
      headers: this.headers(),
      body: form,
    });
    const text = await response.text();
    let json: unknown = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* non JSON error */ }
    if (!response.ok) throw new Error(`content upload failed: HTTP ${response.status}`);
    return json;
  }

  /** cursor 超出保留窗口：拉取权威 snapshot，更新流 cursor 并转发 renderer。 */
  private async handleReset(reset: RingingResetRequired): Promise<void> {
    if (!this.client) return;
    try {
      const snapshot = await this.snapshot(reset.seed, reset.channel);
      const baseline = (snapshot as { baseline_stream_seq?: number })?.baseline_stream_seq;
      if (typeof baseline === "number") {
        this.client.streams[reset.channel].resetCursor(baseline);
      }
      this.onSnapshot?.(reset.seed, reset.channel, snapshot);
    } catch (err) {
      console.warn("[ringing] reset snapshot failed", err);
    }
  }

  private instanceId(): string {
    return this.requireClient().session.clientInstanceId;
  }

  private enqueueBatch(batch: RingingEventBatch): void {
    const key = `${batch.seed}:${batch.channel}`;
    const bytes = new TextEncoder().encode(JSON.stringify(batch)).byteLength;
    if (this.bootstrapping.has(key)) {
      const queue = this.bootstrapQueues.get(key) ?? [];
      const stats = this.bootstrapQueueStats.get(key) ?? { envelopes: 0, bytes: 0 };
      const envelopes = stats.envelopes + batch.envelopes.length;
      const totalBytes = stats.bytes + bytes;
      if (envelopes > MAX_BOOTSTRAP_QUEUE_ENVELOPES || totalBytes > MAX_BOOTSTRAP_QUEUE_BYTES) {
        this.bootstrapQueues.set(key, []);
        this.bootstrapQueueStats.set(key, { envelopes: 0, bytes: 0 });
        this.bootstrapOverflow.add(key);
        return;
      }
      queue.push(batch);
      this.bootstrapQueues.set(key, queue);
      this.bootstrapQueueStats.set(key, { envelopes, bytes: totalBytes });
      return;
    }
    const existing = this.pendingBatches.get(key);
    if (
      existing
      && existing.batch.server_epoch === batch.server_epoch
      && existing.batch.to_stream_seq + 1 === batch.from_stream_seq
      && existing.bytes + bytes <= 256 * 1024
    ) {
      existing.batch = {
        ...existing.batch,
        to_stream_seq: batch.to_stream_seq,
        envelopes: [...existing.batch.envelopes, ...batch.envelopes],
      };
      existing.bytes += bytes;
      if (existing.bytes >= 256 * 1024) this.flushBatch(key);
      return;
    }
    if (existing) this.flushBatch(key);
    const timer = setTimeout(() => this.flushBatch(key), 16);
    this.pendingBatches.set(key, { batch, bytes, timer });
  }

  private flushBatch(key: string): void {
    const pending = this.pendingBatches.get(key);
    if (!pending) return;
    clearTimeout(pending.timer);
    this.pendingBatches.delete(key);
    this.onBatch(pending.batch);
  }

  private flushBatches(): void {
    for (const key of [...this.pendingBatches.keys()]) this.flushBatch(key);
  }

  private async getJson(path: string, label: string): Promise<unknown> {
    const response = await fetch(`${this.baseUrl}${path}`, {
      headers: this.headers(),
    });
    const text = await response.text();
    let json: unknown = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* non JSON error */ }
    if (!response.ok) {
      const message = (json as { message?: string; code?: string } | null)?.message
        ?? (json as { code?: string } | null)?.code
        ?? `HTTP ${response.status}`;
      throw new Error(`${label} failed: ${message}`);
    }
    return json;
  }

  private headers(): Record<string, string> {
    return {
      Authorization: `Bearer ${this.token}`,
      "X-DeepX-Client-Session-Id": this.requireClient().session.clientSessionId ?? "",
    };
  }

  private async postJson(path: string, payload: unknown, label = "ringing request"): Promise<any> {
    this.requireClient();
    const response = await fetch(`${this.baseUrl}${path}`, {
      method: "POST",
      headers: {
        ...this.headers(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify(payload),
    });
    const text = await response.text();
    let json: any = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      // 非 JSON 响应（错误页）
    }
    if (!response.ok) {
      const message = json?.message ?? json?.code ?? `HTTP ${response.status}`;
      throw new Error(`${label} failed: ${message}`);
    }
    return json;
  }
}
