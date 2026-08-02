import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { app } from "electron";
import WebSocket from "ws";
import { daemonIdentityMismatch, hasActiveDaemonWork, type ExpectedDaemonIdentity } from "../src/runtime/daemonLifecycle";
import { ControlCursor } from "../src/runtime/controlCursor";
import type { RingingSessionOpen } from "./ringingClient";
import type { BackendStatus, ControlMessage, DaemonDiscovery, DaemonManifest } from "./types";

const PROTOCOL_VERSION = 1;
const REQUEST_TIMEOUT_MS = 30_000;
const START_TIMEOUT_MS = 8_000;
const CLOSE_TIMEOUT_MS = 1_500;

type Pending = {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
};

export interface RingingConnectionInfo {
  baseUrl: string;
  token: string;
  session: RingingSessionOpen;
}

export class DaemonControlClient {
  private socket?: WebSocket;
  private connecting?: Promise<void>;
  private heartbeat?: NodeJS.Timeout;
  private reconnect?: NodeJS.Timeout;
  private upgradeCheck?: NodeJS.Timeout;
  private readonly cursor = new ControlCursor();
  private readonly pending = new Map<string, Pending>();
  private readonly attached = new Set<string>();
  private updateReattach: string[] = [];
  private readonly clientId = `electron-${randomUUID()}`;
  private stopped = false;
  private restarting = false;
  private closing?: Promise<void>;
  private status: BackendStatus = { connected: false, transport: "legacy" };
  private lastDiscovery?: { baseUrl: string; token: string };
  private ringingConnection?: RingingConnectionInfo;
  private transport: "ringing" | "legacy" = "legacy";

  /** 最近一次成功的 daemon discovery（Ringing HTTP 客户端复用同一 token）。 */
  discoveryInfo(): { baseUrl: string; token: string } | null {
    return this.lastDiscovery ?? null;
  }

  /** 已完成 Ringing V1 open 的会话；只给 Electron main 的 RingingManager 使用。 */
  ringingConnectionInfo(): RingingConnectionInfo | null {
    return this.ringingConnection ?? null;
  }

  /** 强制重新协商 Ringing V1 lease；只用于健康监督恢复，绝不改选 legacy。 */
  async refreshRingingConnection(): Promise<RingingConnectionInfo> {
    if (this.transport !== "ringing") throw new Error("Ringing v1 is not the selected transport");
    const discovery = await readDiscovery();
    if (!(await this.tryConnectRinging(discovery)) || !this.ringingConnection) {
      throw new Error("Ringing v1 is no longer supported by the daemon");
    }
    return this.ringingConnection;
  }

  constructor(
    private readonly onMessage: (message: ControlMessage) => void,
    private readonly onStatus: (status: BackendStatus) => void,
  ) {}

  currentStatus(): BackendStatus {
    return { ...this.status };
  }

  /** 新版 daemon 选择 Ringing 后不再建立 legacy WebSocket。 */
  usingRinging(): boolean {
    return this.transport === "ringing" && this.status.connected;
  }

  async connect(): Promise<void> {
    if (this.socket?.readyState === WebSocket.OPEN) return;
    if (this.usingRinging()) return;
    if (this.connecting) return this.connecting;
    this.stopped = false;
    this.connecting = this.connectOrLaunch().finally(() => { this.connecting = undefined; });
    return this.connecting;
  }

  async request(method: string, params: Record<string, unknown> = {}): Promise<unknown> {
    if (!method || typeof method !== "string") throw new Error("invalid backend method");
    await this.connect();
    if (this.usingRinging()) {
      throw new Error(`legacy backend request is unavailable on Ringing v1: ${method}`);
    }
    return this.roundTrip({
      type: "request",
      request_id: randomUUID(),
      method,
      params,
    });
  }

  async attach(seed: string): Promise<unknown> {
    if (!seed) throw new Error("session seed is required");
    await this.connect();
    if (this.usingRinging()) {
      this.attached.add(seed);
      return { ok: true, transport: "ringing" };
    }
    const result = await this.attachWire(seed);
    this.attached.add(seed);
    return result;
  }

  async detach(seed: string): Promise<unknown> {
    if (!seed) throw new Error("session seed is required");
    await this.connect();
    if (this.usingRinging()) {
      this.attached.delete(seed);
      return { ok: true, transport: "ringing" };
    }
    const result = await this.roundTrip({
      type: "session_detach",
      request_id: randomUUID(),
      seed,
    });
    this.attached.delete(seed);
    return result;
  }

