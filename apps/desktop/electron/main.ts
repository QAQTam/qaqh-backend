import { dirname, join, resolve, sep } from "node:path";
import { execFile, spawn } from "node:child_process";
import { deflateSync } from "node:zlib";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { app, BrowserWindow, dialog, ipcMain, Menu, nativeImage, shell, Tray } from "electron";
import { DaemonControlClient } from "./controlClient";
import { RingingManager, type ChannelName } from "./ringingManager";
import type { ConfirmDialogOptions, OpenDialogOptions, UpdateInfo } from "./types";
import {
  RINGING_COMMAND_METHODS,
  RINGING_QUERY_METHODS,
  buildRingingCommandEnvelope,
} from "../src/runtime/ringingCommandRouter";

let mainWindow: BrowserWindow | undefined;
let quitting = false;
let tray: Tray | undefined;
let petProcess: ReturnType<typeof spawn> | undefined;
let petEnabled = false;
let lastPendingOperation = "";
let lastResumedSeed: string | null = null;
let updatePoll: ReturnType<typeof setInterval> | undefined;
let resolveInitialRenderer!: () => void;
const initialRendererReady = new Promise<void>(resolveReady => {
  resolveInitialRenderer = resolveReady;
});

function getClawdPath(): string {
  // 1. 硬编码测试路径（临时，后续打包时去掉）
  const hardcoded = "D:\\clawd-on-desk";
  if (require("fs").existsSync(join(hardcoded, "launch.js"))) return hardcoded;

  // 2. 开发: 项目树同级目录
  if (!app.isPackaged) {
    const dev = join(app.getAppPath(), "..", "..", "clawd-on-desk");
    if (require("fs").existsSync(join(dev, "launch.js"))) return dev;
  }

  // 3. 生产: extraResources 中的 clawd-on-desk
  return join(process.resourcesPath, "clawd-on-desk");
}

function findNodeBin(): string {
  // 1. 系统 Node.js 安装路径（无需 execSync，直接 stat）
  const candidates = [
    join(process.env.ProgramFiles || "C:\\Program Files", "nodejs", "node.exe"),
    join(process.env["ProgramFiles(x86)"] || "C:\\Program Files (x86)", "nodejs", "node.exe"),
    join(process.env.USERPROFILE || "", "AppData", "Roaming", "nvm", process.arch === "x64" ? "v24.18.0" : "v24.18.0", "node.exe"),
    process.execPath,  // dev Electron = runs JS fine
    "node",          // 最后的裸名兜底
  ];
  for (const c of candidates) {
    try { require("fs").accessSync(c, require("fs").constants.X_OK); return c; } catch { /* next */ }
  }
  return "node";
}

function launchPet(): void {
  if (petProcess) return;
  const clawdDir = getClawdPath();
  const launchJs = join(clawdDir, "launch.js");
  try { require("fs").accessSync(launchJs); }
  catch {
    console.error("[pet] clawd-on-desk not found at", clawdDir);
    return;
  }
  const nodeBin = findNodeBin();
  console.log("[pet] launching clawd-on-desk with", nodeBin);
  petProcess = spawn(nodeBin, [launchJs], {
    cwd: clawdDir,
    detached: true,
    windowsHide: false,
    stdio: "ignore",
  });
  petProcess.on("exit", () => { petProcess = undefined; });
  petEnabled = true;
}

function killPet(): void {
  if (!petProcess) return;
  try {
    // Windows: taskkill 子进程树
    if (process.platform === "win32") {
      spawn("taskkill", ["/pid", String(petProcess.pid), "/f", "/t"], { windowsHide: true });
    } else {
      petProcess.kill("SIGTERM");
    }
  } catch { /* ignore */ }
  petProcess = undefined;
  petEnabled = false;
}
const smokeMode = process.env.DEEPX_DESKTOP_SMOKE === "1" || process.argv.includes("--deepx-smoke");
const backend = new DaemonControlClient(
  message => sendToRenderer("backend:message", message),
  status => sendToRenderer("backend:status", status),
);
// Ringing 会话管理（三 SSE）。batch 整批转发 renderer，禁止逐事件展开。
const ringing = new RingingManager(
  batch => sendToRenderer("ringing:batch", batch),
  (channel, status) => sendToRenderer("ringing:status", { channel, status }),
  (seed, channel, snapshot) =>
    sendToRenderer("ringing:snapshot", { seed, channel, snapshot }),
  async () => {
    const info = await backend.refreshRingingConnection();
    await ringing.ensureConnected(info.baseUrl, info.token, info.session);
  },
  (seed, entry) => sendToRenderer("timeline:entry", { seed, entry }),
  status => sendToRenderer("timeline:status", status),
  snapshot => sendToRenderer("timeline:snapshot", snapshot),
);

