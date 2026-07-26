export interface OpenDialogOptions {
  directory?: boolean;
  multiple?: boolean;
  title?: string;
}

export interface ConfirmDialogOptions {
  title?: string;
  kind?: "info" | "warning" | "error";
}

function desktopBridge(): NonNullable<Window["deepx"]>["desktop"] {
  const desktop = window.deepx?.desktop;
  if (!desktop) throw new Error("Electron preload bridge is unavailable");
  return desktop;
}

export function openDialog(options: OpenDialogOptions = {}): Promise<string | string[] | null> {
  return desktopBridge().openDialog(options);
}

export function confirmDialog(message: string, options?: ConfirmDialogOptions): Promise<boolean> {
  return desktopBridge().confirm(message, options);
}

export function openPath(target: string): Promise<void> {
  return desktopBridge().openPath(target);
}

export function togglePet(): Promise<boolean> {
  return desktopBridge().togglePet();
}

export function getPetStatus(): Promise<boolean> {
  return desktopBridge().getPetStatus();
}

/** Opens Electron DevTools in a detached window, including in packaged builds. */
export async function openDevTools(): Promise<boolean> {
  return window.deepx?.desktop.openDevTools() ?? false;
}

// ── Auto-update ──────────────────────────────────────────

export interface UpdateInfo {
  version: string;
  downloadUrl?: string;
  releaseNotes?: string;
  operationPath?: string;
  operationId?: string;
  mode?: "install" | "update" | "upgrade" | "current";
  artifacts?: string[];
  actions?: string[];
}

export function checkUpdate(): Promise<UpdateInfo | null> {
  return desktopBridge().checkUpdate();
}

export function stageUpdate(source: string): Promise<UpdateInfo | null> {
  return desktopBridge().stageUpdate(source);
}

export function applyUpdate(operationPath: string): Promise<{ restarting: boolean }> {
  return desktopBridge().applyUpdate(operationPath);
}

export function onUpdateAvailable(listener: (info: UpdateInfo) => void): () => void {
  return desktopBridge().onUpdateAvailable(listener);
}

export function onUpdateFailed(
  listener: (failure: { operationId: string; message: string }) => void,
): () => void {
  return desktopBridge().onUpdateFailed(listener);
}