  close(): Promise<void> {
    if (this.closing) return this.closing;
    this.stopped = true;
    if (this.reconnect) clearTimeout(this.reconnect);
    if (this.upgradeCheck) clearTimeout(this.upgradeCheck);
    this.closing = this.releaseLeases().finally(() => {
      this.transport = "legacy";
      this.ringingConnection = undefined;
      this.disconnectSocket();
    });
    return this.closing;
  }

  async prepareBackendUpdate(): Promise<boolean> {
    const discovery = await readDiscovery();
    const stopped = await requestDaemonStop(discovery, true);
    if (stopped === "busy") {
      this.setStatus({ connected: true, updatePending: true });
      return false;
    }
    if (stopped !== "stopping") {
      throw new Error("daemon does not support safe update handoff");
    }
    this.restarting = true;
    this.updateReattach = [...this.attached];
    try {
      this.disconnectSocket();
      await waitForDaemonExit(discovery.pid);
      return true;
    } catch (error) {
      this.restarting = false;
      this.updateReattach = [];
      throw error;
    }
  }

  async resumeAfterBackendUpdate(): Promise<void> {
    try {
      await this.connectOrLaunch();
      for (const seed of this.updateReattach) {
        await this.attachWire(seed);
      }
      this.updateReattach = [];
      this.setStatus({ connected: true });
    } finally {
      this.restarting = false;
    }
  }

  private async releaseLeases(): Promise<void> {
    const seeds = [...this.attached];
    if (seeds.length === 0 || this.socket?.readyState !== WebSocket.OPEN) {
      this.attached.clear();
      return;
    }
    const detachAll = Promise.allSettled(seeds.map(seed => this.roundTrip({
      type: "session_detach",
      request_id: randomUUID(),
      seed,
    })));
    await Promise.race([detachAll, delay(CLOSE_TIMEOUT_MS)]);
    this.attached.clear();
  }

  private async connectOrLaunch(): Promise<void> {
    const expected = await expectedDaemonIdentity();
    let lastError: unknown = new Error("daemon did not publish discovery");
    try {
      const discovery = await readDiscovery();
      const mismatch = daemonIdentityMismatch(discovery, expected);
      if (mismatch) throw new Error(`incompatible daemon: ${mismatch}`);
      if (await this.tryConnectRinging(discovery)) return;
      throw new Error("Ringing v1 is required but this daemon does not support it");
    } catch (error) {
      lastError = error;
      this.disconnectSocket();
    }

    launchDaemon();
    const deadline = Date.now() + START_TIMEOUT_MS;
    while (Date.now() < deadline) {
      await delay(120);
      try {
        const discovery = await readDiscovery();
        const mismatch = daemonIdentityMismatch(discovery, expected);
        if (mismatch) {
          lastError = new Error(`incompatible daemon: ${mismatch}`);
          continue;
        }
        if (await this.tryConnectRinging(discovery)) return;
        lastError = new Error("Ringing v1 is required but this daemon does not support it");
      } catch (error) {
        lastError = error;
      }
    }
    const message = errorMessage(lastError);
    this.setStatus({ connected: false, error: message });
    throw new Error(message);
  }

  private scheduleUpgrade(discovery: DaemonDiscovery, expected: ExpectedDaemonIdentity): void {
    if (this.upgradeCheck) clearTimeout(this.upgradeCheck);
    this.upgradeCheck = setTimeout(async () => {
      this.upgradeCheck = undefined;
      if (this.stopped || this.restarting) return;
      try {
        const activities = await this.roundTrip({
          type: "request",
          request_id: randomUUID(),
          method: "session.activity",
          params: {},
        });
        if (hasActiveDaemonWork(activities)) {
          this.scheduleUpgrade(discovery, expected);
          return;
        }
        await this.takeOverDaemon(discovery, expected, true);
      } catch {
        if (!this.stopped) this.scheduleUpgrade(discovery, expected);
      }
    }, 5_000);
  }

