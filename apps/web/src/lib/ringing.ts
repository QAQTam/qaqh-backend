// Ringing V1 HTTP/SSE 客户端 — 浏览器侧，直接对接 qaqh-daemon。
//
// 注意：daemon 要求所有请求携带 `Authorization: Bearer <token>`，SSE 无法用
// EventSource（不能自定义 header），因此用 fetch + ReadableStream 手动解析。

import {
  BASE,
  SESSION_HEADER,
  SCHEMA,
  VERSION,
  CAPABILITIES,
  type RingingChannel,
  type ClientOpenRequest,
  type ClientOpenResponse,
  type CommandEnvelope,
  type RingingCommandAck,
  type EventEnvelope,
  type ResetRequired,
  type TimelinePage,
  type WebConfig,
  type ContentRef,
} from './types';

export type ConnStatus =
  | { state: 'idle' }
  | { state: 'connecting' }
  | { state: 'open'; epoch: string; sessionId: string; ttlMs: number }
  | { state: 'error'; message: string };

export interface StreamStatus {
  channel: RingingChannel;
  state: 'connecting' | 'open' | 'reconnecting' | 'closed';
  cursor: number;
  retryMs?: number;
}

export interface ClientHandlers {
  onEvent(channel: RingingChannel, env: EventEnvelope): void;
  onReset(reset: ResetRequired): void;
  onStatus(channel: RingingChannel, status: StreamStatus): void;
  onConnChange(status: ConnStatus): void;
}

export class RingingClient {
  readonly base: string;
  readonly token: string;
  private handlers: ClientHandlers;

  private sessionId: string | null = null;
  private instanceId: string = crypto.randomUUID();
  private epoch: string | null = null;
  private renewIntervalMs = 10_000;
  private renewTimer: ReturnType<typeof setInterval> | null = null;

  private cursors: Record<RingingChannel, number> = { control: 0, conversation: 0, tool: 0 };
  private streamAborters: Partial<Record<RingingChannel, AbortController>> = {};
  private stopped = false;
  private reconnectTimers: Partial<Record<RingingChannel, ReturnType<typeof setTimeout>>> = {};

  constructor(base: string, token: string, handlers: ClientHandlers) {
    this.base = base.replace(/\/+$/, '');
    this.token = token;
    this.handlers = handlers;
  }

  // ── 生命周期 ────────────────────────────────────────────────────────────

