import { contextBridge, ipcRenderer } from "electron";
import type { BackendStatus, ConfirmDialogOptions, ControlMessage, OpenDialogOptions, UpdateInfo } from "./types";
import type { RingingEventBatch } from "../src/lib/types/ringing";

contextBridge.exposeInMainWorld("deepx", {
  backend: {
    connect: () => ipcRenderer.invoke("backend:connect"),
    request: (method: string, params: Record<string, unknown>) => ipcRenderer.invoke("backend:request", method, params),
    restart: () => ipcRenderer.invoke("backend:restart") as Promise<{ ok: boolean; reason?: string }>,
    attach: (seed: string) => ipcRenderer.invoke("backend:attach", seed),
    detach: (seed: string) => ipcRenderer.invoke("backend:detach", seed),
    status: () => ipcRenderer.invoke("backend:status") as Promise<BackendStatus>,
    onMessage: (listener: (message: ControlMessage) => void) => {
      const handler = (_event: Electron.IpcRendererEvent, message: ControlMessage) => listener(message);
      ipcRenderer.on("backend:message", handler);
      return () => ipcRenderer.removeListener("backend:message", handler);
    },
    onStatus: (listener: (status: BackendStatus) => void) => {
      const handler = (_event: Electron.IpcRendererEvent, status: BackendStatus) => listener(status);
      ipcRenderer.on("backend:status", handler);
      return () => ipcRenderer.removeListener("backend:status", handler);
    },
  },
  ringing: {
    status: () => ipcRenderer.invoke("ringing:status"),
    bootstrap: (seed: string) => ipcRenderer.invoke("ringing:bootstrap", seed),
    snapshot: (seed: string, channel: string) =>
      ipcRenderer.invoke("ringing:snapshot", seed, channel),
    command: (seed: string, channel: string, envelope: unknown) =>
      ipcRenderer.invoke("ringing:command", seed, channel, envelope),
    query: (path: string, params?: Record<string, string | undefined>) =>
      ipcRenderer.invoke("ringing:query", path, params),
    onBatch: (listener: (batch: RingingEventBatch) => void) => {
      const handler = (_event: Electron.IpcRendererEvent, batch: RingingEventBatch) => listener(batch);
      ipcRenderer.on("ringing:batch", handler);
      return () => ipcRenderer.removeListener("ringing:batch", handler);
    },
    onStatus: (listener: (update: { channel: string; status: unknown }) => void) => {
      const handler = (_event: Electron.IpcRendererEvent, update: { channel: string; status: unknown }) => listener(update);
      ipcRenderer.on("ringing:status", handler);
      return () => ipcRenderer.removeListener("ringing:status", handler);
    },
    onSnapshot: (listener: (update: { seed: string; channel: string; snapshot: unknown }) => void) => {
      const handler = (
        _event: Electron.IpcRendererEvent,
        update: { seed: string; channel: string; snapshot: unknown },
      ) => listener(update);
      ipcRenderer.on("ringing:snapshot", handler);
      return () => ipcRenderer.removeListener("ringing:snapshot", handler);
    },
  },
  desktop: {
    openDialog: (options: OpenDialogOptions) => ipcRenderer.invoke("desktop:open-dialog", options),
    openImageDialog: () => ipcRenderer.invoke("desktop:open-image-dialog") as Promise<string | null>,
    readFileBase64: (filePath: string) => ipcRenderer.invoke("desktop:read-file-base64", filePath) as Promise<{ mimeType: string; data: string; size: number }>,
    readTextFile: (filePath: string) => ipcRenderer.invoke("desktop:read-text-file", filePath) as Promise<{ content: string; size: number }>,
    confirm: (message: string, options?: ConfirmDialogOptions) => ipcRenderer.invoke("desktop:confirm", message, options),
    openPath: (target: string) => ipcRenderer.invoke("desktop:open-path", target),
    togglePet: () => ipcRenderer.invoke("desktop:toggle-pet") as Promise<boolean>,
    getPetStatus: () => ipcRenderer.invoke("desktop:pet-status") as Promise<boolean>,
    checkUpdate: () => ipcRenderer.invoke("desktop:check-update") as Promise<UpdateInfo | null>,
    stageUpdate: (source: string) => ipcRenderer.invoke("desktop:stage-update", source) as Promise<UpdateInfo | null>,
    applyUpdate: (operationPath: string) => ipcRenderer.invoke("desktop:apply-update", operationPath) as Promise<{ restarting: boolean }>,
    openDevTools: () => ipcRenderer.invoke("desktop:open-devtools") as Promise<boolean>,
    setBackgroundMaterial: (material: "auto" | "mica" | "acrylic" | "none") =>
      ipcRenderer.invoke("desktop:set-background-material", material) as Promise<boolean>,
    onUpdateAvailable: (listener: (info: UpdateInfo) => void) => {
      const handler = (_event: Electron.IpcRendererEvent, info: UpdateInfo) => listener(info);
      ipcRenderer.on("update:available", handler);
      return () => ipcRenderer.removeListener("update:available", handler);
    },
    onUpdateFailed: (listener: (failure: { operationId: string; message: string }) => void) => {
      const handler = (
        _event: Electron.IpcRendererEvent,
        failure: { operationId: string; message: string },
      ) => listener(failure);
      ipcRenderer.on("update:failed", handler);
      return () => ipcRenderer.removeListener("update:failed", handler);
    },
  },
});