  private async takeOverDaemon(
    discovery: DaemonDiscovery,
    expected: ExpectedDaemonIdentity,
    allowLegacyStop: boolean,
  ): Promise<void> {
    this.restarting = true;
    try {
      let stopped = await requestDaemonStop(discovery, true);
      if (stopped === "busy") {
        this.setStatus({ connected: true, updatePending: true });
        this.scheduleUpgrade(discovery, expected);
        return;
      }
      if (stopped === "unsupported" && allowLegacyStop) {
        stopped = await requestDaemonStop(discovery, false);
      }
      if (stopped !== "stopping") throw new Error("daemon refused lifecycle takeover");

      this.disconnectSocket();
      await waitForDaemonExit(discovery.pid);
      launchDaemon();
      const replacement = await waitForCompatibleDiscovery(expected);
      if (!(await this.tryConnectRinging(replacement))) {
        throw new Error("replacement daemon does not support required Ringing v1");
      }
      this.setStatus({ connected: true });
    } finally {
      this.restarting = false;
    }
  }

  private async connectDiscovery(discovery: DaemonDiscovery): Promise<void> {
    if (discovery.protocol_version !== PROTOCOL_VERSION) {
      throw new Error(`daemon protocol ${discovery.protocol_version} is incompatible`);
    }
    this.transport = "legacy";
    this.ringingConnection = undefined;
    // 保存 baseUrl（仅 origin：ws://host:port/control/v1 → http://host:port）
    // 供 Ringing HTTP 复用。endpoint 带 /control/v1 路径，必须去掉，
    // 否则 /ringing/v1/... 请求会 404。
    const endpointUrl = new URL(discovery.endpoint);
    this.lastDiscovery = {
      baseUrl: `${endpointUrl.protocol === "wss:" ? "https" : "http"}://${endpointUrl.host}`,
      token: discovery.token,
    };
    const socket = new WebSocket(discovery.endpoint, {
      headers: { Authorization: `Bearer ${discovery.token}` },
      maxPayload: 64 * 1024 * 1024,
      handshakeTimeout: 5_000,
    });
    this.socket = socket;

    await new Promise<void>((resolveConnection, rejectConnection) => {
      const timer = setTimeout(() => rejectConnection(new Error("daemon hello timed out")), 5_000);
      const fail = (error: Error) => {
        clearTimeout(timer);
        rejectConnection(error);
      };
      socket.once("error", fail);
      socket.once("open", () => {
        const resume = this.cursor.resume();
        socket.send(JSON.stringify({
          type: "client_hello",
          protocol_version: PROTOCOL_VERSION,
          client_version: app.getVersion(),
          client_kind: "electron",
          client_instance_id: this.clientId,
          after_epoch: resume?.after_epoch,
          after_seq: resume?.after_seq,
        }));
      });
      socket.on("message", data => {
        let message: ControlMessage;
        try { message = JSON.parse(data.toString()) as ControlMessage; }
        catch { return; }
        if (message.type === "server_hello") {
          clearTimeout(timer);
          socket.off("error", fail);
          resolveConnection();
          return;
        }
        this.handleMessage(message);
      });
      socket.once("close", () => {
        clearTimeout(timer);
        rejectConnection(new Error("daemon closed during handshake"));
      });
    });

    socket.on("close", (code: number, reason: Buffer, wasClean: boolean) => {
      this.handleDisconnect(`daemon closed: code=${code} clean=${wasClean} reason="${(reason ?? "").toString()}"`);
    });
    socket.on("error", error => {
      if (socket.readyState !== WebSocket.OPEN) this.handleDisconnect(error.message);
    });
    this.startHeartbeat();
    this.setStatus({ connected: true });
    for (const seed of this.attached) await this.attachWire(seed);
  }

