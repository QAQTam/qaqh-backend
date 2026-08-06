interface DeepxControlMessage {
  type: string;
  [key: string]: unknown;
}

interface DeepxDesktopApi {
  backend: {
    connect(): Promise<void>;
    request(method: string, params: Record<string, unknown>): Promise<unknown>;
    restart(): Promise<{ ok: boolean; reason?: string }>;
    attach(seed: string): Promise<unknown>;
    detach(seed: string): Promise<unknown>;
    status(): Promise<{ connected: boolean; transport?: "ringing" | "legacy"; error?: string }>;
    onMessage(listener: (message: DeepxControlMessage) => void): () => void;
    onStatus(listener: (status: { connected: boolean; transport?: "ringing" | "legacy"; error?: string }) => void): () => void;
  };
  ringing: {
    status(): Promise<Record<string, { state: string; detail?: string } | null>>;
    bootstrap?(seed: string): Promise<unknown>;
    snapshot(seed: string, channel: string): Promise<unknown>;
    command(seed: string, channel: string, envelope: unknown): Promise<unknown>;
    query(path: string, params?: Record<string, string | undefined>): Promise<unknown>;
    onBatch(listener: (batch: import("../lib/types/ringing").RingingEventBatch) => void): () => void;
    onStatus(listener: (update: { channel: string; status: unknown }) => void): () => void;
    onSnapshot(listener: (update: { seed: string; channel: string; snapshot: unknown }) => void): () => void;
  };
  timeline?: {
    activate(seed: string): Promise<import("../store/timelineProtocol").TimelineSnapshotResponse>;
    status(): Promise<unknown>;
    onEntry(listener: (update: { seed: string; entry: import("../store/timelineProtocol").TimelineEntry }) => void): () => void;
    onSnapshot(listener: (snapshot: import("../store/timelineProtocol").TimelineSnapshotResponse) => void): () => void;
    onStatus(listener: (status: unknown) => void): () => void;
  };
  shell?: {
    /** XAML 原生侧栏导航事件（host → renderer 单向）。 */
    onNavigate(listener: (nav: ShellNavigate) => void): () => void;
    /** XAML 标题栏状态投影（Web → 壳；载荷镜像 bridge.rs `HeaderState`）。 */
    setHeader(state: {
      view: string;
      title: string;
      workspace: string;
      infoOpen: boolean;
      statsOpen: boolean;
      compacting: boolean;
      compactDisabled: boolean;
      undoDisabled: boolean;
      petEnabled: boolean;
    }): Promise<unknown>;
    /** 壳点击标题栏动作回传（host → renderer 事件）。 */
    onHeaderAction(listener: (action: { action: string; path?: string }) => void): () => void;
    /** 主题推送（P-5 三态）：light | dark | dark-gray | system。 */
    setTheme(mode: "light" | "dark" | "dark-gray" | "system"): Promise<unknown>;
    /** 壳系统主题变化（host → renderer）：`{ mode: "light" | "dark" }`。 */
    onThemeChanged(listener: (update: { mode: "light" | "dark" }) => void): () => void;
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
    openDevTools(): Promise<boolean>;
    setBackgroundMaterial(material: "auto" | "mica" | "acrylic" | "none"): Promise<boolean>;
    onUpdateAvailable(listener: (info: UpdateInfo) => void): () => void;
    onUpdateFailed(listener: (failure: { operationId: string; message: string }) => void): () => void;
    openImageDialog(): Promise<string | null>;
    readFileBase64(filePath: string): Promise<{ mimeType: string; data: string; size: number }>;
    readTextFile(filePath: string): Promise<{ content: string; size: number }>;
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

/** XAML 原生侧栏导航载荷（镜像 bridge.rs `shell.navigate`）。 */
interface ShellNavigate {
  view: "home" | "chat" | "skills" | "settings";
  seed?: string;
}

declare global {
  interface Window {
    deepx?: DeepxDesktopApi;
    /** WinUI 壳注入：原生 XAML 侧栏接管时置 true（renderer 隐藏 web 侧栏）。 */
    __DEEPX_XAML_SIDEBAR__?: boolean;
    /** P-3 统一 flag（WORKFLOW §6.1）：`{ sidebar: true, header: true, ... }`。 */
    __DEEPX_XAML__?: Partial<Record<"sidebar" | "header", boolean>>;
    /** WebView2 宿主桥（winui 壳）：postMessage 双向通道。 */
    chrome?: {
      webview?: {
        postMessage(message: unknown): void;
        addEventListener(type: "message", listener: (event: { data: unknown }) => void): void;
      };
    };
  }
}

export {};