if (smokeMode) {
  setTimeout(() => {
    void backend.close();
    console.error("Electron smoke test timed out before the preload/backend bridge was ready");
    app.exit(1);
  }, 30_000);
}

// ── Tray / graceful shutdown ────────────────────────────
//
// 关闭行为：点窗口关闭按钮 → 弹窗选择「最小化到托盘」/「完全退出」/「取消」。
// 完全退出时先优雅停止 daemon（POST /control/v1/stop + 等待进程退出），
// 避免 Electron 退出后 daemon 变成孤儿进程。

// 运行时生成 32×32 RGBA PNG 托盘图标（聊天气泡样式），不依赖外部图标资源。
const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(buf: Buffer): number {
  let c = 0xffffffff;
  for (const byte of buf) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function pngChunk(type: string, data: Buffer): Buffer {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const typeBuf = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])));
  return Buffer.concat([len, typeBuf, data, crc]);
}

function encodePng(width: number, height: number, rgba: Buffer): Buffer {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type: RGBA
  const stride = width * 4 + 1;
  const raw = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y++) {
    raw[y * stride] = 0; // filter: none
    rgba.copy(raw, y * stride + 1, y * width * 4, (y + 1) * width * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", deflateSync(raw)),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

function createTrayIcon() {
  const size = 32;
  const rgba = Buffer.alloc(size * size * 4);
  const cx = 15.5;
  const cy = 15.5;
  const inRoundedRect = (x: number, y: number) => {
    const half = 14;
    const radius = 5;
    const dx = Math.max(Math.abs(x - cx) - (half - radius), 0);
    const dy = Math.max(Math.abs(y - cy) - (half - radius), 0);
    return dx * dx + dy * dy <= radius * radius;
  };
  const blue = [0x2f, 0x6f, 0xed, 255];
  const white = [255, 255, 255, 255];
  const dotBlue = [0x2f, 0x6f, 0xed, 255];
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4;
      if (!inRoundedRect(x + 0.5, y + 0.5)) continue;
      rgba[i] = blue[0]; rgba[i + 1] = blue[1]; rgba[i + 2] = blue[2]; rgba[i + 3] = blue[3];
      // 白色气泡
      if (Math.hypot(x + 0.5 - cx, y + 0.5 - cy - 1.5) <= 9.5) {
        rgba[i] = white[0]; rgba[i + 1] = white[1]; rgba[i + 2] = white[2]; rgba[i + 3] = white[3];
      }
    }
  }
  // 气泡内三圆点（聊天气泡）
  for (const [dotX, dotY] of [[12, 16.5], [16, 16.5], [20, 16.5]] as const) {
    for (let y = 0; y < size; y++) {
      for (let x = 0; x < size; x++) {
        if (Math.hypot(x + 0.5 - dotX, y + 0.5 - dotY) <= 2) {
          const i = (y * size + x) * 4;
          rgba[i] = dotBlue[0]; rgba[i + 1] = dotBlue[1]; rgba[i + 2] = dotBlue[2]; rgba[i + 3] = dotBlue[3];
        }
      }
    }
  }
  return nativeImage.createFromBuffer(encodePng(size, size, rgba));
}

function createTray(): void {
  if (tray) return;
  tray = new Tray(createTrayIcon());
  tray.setToolTip("DeepX");
  tray.setContextMenu(Menu.buildFromTemplate([
    { label: "显示 DeepX", click: () => showMainWindow() },
    { type: "separator" },
    { label: "退出", click: () => { void quitDeepX(); } },
  ]));
  tray.on("click", () => showMainWindow());
}