  /** 先探测 Ringing V1 open；成功时整个 daemon 连接固定走 Ringing，不回退到 legacy WS。 */
  private async tryConnectRinging(discovery: DaemonDiscovery): Promise<boolean> {
    const endpointUrl = new URL(discovery.endpoint);
    const baseUrl = `${endpointUrl.protocol === "wss:" ? "https" : "http"}://${endpointUrl.host}`;
    try {
      const response = await fetch(`${baseUrl}/ringing/v1/clients/open`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${discovery.token}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          schema: "deepx.Ringing",
          version: 1,
          client_instance_id: this.clientId,
          capabilities: [
            "Ringing_v1",
            "Ringing_batch_v1",
            "Ringing_bootstrap_v1",
            "Ringing_command_status_v1",
          ],
        }),
      });
      if (!response.ok) {
        // 只有明确表明 daemon 不支持 Ringing v1 的响应才允许选择
        // 兼容 legacy；认证、服务端故障和其他协议错误必须暴露出来。
        if (response.status === 404 || response.status === 426) return false;
        throw new Error(`Ringing v1 negotiation failed: HTTP ${response.status}`);
      }
      const result = await response.json() as {
        accepted?: boolean;
        client_session_id?: string;
        server_epoch?: string;
        lease_ttl_ms?: number;
        renew_interval_ms?: number;
      };
      if (result.accepted !== true) {
        throw new Error("Ringing v1 negotiation was not accepted by daemon");
      }
      if (
        typeof result.client_session_id !== "string" ||
        typeof result.server_epoch !== "string" ||
        typeof result.lease_ttl_ms !== "number" ||
        typeof result.renew_interval_ms !== "number"
      ) {
        throw new Error("Ringing v1 negotiation returned an incomplete session");
      }
      this.lastDiscovery = { baseUrl, token: discovery.token };
      this.ringingConnection = {
        baseUrl,
        token: discovery.token,
        session: {
          clientInstanceId: this.clientId,
          clientSessionId: result.client_session_id,
          serverEpoch: result.server_epoch,
          leaseTtlMs: result.lease_ttl_ms,
          renewIntervalMs: result.renew_interval_ms,
        },
      };
      this.transport = "ringing";
      this.setStatus({ connected: true });
      return true;
    } catch (error) {
      // 旧 daemon 的明确 404/426 在上方选择 legacy；网络超时、连接中断
      // 不能被伪装成旧协议，否则 Ringing V1 daemon 故障会悄悄建立 legacy 双链路。
      throw error;
    }
  }

  private handleMessage(message: ControlMessage): void {
    this.cursor.observe(message);
    const requestId = typeof message.request_id === "string" ? message.request_id : undefined;
    if (requestId && (message.type === "response" || message.type === "error" || message.type === "lease_denied")) {
      const pending = this.pending.get(requestId);
      if (pending) {
        clearTimeout(pending.timer);
        this.pending.delete(requestId);
        if (message.type === "response") pending.resolve(message.result);
        else pending.reject(new Error(`${String(message.code ?? message.type)}: ${String(message.message ?? "request failed")}`));
      }
    }
    // Expand bulk EventBatch into individual Event messages
    if (message.type === "event_batch") {
      const raw = message as unknown as { seed?: string; events?: unknown[] };
      const seed = String(raw.seed ?? "");
      const events = raw.events;
      if (Array.isArray(events)) {
        for (const event of events) {
          const synthetic: ControlMessage = { type: "event", seed, event };
          this.routeMessage(synthetic);
        }
      }
      return;
    }
    this.routeMessage(message);
  }

  private routeMessage(message: ControlMessage): void {
    // Streaming events must cross the main-process boundary immediately.
    // Frame-level coalescing belongs in the renderer, where it can track paint
    // cadence without adding an artificial 50 ms transport delay.
    this.onMessage(message);
  }

  private async attachWire(seed: string): Promise<unknown> {
    return this.roundTrip({ type: "session_attach", request_id: randomUUID(), seed });
  }

  private roundTrip(message: ControlMessage): Promise<unknown> {
    const requestId = String(message.request_id ?? "");
    if (!requestId || this.socket?.readyState !== WebSocket.OPEN) {
      const state = this.socket?.readyState ?? -1;
      const stateNames: Record<number, string> = { 0: "CONNECTING", 1: "OPEN", 2: "CLOSING", 3: "CLOSED" };
      return Promise.reject(new Error(`daemon not connected: socket state=${state} (${stateNames[state] ?? "UNKNOWN"})`));
    }
    return new Promise((resolveRequest, rejectRequest) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        rejectRequest(new Error("daemon request timed out"));
      }, REQUEST_TIMEOUT_MS);
      this.pending.set(requestId, { resolve: resolveRequest, reject: rejectRequest, timer });
      this.socket!.send(JSON.stringify(message), error => {
        if (!error) return;
        clearTimeout(timer);
        this.pending.delete(requestId);
        rejectRequest(error);
      });
    });
  }

  private startHeartbeat(): void {
    if (this.heartbeat) clearInterval(this.heartbeat);
    let nonce = 0;
    this.heartbeat = setInterval(() => {
      if (this.socket?.readyState === WebSocket.OPEN) {
        this.socket.send(JSON.stringify({ type: "heartbeat", nonce: ++nonce }));
      }
    }, 5_000);
  }

  private handleDisconnect(reason: string): void {
    if (this.socket?.readyState === WebSocket.OPEN) return;
    this.disconnectSocket();
    this.setStatus({ connected: false, error: reason });
    if (!this.stopped && !this.restarting && !this.reconnect) {
      this.reconnect = setTimeout(() => {
        this.reconnect = undefined;
        void this.connect().catch(() => this.handleDisconnect("daemon reconnect failed"));
      }, 1_000);
    }
  }

  private disconnectSocket(): void {
    if (this.heartbeat) clearInterval(this.heartbeat);
    this.heartbeat = undefined;
    const socket = this.socket;
    this.socket = undefined;
    if (socket && socket.readyState < WebSocket.CLOSING) socket.close();
    for (const [reqId, pending] of this.pending.entries()) {
      clearTimeout(pending.timer);
      pending.reject(new Error(`daemon disconnected: ${this.status.error || "connection dropped"} (pending request: ${reqId})`));
    }
    this.pending.clear();
  }

  private setStatus(status: BackendStatus): void {
    this.status = { ...status, transport: status.transport ?? this.transport };
    this.onStatus({ ...this.status });
  }
}

