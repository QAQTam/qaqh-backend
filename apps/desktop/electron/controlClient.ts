import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { app } from "electron";
import WebSocket from "ws";
import { daemonIdentityMismatch, hasActiveDaemonWork, type ExpectedDaemonIdentity } from "../src/runtime/daemonLifecycle";
import { ControlEventBatcher } from "../src/runtime/controlEventBatcher";
import { ControlCursor } from "../src/runtime/controlCursor";
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

export class DaemonControlClient {
  private socket?: WebSocket;
  private connecting?: Promise<void>;
  private heartbeat?: NodeJS.Timeout;
  private reconnect?: NodeJS.Timeout;
  private upgradeCheck?: NodeJS.Timeout;
  private streamFlush?: NodeJS.Timeout;
  private readonly eventBatcher = new ControlEventBatcher();
  private readonly cursor = new ControlCursor();
  private readonly pending = new Map<string, Pending>();
  private readonly attached = new Set<string>();
  private readonly clientId = `electron-${randomUUID()}`;
  private stopped = false;
  private restarting = false;
  private closing?: Promise<void>;
  private status: BackendStatus = { connected: false };

  constructor(
    private readonly onMessage: (message: ControlMessage) => void,
    private readonly onStatus: (status: BackendStatus) => void,
  ) {}

  currentStatus(): BackendStatus {
    return { ...this.status };
  }

  async connect(): Promise<void> {
    if (this.socket?.readyState === WebSocket.OPEN) return;
    if (this.connecting) return this.connecting;
    this.stopped = false;
    this.connecting = this.connectOrLaunch().finally(() => { this.connecting = undefined; });
    return this.connecting;
  }

  async request(method: string, params: Record<string, unknown> = {}): Promise<unknown> {
    if (!method || typeof method !== "string") throw new Error("invalid backend method");
    await this.connect();
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
    const result = await this.attachWire(seed);
    this.attached.add(seed);
    return result;
  }

  async detach(seed: string): Promise<unknown> {
    if (!seed) throw new Error("session seed is required");
    await this.connect();
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
    this.closing = this.releaseLeases().finally(() => this.disconnectSocket());
    return this.closing;
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
      await this.connectDiscovery(discovery);
      const mismatch = daemonIdentityMismatch(discovery, expected);
      if (!mismatch) return;

      const activities = await this.roundTrip({
        type: "request",
        request_id: randomUUID(),
        method: "session.activity",
        params: {},
      });
      if (hasActiveDaemonWork(activities)) {
        this.setStatus({ connected: true, updatePending: true });
        this.scheduleUpgrade(discovery, expected);
        return;
      }
      await this.takeOverDaemon(discovery, expected, true);
      return;
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
        await this.connectDiscovery(discovery);
        return;
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
      await this.connectDiscovery(replacement);
      this.setStatus({ connected: true });
    } finally {
      this.restarting = false;
    }
  }

  private async connectDiscovery(discovery: DaemonDiscovery): Promise<void> {
    if (discovery.protocol_version !== PROTOCOL_VERSION) {
      throw new Error(`daemon protocol ${discovery.protocol_version} is incompatible`);
    }
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

    socket.on("close", () => this.handleDisconnect("daemon connection closed"));
    socket.on("error", error => {
      if (socket.readyState !== WebSocket.OPEN) this.handleDisconnect(error.message);
    });
    this.startHeartbeat();
    this.setStatus({ connected: true });
    for (const seed of this.attached) await this.attachWire(seed);
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
    const ready = this.eventBatcher.push(message);
    if (ready.length === 0) {
      this.scheduleStreamFlush();
      return;
    }
    for (const outgoing of ready) this.onMessage(outgoing);
  }

  private scheduleStreamFlush(): void {
    if (this.streamFlush) return;
    this.streamFlush = setTimeout(() => {
      this.streamFlush = undefined;
      for (const message of this.eventBatcher.flush()) this.onMessage(message);
    }, 16);
  }

  private async attachWire(seed: string): Promise<unknown> {
    return this.roundTrip({ type: "session_attach", request_id: randomUUID(), seed });
  }

  private roundTrip(message: ControlMessage): Promise<unknown> {
    const requestId = String(message.request_id ?? "");
    if (!requestId || this.socket?.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("daemon disconnected"));
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
    if (this.streamFlush) clearTimeout(this.streamFlush);
    this.streamFlush = undefined;
    for (const message of this.eventBatcher.flush()) this.onMessage(message);
    const socket = this.socket;
    this.socket = undefined;
    if (socket && socket.readyState < WebSocket.CLOSING) socket.close();
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error("daemon disconnected"));
    }
    this.pending.clear();
  }

  private setStatus(status: BackendStatus): void {
    this.status = status;
    this.onStatus({ ...status });
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
