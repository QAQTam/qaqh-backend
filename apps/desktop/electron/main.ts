import { join } from "node:path";
import { spawn } from "node:child_process";
import { app, BrowserWindow, dialog, ipcMain, shell } from "electron";
import { DaemonControlClient } from "./controlClient";
import type { ConfirmDialogOptions, OpenDialogOptions } from "./types";

let mainWindow: BrowserWindow | undefined;
let quitting = false;
let petProcess: ReturnType<typeof spawn> | undefined;
let petEnabled = false;

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
  ipcMain.handle("desktop:open-dialog", async (_event, raw: OpenDialogOptions = {}) => {
    const options = isRecord(raw) ? raw : {};
    const result = await dialog.showOpenDialog(mainWindow!, {
      title: typeof options.title === "string" ? options.title : undefined,
      properties: [options.directory ? "openDirectory" : "openFile", ...(options.multiple ? ["multiSelections" as const] : [])],
    });
    if (result.canceled) return null;
    return options.multiple ? result.filePaths : (result.filePaths[0] ?? null);
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

app.whenReady().then(() => {
  registerIpc();
  createWindow();
  void backend.connect().catch(() => {});
  // Smoke mode validates that Electron can create the secured renderer and start
  // the backend connection path. Reconnection is intentionally unbounded in the
  // product, so the smoke process needs its own deterministic deadline.
  app.on("will-quit", () => {
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
