interface DeepxControlMessage {
  type: string;
  [key: string]: unknown;
}

interface DeepxDesktopApi {
  backend: {
    connect(): Promise<void>;
    request(method: string, params: Record<string, unknown>): Promise<unknown>;
    attach(seed: string): Promise<unknown>;
    detach(seed: string): Promise<unknown>;
    status(): Promise<{ connected: boolean; error?: string }>;
    onMessage(listener: (message: DeepxControlMessage) => void): () => void;
    onStatus(listener: (status: { connected: boolean; error?: string }) => void): () => void;
  };
  desktop: {
    openDialog(options: { directory?: boolean; multiple?: boolean; title?: string }): Promise<string | string[] | null>;
    confirm(message: string, options?: { title?: string; kind?: "info" | "warning" | "error" }): Promise<boolean>;
    openPath(target: string): Promise<void>;
    togglePet(): Promise<boolean>;
    getPetStatus(): Promise<boolean>;
    checkUpdate(): Promise<UpdateInfo | null>;
    stageUpdate(source: string): Promise<UpdateInfo | null>;
    applyUpdate(operationPath: string): Promise<{ restarting: boolean }>;
    windowMinimize(): void;
    windowToggleMaximize(): Promise<boolean>;
    windowIsMaximized(): Promise<boolean>;
    windowClose(): void;
    onWindowMaximizedChanged(listener: (maximized: boolean) => void): () => void;
    onUpdateAvailable(listener: (info: UpdateInfo) => void): () => void;
    onUpdateFailed(listener: (failure: { operationId: string; message: string }) => void): () => void;
  };
}

interface UpdateInfo {
  version: string;
  downloadUrl?: string;
  releaseNotes?: string;
  operationPath?: string;
  operationId?: string;
  mode?: "install" | "update" | "upgrade" | "current";
  artifacts?: string[];
  actions?: string[];
}

declare global {
  interface Window {
    deepx?: DeepxDesktopApi;
  }
}

export {};
