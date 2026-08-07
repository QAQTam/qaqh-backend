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
      seed: string;
      infoOpen: boolean;
      statsOpen: boolean;
      compacting: boolean;
      compactDisabled: boolean;
      undoDisabled: boolean;
      petEnabled: boolean;
    }): Promise<unknown>;
    /** 壳点击标题栏动作回传（host → renderer 事件）。 */
    onHeaderAction(listener: (action: { action: string; path?: string; file?: string }) => void): () => void;
    /** 主题推送（P-5 三态）：light | dark | dark-gray | system。 */
    setTheme(mode: "light" | "dark" | "dark-gray" | "system"): Promise<unknown>;
    /** 壳系统主题变化（host → renderer）：`{ mode: "light" | "dark" }`。 */
    onThemeChanged(listener: (update: { mode: "light" | "dark" }) => void): () => void;
    /** XAML 设置页初始投影（Web → 壳；镜像 bridge.rs `SettingsProjection`）。 */
    setSettings(state: {
      theme: "light" | "dark" | "dark-gray" | "system";
      lang: "en" | "zh";
      permissionLevel: number;
      workspaceMode: string;
    }): Promise<unknown>;
    /** 壳设置页动作回传（host → renderer 事件）。 */
    onSettingsAction(
      listener: (action: { action: string; lang?: string; mode?: string; level?: number }) => void,
    ): () => void;
    /** XAML 交互模态状态投影（Web → 壳；镜像 bridge.rs `InteractionState`）。 */
    setInteraction(state: {
      kind: "none" | "permission" | "ask" | "plan";
      id?: string;
      seed?: string;
      toolName?: string;
      reason?: string;
      paths?: string[];
      category?: string;
      level?: number;
      risk?: "low" | "medium" | "high";
      consequence?: string;
      questions?: Array<{
        id: string;
        question: string;
        options?: string[];
        allowCustom: boolean;
      }>;
      planContent?: string;
      reviewType?: string;
      todoItems?: Array<{ id: string; title: string; description: string; complexity: string }> | null;
    }): Promise<unknown>;
    /** 置位交互数据源直连（Rust 直连 daemon；置位后 setInteraction 投影停发）。 */
    setInteractionDirect(): Promise<unknown>;
    /** 壳交互覆盖层面板动作回传（host → renderer 事件）。 */
    onInteractionAction(
      listener: (action: {
        action: string;
        id?: string;
        approved?: boolean;
        trustFolder?: boolean;
        answers?: Array<{ question_id: string; answer: string }>;
        message?: string | null;
        autonomous?: boolean;
      }) => void,
    ): () => void;
    /** XAML Composer 状态投影（Web → 壳；镜像 bridge.rs `ComposerState`）。 */
    setComposer(state: {
      seed: string;
      isStreaming: boolean;
      hasPendingGate: boolean;
      mode: string;
      model: string;
      contextTokens: number;
      contextLimit: number;
      permissionLevel: number;
      queueCount: number;
      queueItems: Array<{ id: string; text: string }>;
      submitError: string;
      sendAck: number;
    }): Promise<unknown>;
    /** 置位 Composer 数据源直连（Rust 直连 daemon；壳侧合并读取投影）。 */
    setComposerDirect(): Promise<unknown>;
    /** 壳底部栏动作回传（host → renderer 事件）。 */
    onComposerAction(
      listener: (action: {
        action: string;
        text?: string;
        imagePaths?: Array<{ fileName: string; mimeType: string; path: string }>;
        textFiles?: Array<{ fileName: string; path: string }>;
        mode?: string;
        level?: number;
        id?: string;
      }) => void,
    ): () => void;
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
    /** P-3 统一 flag（WORKFLOW §6.1）：`{ sidebar: true, header: true, home: true, settings: true, ... }`。 */
    __DEEPX_XAML__?: Partial<Record<"sidebar" | "header" | "home" | "settings" | "info" | "interaction" | "composer" | "interactionDirect" | "composerDirect", boolean>>;
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