function showMainWindow(): void {
  if (!mainWindow || mainWindow.isDestroyed()) {
    createWindow();
    return;
  }
  if (mainWindow.isMinimized()) mainWindow.restore();
  mainWindow.show();
  mainWindow.focus();
}

function minimizeToTray(): void {
  createTray();
  mainWindow?.hide();
}

/** 完全退出：先优雅停止 daemon，再关闭 Ringing/控制连接，最后退出应用。 */
async function quitDeepX(): Promise<void> {
  if (quitting) return;
  quitting = true;
  // 先停 daemon（Electron 与 daemon 是分开的进程，直接退出会把 daemon 留成孤儿）。
  await backend.stopDaemon();
  ringing.close();
  void backend.close().finally(() => app.quit());
}

function onWindowClose(event: { preventDefault: () => void }): void {
  if (quitting || smokeMode) return;
  event.preventDefault();
  void dialog.showMessageBox(mainWindow!, {
    type: "question",
    title: "关闭 DeepX",
    message: "DeepX 正在后台运行。",
    detail: "选择关闭后的行为：",
    buttons: ["最小化到托盘", "完全退出", "取消"],
    defaultId: 0,
    cancelId: 2,
    noLink: true,
  }).then(({ response }) => {
    if (response === 0) {
      minimizeToTray();
    } else if (response === 1) {
      void quitDeepX();
    }
    // response === 2: 取消，保持窗口打开
  }).catch(() => {});
}

