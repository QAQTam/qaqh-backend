import { dirname, join, resolve, sep } from "node:path";
import { execFile, spawn } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { app, BrowserWindow, dialog, ipcMain, Menu, shell } from "electron";
import { DaemonControlClient } from "./controlClient";
import type { ConfirmDialogOptions, OpenDialogOptions, UpdateInfo } from "./types";

let mainWindow: BrowserWindow | undefined;
let quitting = false;
let petProcess: ReturnType<typeof spawn> | undefined;
let petEnabled = false;
let lastPendingOperation = "";
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

if (smokeMode) {
  setTimeout(() => {
    void backend.close();
    console.error("Electron smoke test timed out before the preload/backend bridge was ready");
    app.exit(1);
  }, 30_000);
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
  mainWindow.webContents.setWindowOpenHandler(({ url }) => {
    if (url.startsWith("https://") || url.startsWith("http://")) void shell.openExternal(url);
    return { action: "deny" };
  });
  mainWindow.webContents.on("will-navigate", event => event.preventDefault());

  if (process.env.ELECTRON_RENDERER_URL) void mainWindow.loadURL(process.env.ELECTRON_RENDERER_URL);
  else void mainWindow.loadFile(join(__dirname, "../renderer/index.html"));
}

function registerIpc(): void {
  ipcMain.handle("backend:connect", () => backend.connect());
  ipcMain.handle("backend:request", (_event, method: unknown, params: unknown) => {
    if (typeof method !== "string" || !isRecord(params)) throw new Error("invalid backend request");
    return backend.request(method, params);
  });
  ipcMain.handle("backend:attach", (_event, seed: unknown) => backend.attach(requireSeed(seed)));
  ipcMain.handle("backend:detach", (_event, seed: unknown) => backend.detach(requireSeed(seed)));
  ipcMain.handle("backend:status", () => backend.currentStatus());
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
    killPet();
  });
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

app.on("before-quit", event => {
  if (quitting) return;
  event.preventDefault();
  quitting = true;
  void backend.close().finally(() => app.quit());
});
