import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { app } from "electron";
import { daemonIdentityMismatch, type ExpectedDaemonIdentity } from "../src/runtime/daemonLifecycle";
import type { RingingSessionOpen } from "./ringingClient";
import type { BackendStatus, DaemonDiscovery, DaemonManifest } from "./types";

const PROTOCOL_VERSION = 1;
const START_TIMEOUT_MS = 8_000;

export interface RingingConnectionInfo {
  baseUrl: string;
  token: string;
  session: RingingSessionOpen;
}

/**
 * Daemon 控制客户端（Ringing V1 only）。
 *
 * legacy `/control/v1` WebSocket 数据协议已随 M3 拆除：连接协商固定走
 * `POST /ringing/v1/clients/open`（lease + 身份），seed 挂载为纯本地记账
 * （attach/detach 不再有 wire 往返，会话归属由 daemon Ringing lease 侧
 * 的 session.resume / session.close 命令建立）。生命周期端点
 * `POST /control/v1/stop` 仅用于优雅停止/更新接管，不属于数据协议。
 */
export class DaemonControlClient {
  private connecting?: Promise<void>;
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
    private readonly onStatus: (status: BackendStatus) => void,
  ) {}

  currentStatus(): BackendStatus {
    return { ...this.status };
  }

  /** 连接固定为 Ringing；legacy WS 路径已移除。 */
  usingRinging(): boolean {
    return this.transport === "ringing" && this.status.connected;
  }

  async connect(): Promise<void> {
    if (this.usingRinging()) return;
    if (this.connecting) return this.connecting;
    this.stopped = false;
    this.connecting = this.connectOrLaunch().finally(() => { this.connecting = undefined; });
    return this.connecting;
  }

  async attach(seed: string): Promise<unknown> {
    if (!seed) throw new Error("session seed is required");
    await this.connect();
    this.attached.add(seed);
    return { ok: true, transport: "ringing" };
  }

  async detach(seed: string): Promise<unknown> {
    if (!seed) throw new Error("session seed is required");
    await this.connect();
    this.attached.delete(seed);
    return { ok: true, transport: "ringing" };
  }

  close(): Promise<void> {
    if (this.closing) return this.closing;
    this.stopped = true;
    this.attached.clear();
    this.closing = Promise.resolve().then(() => {
      this.transport = "legacy";
      this.ringingConnection = undefined;
      this.setStatus({ connected: false });
    });
    return this.closing;
  }

  /**
   * 优雅停止 daemon 并等待其进程退出（POST /control/v1/stop）。
   */
  async stopDaemon(): Promise<boolean> {
    this.stopped = true;
    try {
      const discovery = await readDiscovery();
      const stopped = await requestDaemonStop(discovery, false);
      if (stopped !== "stopping") {
        console.warn("[backend] daemon did not acknowledge graceful stop:", stopped);
        return false;
      }
      await waitForDaemonExit(discovery.pid);
      return true;
    } catch (error) {
      console.warn("[backend] graceful daemon stop failed", error);
      return false;
    }
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
      // Ringing 模式下 attach 是纯本地记账；会话归属由 main 侧的
      // reattachSeedAfterRecovery（session.resume 命令）重建。
      this.updateReattach = [];
      this.setStatus({ connected: true });
    } finally {
      this.restarting = false;
    }
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

  /** Ringing V1 open（唯一入口）。失败即抛错，绝不回退 legacy WS。 */
  private async tryConnectRinging(discovery: DaemonDiscovery): Promise<boolean> {
    const endpointUrl = new URL(discovery.endpoint);
    const baseUrl = `${endpointUrl.protocol === "wss:" ? "https" : "http"}://${endpointUrl.host}`;
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
      // daemon 不支持 Ringing v1（旧版）时视为不可用：统一协议要求下
      // 不提供 legacy 兼容通道。
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
  return new Promise(resolve => setTimeout(resolve, ms));
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
