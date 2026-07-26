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
  };
}

declare global {
  interface Window {
    deepx?: DeepxDesktopApi;
  }
}

export {};