function deepxDataDir(): string {
  if (process.platform === "win32") return join(process.env.USERPROFILE || homedir(), ".deepx");
  return join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "deepx");
}

async function readDiscovery(): Promise<DaemonDiscovery> {
  return JSON.parse(await readFile(join(deepxDataDir(), "daemon.json"), "utf8")) as DaemonDiscovery;
}

async function expectedDaemonIdentity(): Promise<ExpectedDaemonIdentity> {
  if (!app.isPackaged) {
    return {
      protocol_version: PROTOCOL_VERSION,
      version: app.getVersion(),
      channel: "dev",
    };
  }
  const manifest = JSON.parse(
    await readFile(join(process.resourcesPath, "daemon-manifest.json"), "utf8"),
  ) as DaemonManifest;
  return {
    protocol_version: manifest.protocol_version,
    version: manifest.version,
    build_id: manifest.build_id,
    channel: manifest.channel,
  };
}

async function requestDaemonStop(
  discovery: DaemonDiscovery,
  idleOnly: boolean,
): Promise<"stopping" | "busy" | "unsupported"> {
  const url = new URL(discovery.endpoint.replace(/^ws:/, "http:"));
  url.pathname = idleOnly ? "/control/v1/stop-if-idle" : "/control/v1/stop";
  try {
    const response = await fetch(url, {
      method: "POST",
      headers: { Authorization: `Bearer ${discovery.token}` },
    });
    if (response.status === 200) return "stopping";
    if (response.status === 409) return "busy";
    return "unsupported";
  } catch {
    return "unsupported";
  }
}

async function waitForDaemonExit(pid: number): Promise<void> {
  const deadline = Date.now() + START_TIMEOUT_MS;
  while (Date.now() < deadline) {
    await delay(100);
    try {
      const discovery = await readDiscovery();
      if (discovery.pid !== pid) return;
    } catch {
      return;
    }
  }
  throw new Error("old daemon did not stop in time");
}

async function waitForCompatibleDiscovery(expected: ExpectedDaemonIdentity): Promise<DaemonDiscovery> {
  const deadline = Date.now() + START_TIMEOUT_MS;
  let mismatch = "daemon discovery unavailable";
  while (Date.now() < deadline) {
    await delay(120);
    try {
      const discovery = await readDiscovery();
      mismatch = daemonIdentityMismatch(discovery, expected) ?? "";
      if (!mismatch) return discovery;
    } catch {}
  }
  throw new Error(`replacement daemon did not start: ${mismatch}`);
}

function daemonPath(): string {
  const executable = process.platform === "win32" ? "deepx-daemon.exe" : "deepx-daemon";
  const developmentBackend = process.env.DEEPX_BACKEND_ROOT
    ? resolve(process.env.DEEPX_BACKEND_ROOT)
    : resolve(app.getAppPath(), "..", "DeepX");
  return app.isPackaged
    ? join(process.resourcesPath, executable)
    : join(developmentBackend, "target", "debug", executable);
}

function launchDaemon(): void {
  const child = spawn(daemonPath(), ["run"], {
    detached: true,
    windowsHide: true,
    stdio: "ignore",
  });
  child.unref();
}

function delay(ms: number): Promise<void> {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
