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