  async open(): Promise<void> {
    this.handlers.onConnChange({ state: 'connecting' });
    try {
      const req: ClientOpenRequest = {
        schema: SCHEMA,
        version: VERSION,
        client_instance_id: this.instanceId,
        capabilities: [...CAPABILITIES],
      };
      const resp = await fetch(`${this.base}${BASE}/clients/open`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${this.token}`,
        },
        body: JSON.stringify(req),
      });
      if (!resp.ok) {
        const text = await resp.text().catch(() => '');
        throw new Error(`open failed: HTTP ${resp.status} ${text.slice(0, 200)}`);
      }
      const result: ClientOpenResponse = await resp.json();
      if (!result.accepted || !result.client_session_id || !result.server_epoch) {
        throw new Error('open not accepted by daemon');
      }
      this.sessionId = result.client_session_id;
      this.epoch = result.server_epoch;
      this.renewIntervalMs = result.renew_interval_ms || 10_000;
      this.cursors = { control: 0, conversation: 0, tool: 0 };
      this.startRenewal();
      this.handlers.onConnChange({
        state: 'open',
        epoch: this.epoch,
        sessionId: this.sessionId,
        ttlMs: result.lease_ttl_ms,
      });
    } catch (err) {
      this.handlers.onConnChange({ state: 'error', message: String(err) });
      throw err;
    }
  }

  async close(): Promise<void> {
    this.stopped = true;
    if (this.renewTimer) clearInterval(this.renewTimer);
    for (const [ch, ab] of Object.entries(this.streamAborters)) {
      ab?.abort();
      delete this.streamAborters[ch as RingingChannel];
    }
    for (const [ch, t] of Object.entries(this.reconnectTimers)) {
      clearTimeout(t);
      delete this.reconnectTimers[ch as RingingChannel];
    }
    this.handlers.onConnChange({ state: 'idle' });
  }

  private startRenewal() {
    if (this.renewTimer) clearInterval(this.renewTimer);
    const interval = Math.max(1000, this.renewIntervalMs / 2);
    this.renewTimer = setInterval(() => {
      void this.renew().catch(() => {
        // 续租失败（通常 lease 过期/daemon 重启）→ 重开会话并重连流。
        if (this.stopped) return;
        this.sessionId = null;
        void this.open()
          .then(() => {
            for (const ch of ['control', 'conversation', 'tool'] as const) {
              this.connectStream(ch);
            }
          })
          .catch(() => {});
      });
    }, interval);
  }

  private async renew(): Promise<void> {
    const sid = this.sessionId;
    if (!sid) return;
    const resp = await fetch(`${this.base}${BASE}/leases/renew`, {
      method: 'POST',
      headers: this.authHeaders(sid),
    });
    if (!resp.ok) throw new Error(`renew failed: HTTP ${resp.status}`);
  }

  private authHeaders(sid?: string | null): Record<string, string> {
    const h: Record<string, string> = {
      Authorization: `Bearer ${this.token}`,
      'Content-Type': 'application/json',
    };
    const id = sid ?? this.sessionId;
    if (id) h[SESSION_HEADER] = id;
    return h;
  }

  // ── 命令 / 查询 / 动作 ─────────────────────────────────────────────────

  async sendCommand(channel: RingingChannel, command: Record<string, unknown>, seed?: string): Promise<RingingCommandAck> {
    const sid = this.sessionId;
    if (!sid) throw new Error('client session not open');
    const envelope: CommandEnvelope = {
      schema: SCHEMA,
      version: VERSION,
      channel,
      command_id: crypto.randomUUID(),
      client_instance_id: this.instanceId,
      client_session_id: sid,
      command,
    };
    if (seed) envelope.seed = seed;
    const resp = await fetch(`${this.base}${BASE}/commands/${channel}`, {
      method: 'POST',
      headers: this.authHeaders(sid),
      body: JSON.stringify(envelope),
    });
    const ack = (await resp.json().catch(() => null)) as RingingCommandAck | null;
    if (!resp.ok) {
      throw new Error(
        `command rejected (HTTP ${resp.status}): ${ack?.message ?? ack?.code ?? 'unknown error'}`,
      );
    }
    if (!ack) throw new Error(`command ${channel}/${String(command.type)}: invalid ack`);
    return ack;
  }

  async commandStatus(commandId: string): Promise<unknown> {
    const resp = await fetch(`${this.base}${BASE}/commands/${commandId}`, {
      method: 'GET',
      headers: this.authHeaders(),
    });
    if (!resp.ok) throw new Error(`command status: HTTP ${resp.status}`);
    return resp.json();
  }

  async query(name: string, params: Record<string, unknown> = {}): Promise<unknown> {
    const resp = await fetch(`${this.base}${BASE}/queries/${name}`, {
      method: 'POST',
      headers: this.authHeaders(),
      body: JSON.stringify(params),
    });
    if (!resp.ok) {
      const text = await resp.text().catch(() => '');
      throw new Error(`query ${name}: HTTP ${resp.status} ${text.slice(0, 300)}`);
    }
    return resp.json();
  }

  /** 辅助动作（config.save 等）：按 daemon 要求计算 sha256 指纹。 */
  async action(name: string, params: Record<string, unknown>): Promise<unknown> {
    // 指纹 payload：{"method":name,"params":{...}}，params 键序 = 发送序。
    const paramsForHash: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(params)) paramsForHash[k] = v;
    const payload = JSON.stringify({ method: name, params: paramsForHash });
    const fingerprint = await sha256Hex(payload);
    const body = { action_id: crypto.randomUUID(), fingerprint, ...params };
    const resp = await fetch(`${this.base}${BASE}/actions/${name}`, {
      method: 'POST',
      headers: this.authHeaders(),
      body: JSON.stringify(body),
    });
    if (!resp.ok) {
      const text = await resp.text().catch(() => '');
      throw new Error(`action ${name}: HTTP ${resp.status} ${text.slice(0, 300)}`);
    }
    return resp.json();
  }

  async loadConfig(): Promise<WebConfig> {
    return (await this.query('config.load', {})) as WebConfig;
  }

  async listSessions(): Promise<import('./types').SessionMeta[]> {
    return (await this.query('session.list', {})) as import('./types').SessionMeta[];
  }

  async fetchTimeline(seed: string): Promise<TimelinePage> {
    const resp = await fetch(`${this.base}${BASE}/sessions/${encodeURIComponent(seed)}/timeline`, {
      method: 'GET',
      headers: this.authHeaders(),
    });
    if (!resp.ok) throw new Error(`timeline: HTTP ${resp.status}`);
    return (await resp.json()) as TimelinePage;
  }

  async downloadContent(seed: string, ref: ContentRef): Promise<Blob> {
    const resp = await fetch(
      `${this.base}${BASE}/content/${ref.content_id}?seed=${encodeURIComponent(seed)}`,
      { method: 'GET', headers: this.authHeaders() },
    );
    if (!resp.ok) throw new Error(`content: HTTP ${resp.status}`);
    return resp.blob();
  }

  // ── SSE 流 ─────────────────────────────────────────────────────────────

  connectAll(): void {
    for (const ch of ['control', 'conversation', 'tool'] as const) {
      this.connectStream(ch);
    }
  }

  private connectStream(channel: RingingChannel) {
    if (this.stopped) return;
    if (this.streamAborters[channel]) return; // already streaming
    const aborter = new AbortController();
    this.streamAborters[channel] = aborter;
    void this.runStream(channel, aborter.signal);
  }

  private async runStream(channel: RingingChannel, signal: AbortSignal) {
    const cursor = this.cursors[channel];
    this.handlers.onStatus(channel, { channel, state: 'connecting', cursor });
    try {
      const headers = this.authHeaders();
      if (cursor > 0 && this.epoch) {
        headers['Last-Event-ID'] = `${this.epoch}:${channel}:${cursor}`;
      }
      const resp = await fetch(`${this.base}${BASE}/events/${channel}`, {
        method: 'GET',
        headers,
        signal,
      });
      if (!resp.ok) throw new Error(`SSE ${channel}: HTTP ${resp.status}`);
      if (!resp.body) throw new Error(`SSE ${channel}: no body`);
      this.handlers.onStatus(channel, { channel, state: 'open', cursor });
      const reader = resp.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let sep: number;
        while ((sep = buffer.indexOf('\n\n')) >= 0) {
          const frame = buffer.slice(0, sep);
          buffer = buffer.slice(sep + 2);
          this.handleFrame(channel, frame);
        }
      }
      throw new Error(`SSE ${channel}: stream ended`);
    } catch (err) {
      if (signal.aborted) return; // intentional stop
      const retryMs = this.scheduleReconnect(channel);
      this.handlers.onStatus(channel, {
        channel,
        state: 'reconnecting',
        cursor: this.cursors[channel],
        retryMs,
      });
    } finally {
      delete this.streamAborters[channel];
    }
  }

  private reconnectAttempts: Partial<Record<RingingChannel, number>> = {};
  private scheduleReconnect(channel: RingingChannel): number {
    const attempts = (this.reconnectAttempts[channel] ?? 0) + 1;
    this.reconnectAttempts[channel] = attempts;
    const delay = Math.min(1000 * 2 ** Math.min(attempts - 1, 4), 15_000);
    this.reconnectTimers[channel] = setTimeout(() => {
      delete this.reconnectTimers[channel];
      if (!this.stopped) this.connectStream(channel);
    }, delay);
    return delay;
  }

  private handleFrame(channel: RingingChannel, frame: string) {
    let id = '';
    let eventType = '';
    let data = '';
    for (const line of frame.split('\n')) {
      if (line.startsWith(':')) continue; // keepalive comment
      if (line.startsWith('id:')) id = line.slice(3).trim();
      else if (line.startsWith('event:')) eventType = line.slice(6).trim();
      else if (line.startsWith('data:')) {
        if (data) data += '\n';
        data += line.slice(5).trimStart();
      }
    }
    if (!data) return;

    // cursor 更新：id 形如 <epoch>:<channel>:<seq>
    if (id && this.epoch) {
      const [e, ch, seqStr] = id.split(':');
      if (e === this.epoch && ch === channel) {
        const seq = Number(seqStr);
        if (Number.isFinite(seq) && seq > this.cursors[channel]) {
          this.cursors[channel] = seq;
        }
      }
    }

    if (eventType === 'ringing.reset_required') {
      try {
        const reset = JSON.parse(data) as ResetRequired;
        this.cursors[channel] = 0;
        this.handlers.onReset(reset);
      } catch {
        /* ignore malformed reset */
      }
      return;
    }

    try {
      const env = JSON.parse(data) as EventEnvelope;
      if (env && env.seed && env.event) {
        this.handlers.onEvent(channel, env);
      }
    } catch {
      /* ignore malformed event */
    }
  }
}

export async function sha256Hex(text: string): Promise<string> {
  const bytes = new TextEncoder().encode(text);
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}
