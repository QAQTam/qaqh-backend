/**
 * 传输单例 — Tauri 桌面壳后端(webui-tauri C3 起)。
 *
 * 浏览器 gateway 部署形态已随 `qaqh-webui-gateway` 移除:webui 的唯一运行形态
 * 是 Tauri 壳,Rust 宿主经 `qaqh-client` 直连 daemon,daemon token 不进 webview。
 * 类型与接口见 `./backend`。
 */
import { TauriTransport, listenHostDiagnostics, openExternalUrl } from "./tauri";
import {
  type Ack,
  type ApprovalKind,
  type ApprovalView,
  type GatewaySession,
  type StreamHandlers,
  type TimelinePageResponse,
  type TimelineStatusWire,
  type TransportBackend,
  isTauriRuntime,
  tauriHost,
} from "./backend";

export const transport: TransportBackend = new TauriTransport();

export { RINGING_SCHEMA, RINGING_VERSION } from "./backend";
export { tauriHost, isTauriRuntime, openExternalUrl, listenHostDiagnostics };
export type {
  Ack,
  ApprovalKind,
  ApprovalView,
  GatewaySession,
  StreamHandlers,
  TimelinePageResponse,
  TimelineStatusWire,
  TransportBackend,
};
