/**
 * 开发夹具:真实 `App` + 假宿主 IPC,在浏览器里跑设置浮层的读写语义。
 *
 * 为什么要有它:设置页的唯一通路是 Tauri 的 `service_rpc` 白名单,浏览器里既没有
 * daemon 也没有宿主。这里把宿主替身做成「白名单式」的——只认宿主真允许的方法,
 * 其余抛错,于是 fixture 里能验的是语义而不是便利:`config.save` 收到什么 patch,
 * 就是 webview 真的发出去了什么。
 *
 * 不进产品构建:vite 构建入口是 `index.html`,本文件只被 `/settings-check.html` 引用。
 */
import { render } from "@solidjs/web";
import App from "./app/App";
import type { ConfigDto } from "./api/qaqh/ConfigDto";
import type { McpServerDto } from "./api/qaqh/McpServerDto";
import type { LspServerDto } from "./api/qaqh/LspServerDto";

type RpcCall = { method: string; params: Record<string, unknown> };

const baseConfig = (): ConfigDto => ({
  model: "ox-alpha-free",
  baseUrl: "https://opencode.ai/zen/go/v1",
  wire: "openai",
  maxTokens: 96000,
  contextLength: 1000000,
  reasoningEffort: "max",
  autoCompactThreshold: 0.95,
  permissionLevel: 2,
  apiKey: "****",
  lang: null,
  fontFamily: "",
  theme: null,
  notificationsEnabled: true,
  activeProfile: "default",
  profiles: ["default", "fast"],
  complianceEnabled: false,
  subagent: {
    model: "om-two",
    baseUrl: "https://other.example/compat/v1",
    apiKey: "",
    apiKeySet: false,
    maxTokens: 4096,
    timeoutSecs: 120,
    defaultTools: ["read", "write"],
    maxDepth: 1,
    messageInFlightPerPair: 16,
    messageOutboundPerSender: 1024,
  },
  mcp: {
    enabled: true,
    idleShutdownSecs: 300,
    servers: [
      {
        name: "context7",
        transport: "stdio",
        command: "npx",
        args: ["-y", "ctx7"],
        env: { KEY: "${secret:ctx7}" },
        url: "",
        headers: {},
        tools: null,
        resourcesEnabled: false,
        defaultTimeoutSecs: 30,
        maxConcurrentCalls: 8,
        cwd: "",
      } satisfies McpServerDto,
    ],
  },
  lsp: {
    enabled: false,
    idleShutdownSecs: 600,
    servers: [
      {
        name: "rust-analyzer",
        command: "rust-analyzer",
        args: [],
        env: {},
        extensions: ["rs"],
        startupTimeoutSecs: 20,
        defaultTimeoutSecs: 60,
      } satisfies LspServerDto,
    ],
  },
  tokenizerPath: null,
});

const probe = {
  calls: [] as RpcCall[],
  saves: [] as Record<string, unknown>[],
  config: baseConfig(),
};

/** 假宿主的 merge patch 应用:与 daemon 同语义——缺字段不动,`"****"`/空串密钥保持。 */
function applyMergePatch(target: ConfigDto, patch: Record<string, unknown>): void {
  for (const [key, value] of Object.entries(patch)) {
    if (key === "subagent" && value != null && typeof value === "object") {
      Object.assign(target.subagent, value as Record<string, unknown>);
      continue;
    }
    if (key === "apiKey" && (value === "****" || value === "")) continue;
    (target as Record<string, unknown>)[key] = value;
  }
}

function serviceRpc(method: string, params: Record<string, unknown>): unknown {
  probe.calls.push({ method, params });
  switch (method) {
    case "config.load":
      return structuredClone(probe.config);
    case "config.save":
      probe.saves.push(structuredClone(params));
      applyMergePatch(probe.config, params);
      return null;
    case "profile.save_current":
      if (!probe.config.profiles.includes(String(params.name))) {
        probe.config.profiles = [...probe.config.profiles, String(params.name)];
      }
      return null;
    case "profile.apply":
      probe.config.activeProfile = String(params.name);
      return null;
    case "profile.delete":
      probe.config.profiles = probe.config.profiles.filter((name) => name !== String(params.name));
      return null;
    default:
      // 宿主白名单(commands.rs 的 SERVICE_METHODS)之外的方法在这里也必须失败,
      // 否则夹具会替真实宿主放行它不曾放行的东西。
      throw new Error(`service method not allowed in fixture: ${method}`);
  }
}

const callbacks = new Map<number, (payload: unknown) => void>();
let nextCallbackId = 1;

const internals = {
  metadata: { currentWindow: { label: "main" }, windowLabels: { main: 1 } },
  transformCallback(callback: (payload: unknown) => void, once = false): number {
    void once;
    const id = nextCallbackId;
    nextCallbackId += 1;
    callbacks.set(id, callback);
    return id;
  },
  unregisterCallback(id: number): void {
    callbacks.delete(id);
  },
  async invoke(cmd: string, args: Record<string, unknown> = {}): Promise<unknown> {
    if (cmd === "service_rpc") return serviceRpc(String(args.method), (args.params ?? {}) as Record<string, unknown>);
    if (cmd === "session_list") return [];
    if (cmd === "timeline_status") return null;
    // 插件命令(event/opener/window)在夹具里都是空转:结构合法即可。
    return null;
  },
};

declare global {
  interface Window {
    __TAURI_INTERNALS__?: unknown;
    __qaqhProbe?: typeof probe;
  }
}

window.__TAURI_INTERNALS__ = internals;
window.__qaqhProbe = probe;

const root = document.getElementById("root");
if (root != null) render(() => <App />, root);