function createWindow(): void {
  mainWindow = new BrowserWindow({
    title: "DeepX",
    width: 1200,
    height: 850,
    minWidth: 900,
    minHeight: 600,
    show: false,
    // Keep the Windows-managed caption buttons and drag area outside the
    // renderer process. They remain usable when the web UI is unresponsive.
    frame: true,
    webPreferences: {
      preload: join(__dirname, "../preload/preload.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
    },
  });
  mainWindow.webContents.on("preload-error", (_event, preloadPath, error) => {
    console.error(`Failed to load preload ${preloadPath}:`, error);
  });
  // This runs in Electron's main process, not in the renderer. Together with
  // the native frame it gives users a recovery path even if the web UI freezes.
  mainWindow.webContents.on("unresponsive", () => {
    void dialog.showMessageBox(mainWindow!, {
      type: "warning",
      title: "DeepX 无响应",
      message: "界面暂时没有响应。你可以等待，或重新加载界面。",
      buttons: ["重新加载", "等待"],
      defaultId: 0,
      cancelId: 1,
      noLink: true,
    }).then(({ response }) => {
      if (response !== 0 || !mainWindow || mainWindow.isDestroyed()) return;
      mainWindow.webContents.forcefullyCrashRenderer();
      mainWindow.webContents.reload();
    }).catch(() => {});
  });
  mainWindow.webContents.once("did-finish-load", () => resolveInitialRenderer());
  if (smokeMode) {
    mainWindow.webContents.once("did-finish-load", async () => {
      const bridgeReady = await mainWindow?.webContents.executeJavaScript(
        "Boolean(window.deepx?.backend && window.deepx?.desktop)",
      );
      let backendReady = false;
      if (bridgeReady) {
        try {
          await backend.connect();
          backendReady = backend.currentStatus().connected;
        } catch (error) {
          console.error("Electron backend lifecycle smoke test failed:", error);
        }
      }
      await backend.close();
      if (!bridgeReady) console.error("Electron preload bridge was not exposed to the renderer");
      if (!backendReady) console.error("Electron could not connect to a compatible daemon");
      app.exit(bridgeReady && backendReady ? 0 : 1);
    });
  }
  if (!smokeMode) mainWindow.once("ready-to-show", () => mainWindow?.show());
  // 关闭按钮 → 弹窗选择（最小化到托盘 / 完全退出 / 取消）
  mainWindow.on("close", onWindowClose);
  mainWindow.webContents.setWindowOpenHandler(({ url }) => {
    if (url.startsWith("https://") || url.startsWith("http://")) void shell.openExternal(url);
    return { action: "deny" };
  });
  mainWindow.webContents.on("will-navigate", event => event.preventDefault());

  if (process.env.ELECTRON_RENDERER_URL) void mainWindow.loadURL(process.env.ELECTRON_RENDERER_URL);
  else void mainWindow.loadFile(join(__dirname, "../renderer/index.html"));
}

function registerIpc(): void {
  // Ringing 会话惰性确保：daemon 重启/端口变化后，任何 ringing IPC 都会用
  // 最新 discovery 重新建立连接（旧逻辑只在 backend:connect 时连一次；
  // 连接失败或 daemon 重启后 client 一直为 null，需重新建立 Ringing V1 client。
  async function ensureRingingConnected(): Promise<void> {
    // 首个 renderer 请求可能早于显式 backend:connect。必须先确定连接级
    // 传输，不能因 status 仍是初始值而误走 legacy 请求。
    try {
      await backend.connect();
    } catch (error) {
      // The first request can fail before RingingManager.query is reached, so
      // queryRingingWithRecovery cannot catch this transport failure itself.
      if (!isRingingFetchFailure(error)) throw error;
      try {
        await recoverRingingConnection();
      } catch (recoveryError) {
        throw new Error(`Ringing connection failed during initial connect: ${ringingErrorMessage(recoveryError)}`);
      }
      return;
    }
    const info = backend.ringingConnectionInfo();
    if (!info || !backend.usingRinging()) {
      throw new Error("Ringing v1 is required but the daemon did not establish its HTTP session");
    }
    // daemon 重启后端口/token 会变：即使 client 还在（流已死），也要重建
    if (ringing.connected() && ringing.connectedBaseUrl() === info.baseUrl) return;
    await ringing.ensureConnected(info.baseUrl, info.token, info.session);
    await reattachSeedAfterRecovery();
  }

  let ringingRecovery: Promise<void> | null = null;

  function isRingingFetchFailure(error: unknown): boolean {
    return error instanceof TypeError && /fetch failed/i.test(error.message);
  }

  function ringingErrorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
  }

  // The daemon's seed leases are in-memory and die with the daemon process.
  // A reconnected Ringing client holds a fresh lease but no seed attachments,
  // so every command for that seed is rejected with lease_required until
  // session.resume re-attaches it. Re-dispatch the last resumed seed after
  // any connection recovery; get_or_spawn makes this idempotent.
  async function reattachSeedAfterRecovery(): Promise<void> {
    const seed = lastResumedSeed;
    if (!seed) return;
    try {
      await requestSelectedBackend("session.resume", { seed });
    } catch (error) {
      console.warn("[ringing] seed re-attach after recovery failed:", error);
    }
  }

  async function recoverRingingConnection(): Promise<void> {
    if (ringingRecovery) return ringingRecovery;
    ringingRecovery = (async () => {
      // The manager can still contain a client after its HTTP/SSE transport has
      // died. Stop those streams before re-reading discovery and opening a new
      // lease; otherwise a stale client keeps making ensureConnected a no-op.
      ringing.close();
      if (!backend.usingRinging() || !backend.ringingConnectionInfo()) {
        await backend.close();
        await backend.connect();
        const info = backend.ringingConnectionInfo();
        if (!info || !backend.usingRinging()) {
          throw new Error("Ringing recovery could not establish a daemon session");
        }
        await ringing.ensureConnected(info.baseUrl, info.token, info.session);
        await reattachSeedAfterRecovery();
        return;
      }
      try {
        const info = await backend.refreshRingingConnection();
        await ringing.ensureConnected(info.baseUrl, info.token, info.session);
        await reattachSeedAfterRecovery();
      } catch (error) {
        if (!isRingingFetchFailure(error)) throw error;

        // The control client can be stale too. Reset both transports so the
        // next connect reads daemon.json again instead of reusing old state.
        await backend.close();
        await backend.connect();
        const info = backend.ringingConnectionInfo();
        if (!info || !backend.usingRinging()) {
          throw new Error("Ringing recovery could not establish a daemon session");
        }
        await ringing.ensureConnected(info.baseUrl, info.token, info.session);
        await reattachSeedAfterRecovery();
      }
    })().finally(() => {
      ringingRecovery = null;
    });
    return ringingRecovery;
  }

  async function queryRingingWithRecovery(
    path: string,
    params?: Record<string, string | undefined>,
  ): Promise<unknown> {
    try {
      return await ringing.query(path, params);
    } catch (error) {
      // Queries are idempotent. A single retry is safe after refreshing daemon
      // discovery, and fixes stale baseUrl/token state after daemon restart.
      if (!isRingingFetchFailure(error)) throw error;
      console.warn("[ringing] query transport failed; refreshing connection", error);
      try {
        await recoverRingingConnection();
      } catch (recoveryError) {
        throw new Error(`Ringing query recovery failed: ${ringingErrorMessage(recoveryError)}`);
      }
      return ringing.query(path, params);
    }
  }

  async function requestSelectedBackend(
    method: string,
    params: Record<string, unknown>,
  ): Promise<unknown> {
    await ensureRingingConnected();
    if (!ringing.connected()) throw new Error("Ringing v1 backend is not connected");

    const seed = typeof params.seed === "string" ? params.seed : "";
    if (method === "session.resume" && seed) lastResumedSeed = seed;
    const spec = RINGING_COMMAND_METHODS[method];
    if (spec) {
      let command = spec.build(params);
      // Local file paths never cross the Ringing wire. Electron main owns the
      // read/upload step and sends only ContentRef values to the daemon.
      if (method === "session.send_message" && command && Array.isArray(params.files)) {
        const files = params.files.filter((file): file is string => typeof file === "string");
        if (files.length > 0) {
          const attachments = [] as unknown[];
          for (const filePath of files) {
            const bytes = await readFile(filePath);
            const mediaType = mimeTypeForPath(filePath);
            attachments.push(await ringing.uploadContent(seed, mediaType, new Uint8Array(bytes)));
          }
          command = { ...command, attachments };
        }
      }
      if (command) {
        return ringing.command(
          seed,
          spec.channel,
          buildRingingCommandEnvelope(
            seed,
            spec.channel,
            command,
            params.expectedRevision ?? params.expected_revision,
          ),
        );
      }
    }
    if (RINGING_QUERY_METHODS.has(method)) {
      const queryParams: Record<string, string | undefined> = {};
      for (const [key, value] of Object.entries(params)) {
        if (value === undefined || value === null) continue;
        if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
          queryParams[key] = String(value);
        }
      }
      return queryRingingWithRecovery(method, queryParams);
    }
    return ringing.action(method, params);
  }

  ipcMain.handle("backend:connect", async () => {
    const result = await backend.connect();
    // 控制客户端已完成唯一的 Ringing V1 open；RingingManager 复用其 lease 启动三 SSE。
    await ensureRingingConnected();
    return result;
  });
  ipcMain.handle("backend:request", (_event, method: unknown, params: unknown) => {
    if (typeof method !== "string" || !isRecord(params)) throw new Error("invalid backend request");
    return requestSelectedBackend(method, params);
  });
  ipcMain.handle("backend:attach", (_event, seed: unknown) => backend.attach(requireSeed(seed)));
  ipcMain.handle("backend:detach", (_event, seed: unknown) => backend.detach(requireSeed(seed)));
  ipcMain.handle("backend:status", () => backend.currentStatus());
  ipcMain.handle("backend:restart", async () => {
    // 运行环境切换（workspace.set_mode 已写 config）后重启 daemon：
    // 复用更新链路的 prepare/resume（停 daemon → 等退出 → 重连拉起）。
    try {
      if (!(await backend.prepareBackendUpdate())) {
        return { ok: false, reason: "busy" };
      }
      await backend.resumeAfterBackendUpdate();
      return { ok: true };
    } catch (error) {
      return { ok: false, reason: String(error) };
    }
  });
  ipcMain.handle("ringing:status", async () => {
    await ensureRingingConnected();
    return ringing.status();
  });
  ipcMain.handle("ringing:snapshot", async (_event, seed: unknown, channel: unknown) => {
    await ensureRingingConnected();
    if (!["control", "conversation", "tool"].includes(String(channel))) {
      throw new Error("invalid ringing channel");
    }
    return ringing.snapshot(requireSeed(seed), String(channel) as ChannelName);
  });
  ipcMain.handle("ringing:bootstrap", async (_event, seed: unknown) => {
    await ensureRingingConnected();
    return ringing.bootstrapSession(requireSeed(seed));
  });
  ipcMain.handle("timeline:activate", async (_event, seed: unknown) => {
    await ensureRingingConnected();
    return ringing.activateTimeline(requireSeed(seed));
  });
  ipcMain.handle("timeline:status", async () => {
    await ensureRingingConnected();
    return ringing.timelineConnectionStatus();
  });
  ipcMain.handle("ringing:command", async (_event, seed: unknown, channel: unknown, envelope: unknown) => {
    await ensureRingingConnected();
    if (!["control", "conversation", "tool"].includes(String(channel))) {
      throw new Error("invalid ringing channel");
    }
    if (
      !isRecord(envelope) ||
      typeof envelope.command_id !== "string" ||
      !isRecord(envelope.command)
  ) {
      throw new Error("invalid ringing command envelope");
    }
    // SessionCreate is the only registry command that is valid before a
    // session seed exists. RingingManager and the daemon both represent this
    // as a null seed; all other commands must retain the strict seed check.
    const isUnseededSessionCreate =
      String(channel) === "control" && envelope.command.type === "session_create";
    return ringing.command(
      isUnseededSessionCreate ? "" : requireSeed(seed),
      String(channel) as ChannelName,
      {
        command_id: envelope.command_id,
        command: envelope.command,
        seed: typeof envelope.seed === "string" ? envelope.seed : undefined,
        expected_revision:
          typeof envelope.expected_revision === "number" ? envelope.expected_revision : undefined,
        client_instance_id:
          typeof envelope.client_instance_id === "string"
            ? envelope.client_instance_id
            : undefined,
      },
    );
  });
  ipcMain.handle("ringing:query", async (_event, path: unknown, params: unknown) => {
    await ensureRingingConnected();
    if (typeof path !== "string" || !/^[a-zA-Z0-9._/-]+$/.test(path)) {
      throw new Error("invalid ringing query path");
    }
    const safeParams: Record<string, string | undefined> | undefined = isRecord(params)
      ? Object.fromEntries(
          Object.entries(params).filter(
            (entry): entry is [string, string | undefined] =>
              entry[1] === undefined || typeof entry[1] === "string",
          ),
        )
      : undefined;
    return queryRingingWithRecovery(path, safeParams);
  });
  ipcMain.handle("desktop:open-devtools", () => {
    if (!mainWindow || mainWindow.isDestroyed()) return false;
    mainWindow.webContents.openDevTools({ mode: "detach" });
    return true;
  });
  ipcMain.handle("desktop:toggle-pet", async () => {
    console.log("[main] toggle-pet called, petProcess:", !!petProcess);
    try {
      if (petProcess) {
        killPet();
      } else {
        launchPet();
      }
      console.log("[main] toggle-pet result:", petEnabled);
      return petEnabled;
    } catch (err) {
      console.error("[main] toggle-pet error:", err);
      return false;
    }
  });
  ipcMain.handle("desktop:pet-status", () => {
    console.log("[main] pet-status:", petEnabled);
    return petEnabled;
  });
  ipcMain.handle("desktop:set-background-material", (_event, material: unknown) => {
    if (!mainWindow || mainWindow.isDestroyed()) return false;
    if (material !== "auto" && material !== "mica" && material !== "acrylic" && material !== "none") {
      console.warn("[main] invalid backgroundMaterial:", material);
      return false;
    }
    try {
      mainWindow.setBackgroundMaterial(material as "auto" | "mica" | "acrylic" | "none");
      return true;
    } catch (err) {
      console.error("[main] setBackgroundMaterial failed:", err);
      return false;
    }
  });
  ipcMain.handle("desktop:open-dialog", async (_event, raw: OpenDialogOptions = {}) => {
    const options = isRecord(raw) ? raw : {};
    const result = await dialog.showOpenDialog(mainWindow!, {
      title: typeof options.title === "string" ? options.title : undefined,
      properties: [options.directory ? "openDirectory" : "openFile", ...(options.multiple ? ["multiSelections" as const] : [])],
    });
    if (result.canceled) return null;
    return options.multiple ? result.filePaths : (result.filePaths[0] ?? null);
  });
  ipcMain.handle("desktop:open-image-dialog", async () => {
    const result = await dialog.showOpenDialog(mainWindow!, {
      title: "选择图片",
      properties: ["openFile"],
      filters: [
        { name: "图片文件", extensions: ["png", "jpg", "jpeg", "gif", "webp", "bmp"] },
        { name: "所有文件", extensions: ["*"] },
      ],
    });
    if (result.canceled) return null;
    return result.filePaths[0] ?? null;
  });
  ipcMain.handle("desktop:read-file-base64", async (_event, filePath: unknown) => {
    if (typeof filePath !== "string" || !filePath) throw new Error("file path is required");
    const buffer = await readFile(filePath);
    const base64 = buffer.toString("base64");
    const ext = filePath.split(".").pop()?.toLowerCase() ?? "png";
    const mimeMap: Record<string, string> = {
      png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg",
      gif: "image/gif", webp: "image/webp", bmp: "image/bmp",
    };
    const mimeType = mimeMap[ext] ?? "image/png";
    return { mimeType, data: base64, size: buffer.length };
  });
  ipcMain.handle("desktop:read-text-file", async (_event, filePath: unknown) => {
    if (typeof filePath !== "string" || !filePath) throw new Error("file path is required");
    const content = await readFile(filePath, "utf-8");
    return { content, size: Buffer.byteLength(content, "utf-8") };
  });
  ipcMain.handle("desktop:confirm", async (_event, message: unknown, raw: ConfirmDialogOptions = {}) => {
    if (typeof message !== "string") throw new Error("invalid confirmation message");
    const options = isRecord(raw) ? raw : {};
    const result = await dialog.showMessageBox(mainWindow!, {
      type: options.kind === "error" || options.kind === "warning" ? options.kind : "info",
      title: typeof options.title === "string" ? options.title : "DeepX",
      message,
      buttons: ["OK"],
    });
    return result.response === 0;
  });
  ipcMain.handle("desktop:open-path", async (_event, target: unknown) => {
    if (typeof target !== "string" || !target) throw new Error("invalid path");
    if (/^https?:\/\//i.test(target)) {
      await shell.openExternal(target);
      return;
    }
    const error = await shell.openPath(target);
    if (error) throw new Error(error);
  });
  ipcMain.handle("desktop:check-update", async () => checkForUpdates());
  ipcMain.handle("desktop:stage-update", async (_event, source: unknown) => {
    if (typeof source !== "string" || !source) throw new Error("update source directory is required");
    await runUpdater(["stage", source, installRoot()]);
    return checkForUpdates();
  });
  ipcMain.handle("desktop:apply-update", async (_event, operationPath: unknown) => {
    if (typeof operationPath !== "string" || !isSafeOperationPath(operationPath)) {
      throw new Error("invalid staged update operation");
    }
    const operation = JSON.parse(await readFile(operationPath, "utf8")) as {
      plan?: { actions?: string[] };
    };
    const actions = operation.plan?.actions ?? [];
    const backendOnly = actions.includes("applyBackend")
      && !actions.includes("restartElectron")
      && !actions.includes("applyFull");
    if (backendOnly) {
      if (!await backend.prepareBackendUpdate()) {
        throw new Error("backend is busy; the update remains staged");
      }
      try {
        await runUpdater(["apply-staged", operationPath, installRoot()]);
        await backend.resumeAfterBackendUpdate();
      } catch (error) {
        try {
          await runUpdater(["rollback-staged", operationPath, installRoot()]);
          await backend.resumeAfterBackendUpdate();
        } catch (rollbackError) {
          throw new Error(`backend update failed (${String(error)}); rollback also failed (${String(rollbackError)})`);
        }
        throw error;
      }
      return { restarting: false };
    }

    await runUpdater(["handoff", operationPath, installRoot(), String(process.pid), process.execPath]);
    await backend.close();
    quitting = true;
    app.quit();
    return { restarting: true };
  });
}

