// Ringing 会话管理层（Electron main 进程）。
//
// 职责：
// - 持有全局唯一的 RingingClient（三条 SSE 流，app 生命周期）；
// - 维护 sessionChannelMode[seed][channel] 内存表（PLAN 切流语义）；
// - 提供 cutover HTTP 调用（events prepare/commit/abort + commands 切换）；
// - 向 renderer 转发 batch（整批 IPC）与频道连接状态。
//
// 切流是 sticky：commit 后该 seed+channel 的事件协议为 Ringing，
// 直到 daemon 重启（CutoverState 是 daemon 内存态）。main 侧模式表
// 在 renderer 刷新后仍保留，保证刷新后重连仍走 Ringing 路径。

import { RingingClient, type ChannelStatus } from "./ringingClient";
import type { RingingEventBatch } from "../src/lib/types/ringing";

export type ChannelName = "control" | "conversation" | "tool";

export interface ChannelMode {
  eventProtocol: "legacy" | "ringing";
  commandProtocol: "legacy" | "ringing";
}

export type SessionModes = Record<ChannelName, ChannelMode>;

const DEFAULT_MODE: ChannelMode = { eventProtocol: "legacy", commandProtocol: "legacy" };

function defaultModes(): SessionModes {
  return { control: { ...DEFAULT_MODE }, conversation: { ...DEFAULT_MODE }, tool: { ...DEFAULT_MODE } };
}

export class RingingManager {
  private client: RingingClient | null = null;
  private baseUrl = "";
  private token = "";
  private readonly modes = new Map<string, SessionModes>();
  private readonly channelStatus: Record<ChannelName, ChannelStatus | null> = {
    control: null,
    conversation: null,
    tool: null,
  };

  constructor(
    private readonly onBatch: (batch: RingingEventBatch) => void,
    private readonly onStatus: (channel: ChannelName, status: ChannelStatus) => void,
  ) {}

  /** backend connect 成功后调用；重复调用幂等（同一 daemon 不重建）。 */
  async ensureConnected(baseUrl: string, token: string): Promise<void> {
    if (this.client && this.baseUrl === baseUrl) return;
    this.close();
    this.baseUrl = baseUrl;
    this.token = token;
    const client = new RingingClient(
      baseUrl,
      token,
      (batch) => this.onBatch(batch),
      (channel, status) => {
        this.channelStatus[channel] = status;
        this.onStatus(channel, status);
      },
    );
    this.client = client;
    try {
      await client.connect();
      console.log("[ringing] connected to daemon", baseUrl);
    } catch (error) {
      console.warn("[ringing] connect failed (legacy 继续承载)", error);
      this.client = null;
    }
  }

  close(): void {
    this.client?.close();
    this.client = null;
  }

  connected(): boolean {
    return this.client !== null;
  }

  status(): Record<ChannelName, ChannelStatus | null> {
    return { ...this.channelStatus };
  }

  mode(seed: string): SessionModes {
    return this.modes.get(seed) ?? defaultModes();
  }

  isEventRinging(seed: string, channel: ChannelName): boolean {
    return this.mode(seed)[channel].eventProtocol === "ringing";
  }

  /** 事件切流（两阶段）：prepare → (snapshot/SSE 就绪) → commit；失败/超时 abort。 */
  async cutoverEvents(
    seed: string,
    channel: ChannelName,
    action: "prepare" | "commit" | "abort",
  ): Promise<{ eventProtocol: string; commandProtocol: string }> {
    const body = await this.postJson(`/ringing/v1/cutover/events/${channel}`, {
      action,
      seed,
      client_instance_id: this.instanceId(),
    });
    // 同步内存模式表（服务端为权威；此处为 renderer 刷新后的重连依据）
    if (action === "commit") {
      this.modes.set(seed, {
        ...this.mode(seed),
        [channel]: { ...this.mode(seed)[channel], eventProtocol: "ringing" },
      });
    } else if (action === "abort") {
      // abort 后保持 legacy（当前已是 legacy 或回退）
      this.modes.set(seed, {
        ...this.mode(seed),
        [channel]: { ...this.mode(seed)[channel], eventProtocol: "legacy" },
      });
    }
    return body;
  }

  /** 命令切流（单阶段）。 */
  async cutoverCommands(
    seed: string,
    channel: ChannelName,
    protocol: "ringing" | "legacy",
  ): Promise<{ commandProtocol: string; eventProtocol: string }> {
    const body = await this.postJson(`/ringing/v1/cutover/commands/${channel}`, {
      protocol,
      seed,
      client_instance_id: this.instanceId(),
    });
    this.modes.set(seed, {
      ...this.mode(seed),
      [channel]: { ...this.mode(seed)[channel], commandProtocol: protocol },
    });
    return body;
  }

  /** 拉取频道领域快照（切流/reload 后重建前端状态）。 */
  async snapshot(seed: string, channel: ChannelName): Promise<unknown> {
    if (!this.client) throw new Error("ringing not connected");
    const response = await fetch(
      `${this.baseUrl}/ringing/v1/snapshots/${channel}/${encodeURIComponent(seed)}`,
      {
        method: "GET",
        headers: { Authorization: `Bearer ${this.token}` },
      },
    );
    if (!response.ok) throw new Error(`snapshot failed: HTTP ${response.status}`);
    return response.json();
  }

  private instanceId(): string {
    if (!this.client) throw new Error("ringing not connected");
    return this.client.session.clientInstanceId;
  }

  private async postJson(path: string, payload: unknown): Promise<any> {
    if (!this.client) throw new Error("ringing not connected");
    const response = await fetch(`${this.baseUrl}${path}`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${this.token}`,
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
      throw new Error(`cutover failed: ${message}`);
    }
    return json;
  }
}
