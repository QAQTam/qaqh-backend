/**
 * 传输后端接口 — 前端到后端的唯一门面。
 *
 * Tauri 桌面壳模式:Rust 宿主(qaqh-webui-app)经 qaqh-client 直连 daemon,
 * 前端只走类型化 IPC invoke + 事件订阅(daemon token 不会出现在 webview)。
 * store/组件只面向本接口;连接管理由宿主事件驱动(见 `StreamHandlers`)。
 */

import type { RingingCommandAck } from "../../api/qaqh/RingingCommandAck";
import type { TimelineSnapshot } from "../../api/qaqh/TimelineSnapshot";

export const RINGING_SCHEMA = "qaqh.Ringing";
/** Ringing v2 单一信封版本:历史 v1 兼容已拆除。 */
export const RINGING_VERSION = 2;

/**
 * 命令确认。形状是生成绑定(此前手写镜像漏掉了 `retry_after_ms` —— 限流/退避
 * 提示),本地只留别名。
 */
export type Ack = RingingCommandAck;

export type ApprovalKind = "tool_permission" | "ask" | "plan";

export type ApprovalView = {
  challenge_id: string;
  kind: ApprovalKind;
  expires_in: number;
  details: Record<string, any>;
};

export type TimelinePageResponse = {
  server_epoch?: string;
  session_id?: string;
  /** 权威快照:形状由 ts-rs 生成(回合/轮/块整条链)。 */
  snapshot?: TimelineSnapshot | null;
  has_more?: boolean;
  total_turns?: number;
  truncated_before?: boolean;
};

/** Tauri 宿主转发的 timeline 状态(timeline://status,serde tagged)。 */
export type TimelineStatusWire = {
  status: "connecting" | "open" | "reconnecting" | "closed";
  session_id?: string;
  [key: string]: unknown;
};

/** todo.list 的条目线类型(daemon typed 投影;status 归一化在 UI 侧做)。 */
export type TodoItemWire = {
  id?: string;
  title?: string;
  description?: string;
  status?: string;
  evidence?: string;
};

/** 宿主事件的订阅面(事件 payload 与现行 SSE 帧同形)。 */
export interface StreamHandlers {
  onTimelineEntry(seed: string, entry: Record<string, any>): void;
  onTimelineStatus(status: TimelineStatusWire): void;
  onProjectionEvent(seed: string, envelope: Record<string, any>): void;
  onProjectionReset(seed: string, reset: Record<string, any>): void;
  onTimelineSnapshot(page: TimelinePageResponse): void;
  onIncompatible(details: Record<string, any>): void;
  onHostError(message: string): void;
}

export interface TransportBackend {
  /** 会话列表(宿主侧已 sanitize 的投影字段)。 */
  sessions(): Promise<any[]>;

  /** attach:宿主切 active seed + attach + 激活 timeline 流。 */
  attach(seed: string, limit?: number): Promise<void>;

  /** 待审批(challenge 由宿主签发,canonical id 不出宿主)。 */
  approvals(): Promise<ApprovalView[]>;

  respondApproval(challengeId: string, decision: string, payload?: Record<string, unknown>): Promise<null>;

  command(channel: "control" | "conversation", command: Record<string, unknown>): Promise<Ack>;

  rpc<T = unknown>(method: string, params?: Record<string, unknown>): Promise<T>;

  /** Timeline 快照/翻页;`query` 形如 `?limit=30&before_index=12`。 */
  timelinePage(seed: string, query?: string): Promise<TimelinePageResponse>;
}

/** Tauri 宿主专属面。 */
export interface TauriHostSurface {
  /** 订阅宿主转发事件;返回反订阅函数。 */
  subscribe(handlers: StreamHandlers): Promise<() => void>;
  /** 查询宿主 timeline 流当前状态(null = 从未激活;兜底订阅晚于事件的竞态)。 */
  timelineStatus(seed: string): Promise<TimelineStatusWire | null>;
  /** per-session bootstrap RPC(刷新 activity)。 */
  sessionBootstrap(seed: string): Promise<unknown>;
  /** 宿主侧重连(用户点「重试」/窗口聚焦)。 */
  streamsRetry(seed?: string): Promise<void>;
  /** D1 不兼容路径:停止旧 daemon(不静默杀;Busy 时返回 "busy")。 */
  stopStaleDaemon(): Promise<"stopping" | "busy" | "unsupported">;
}

/** 若当前后端是 Tauri 宿主,返回其专属面;否则 null。 */
export function tauriHost(backend: TransportBackend): TauriHostSurface | null {
  return backend instanceof Object && "subscribe" in backend
    ? (backend as TransportBackend & TauriHostSurface)
    : null;
}

export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ != null;
}