function sendToRenderer(channel: string, payload: unknown): void {
  if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send(channel, payload);
}

function requireSeed(value: unknown): string {
  if (typeof value !== "string" || !value) throw new Error("session seed is required");
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function mimeTypeForPath(filePath: string): string {
  const extension = filePath.split(".").pop()?.toLowerCase() ?? "";
  const types: Record<string, string> = {
    txt: "text/plain",
    md: "text/markdown",
    json: "application/json",
    csv: "text/csv",
    png: "image/png",
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    gif: "image/gif",
    webp: "image/webp",
    pdf: "application/pdf",
  };
  return types[extension] ?? "application/octet-stream";
}

// ── Installer/updater handoff ──────────────────────────

async function checkForUpdates(): Promise<UpdateInfo | null> {
  try {
    const data = JSON.parse(await readFile(
      join(installRoot(), ".deepx-update", "pending.json"),
      "utf8",
    )) as {
      releaseId?: string;
      operationPath?: string;
      operationId?: string;
      mode?: UpdateInfo["mode"];
      artifacts?: string[];
      actions?: string[];
    };
    if (!data.releaseId || !data.operationPath || !isSafeOperationPath(data.operationPath)) return null;
    return {
      version: data.releaseId,
      operationPath: data.operationPath,
      operationId: data.operationId,
      mode: data.mode,
      artifacts: data.artifacts,
      actions: data.actions,
    };
  } catch {
    return null;
  }
}

function installRoot(): string {
  return dirname(process.execPath);
}

function updaterPath(): string {
  return join(installRoot(), process.platform === "win32" ? "deepx-updater.exe" : "deepx-updater");
}

function isSafeOperationPath(value: string): boolean {
  const stagingRoot = resolve(installRoot(), ".deepx-update", "staging") + sep;
  const operation = resolve(value);
  return operation.startsWith(stagingRoot) && operation.endsWith(`${sep}operation.json`);
}

function runUpdater(args: string[]): Promise<string> {
  return new Promise((resolveRun, reject) => {
    execFile(updaterPath(), args, { windowsHide: true }, (error, stdout, stderr) => {
      if (error) {
        reject(new Error(stderr.trim() || error.message));
        return;
      }
      resolveRun(stdout);
    });
  });
}

async function publishPendingUpdate(): Promise<void> {
  const update = await checkForUpdates();
  if (!update?.operationId || update.operationId === lastPendingOperation) return;
  lastPendingOperation = update.operationId;
  sendToRenderer("update:available", update);
}

function commandArgument(name: string): string | undefined {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

async function acknowledgeUpdateHealth(backendReady: Promise<void>): Promise<void> {
  const operationId = commandArgument("--deepx-update-operation");
  if (!operationId || !/^[A-Za-z0-9._+-]{1,240}$/.test(operationId)) return;
  try {
    await Promise.all([initialRendererReady, backendReady]);
    const healthDir = join(installRoot(), ".deepx-update", "health");
    await mkdir(healthDir, { recursive: true });
    await writeFile(join(healthDir, `${operationId}.ok`), "healthy\n", "utf8");
  } catch (error) {
    console.error("update health confirmation failed", error);
  }
}

async function publishRollbackNotice(): Promise<void> {
  const operationId = commandArgument("--deepx-update-rollback");
  if (!operationId) return;
  await initialRendererReady;
  sendToRenderer("update:failed", {
    operationId,
    message: "The updated application did not become healthy; the previous version was restored.",
  });
}

app.whenReady().then(() => {
  Menu.setApplicationMenu(null);
  registerIpc();
  createWindow();
  const backendReady = backend.connect();
  void backendReady.catch(() => {});
  void acknowledgeUpdateHealth(backendReady);
  void publishRollbackNotice();
  // Installer/updater writes a durable pending marker. Polling also covers an
  // external installer pushing an update while DeepX is already running.
  if (!smokeMode && app.isPackaged) {
    void publishPendingUpdate();
    updatePoll = setInterval(() => void publishPendingUpdate(), 2_000);
  }
  // Smoke mode validates that Electron can create the secured renderer and start
  // the backend connection path. Reconnection is intentionally unbounded in the
  // product, so the smoke process needs its own deterministic deadline.
  app.on("will-quit", () => {
    if (updatePoll) clearInterval(updatePoll);
    if (tray) {
      tray.destroy();
      tray = undefined;
    }
    killPet();
  });
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
    else showMainWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

app.on("before-quit", event => {
  if (quitting) return;
  event.preventDefault();
  quitting = true;
  ringing.close();
  void backend.close().finally(() => app.quit());
});
