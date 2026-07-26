import { contextBridge, ipcRenderer } from "electron";
import type { BackendStatus, ConfirmDialogOptions, ControlMessage, OpenDialogOptions } from "./types";

contextBridge.exposeInMainWorld("deepx", {
  backend: {
    connect: () => ipcRenderer.invoke("backend:connect"),
    request: (method: string, params: Record<string, unknown>) => ipcRenderer.invoke("backend:request", method, params),
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
  desktop: {
    openDialog: (options: OpenDialogOptions) => ipcRenderer.invoke("desktop:open-dialog", options),
    confirm: (message: string, options?: ConfirmDialogOptions) => ipcRenderer.invoke("desktop:confirm", message, options),
    openPath: (target: string) => ipcRenderer.invoke("desktop:open-path", target),
    togglePet: () => ipcRenderer.invoke("desktop:toggle-pet") as Promise<boolean>,
    getPetStatus: () => ipcRenderer.invoke("desktop:pet-status") as Promise<boolean>,
  },
});
