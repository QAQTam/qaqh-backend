/**
 * Tauri 桌面壳传输后端 — 经 Rust 宿主(qaqh-webui-app)的类型化 IPC 访问 daemon。
 *
 * 前端只见两类通道:
 *  - `invoke` 请求/响应(与宿主 commands.rs 一一对应);
 *  - 宿主 `ClientHandlers` 回调转发的事件(timeline://*, projection://*, conn://*,
 *    payload 与现行 SSE 帧同形)。
 *
 * 连接语义:重连/退避/续传责任上移宿主(qaqh-client 的 V2Stream/timeline 流自带
 * 重连),`lib/reconnect.ts` 的自动退避不再使用;connection 状态由
 * `timeline://status` 驱动;offline 后由用户动作(重试按钮/窗口聚焦)触发
 * `streams_retry`。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type Event, type UnlistenFn } from "@tauri-apps/api/event";
import {
  type Ack,
  type ApprovalView,
  type GatewaySession,
  isTauriRuntime,
  type StreamHandlers,
  type TimelinePageResponse,
  type TimelineStatusWire,
  type TransportBackend,
} from "./backend";

type HostDiagnostics = {
  onIncompatible?: (details: Record<string, any>) => void;
  onHostError?: (message: string) => void;
};

function rejectAck(ack: Ack): Error {
  return new Error(`command rejected (${ack.code ?? "unknown"}): ${ack.message ?? ""}`);
}

export class TauriTransport implements TransportBackend {
  /** 宿主的单一 active seed(attach 时登记;命令按它定界,与宿主语义一致)。 */
  private activeSeed: string | null = null;

  async bootstrap(): Promise<GatewaySession> {
    return { csrfToken: "tauri-host", expiresIn: 0 };
  }

  sessions(): Promise<any[]> {
    return invoke<any[]>("session_list");
  }

  async attach(seed: string, limit?: number): Promise<void> {
    // limit:宿主按这个大小取权威首页并推给 webview;省略则后端用默认页大小。
    await invoke("attach", { seed, limit: limit ?? null });
    this.activeSeed = seed;
  }

  approvals(): Promise<ApprovalView[]> {
    return invoke<ApprovalView[]>("pending_approvals");
  }

  async respondApproval(challengeId: string, decision: string, payload: Record<string, unknown> = {}): Promise<null> {
    await invoke("respond_approval", { challengeId, decision, payload });
    return null;
  }

  async command(channel: "control" | "conversation", command: Record<string, unknown>): Promise<Ack> {
    const type = typeof command.type === "string" ? command.type : "";
    if (channel === "conversation" && type === "conversation_send_message") {
      const text = typeof command.text === "string" ? command.text : "";
      const ack = await invoke<Ack>("send_message", { seed: this.requireSeed("send_message"), text });
      if (ack.status === "rejected") throw rejectAck(ack);
      return ack;
    }
    if (channel === "conversation" && type === "conversation_cancel") {
      const ack = await invoke<Ack>("cancel_turn", { seed: this.requireSeed("cancel_turn") });
      if (ack.status === "rejected") throw rejectAck(ack);
      return ack;
    }
    if (channel === "control" && type === "session_create") {
      const ack = await invoke<Ack>("create_session");
      if (ack.status === "rejected") throw rejectAck(ack);
      return ack;
    }
    throw new Error(`command not allowed from webui: ${channel}/${type || "(untyped)"}`);
  }

  rpc<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    return invoke<T>("service_rpc", { method, params });
  }

  async timelinePage(seed: string, query = ""): Promise<TimelinePageResponse> {
    const params = new URLSearchParams(query.startsWith("?") ? query.slice(1) : query);
    const limit = params.get("limit");
    const beforeIndex = params.get("before_index");
    return invoke<TimelinePageResponse>("timeline_page", {
      seed,
      limit: limit != null && limit !== "" ? Number(limit) : null,
      beforeIndex: beforeIndex != null && beforeIndex !== "" ? Number(beforeIndex) : null,
    });
  }

  // ── Tauri 专属面(TauriHostSurface) ──────────────────────────────────────────

  /** 订阅宿主转发事件;返回反订阅函数(store 切换/销毁时调用)。 */
  async subscribe(handlers: StreamHandlers): Promise<() => void> {
    const unlistens: UnlistenFn[] = [];
    const on = async <P>(name: string, run: (payload: P) => void): Promise<void> => {
      unlistens.push(await listen<P>(name, (event: Event<P>) => run(event.payload)));
    };
    await on<{ session_id: string; entry: Record<string, any> }>("timeline://entry", (payload) => {
      if (typeof payload?.session_id === "string") handlers.onTimelineEntry(payload.session_id, payload.entry);
    });
    await on<TimelineStatusWire>("timeline://status", (payload) => handlers.onTimelineStatus(payload));
    await on<TimelinePageResponse>("timeline://snapshot", (payload) => handlers.onTimelineSnapshot(payload));
    await on<Record<string, any>>("projection://event", (payload) => {
      const seed = typeof payload?.session_id === "string" ? payload.session_id : "";
      handlers.onProjectionEvent(seed, payload);
    });
    await on<Record<string, any>>("projection://reset", (payload) => {
      const seed = typeof payload?.session_id === "string" ? payload.session_id : "";
      handlers.onProjectionReset(seed, payload);
    });
    await on<Record<string, any>>("conn://incompatible", (payload) => handlers.onIncompatible(payload ?? {}));
    await on<{ message?: string }>("conn://error", (payload) => handlers.onHostError(String(payload?.message ?? "host error")));
    return () => {
      for (const unlisten of unlistens.splice(0)) unlisten();
    };
  }

  sessionBootstrap(seed: string): Promise<unknown> {
    return invoke<unknown>("session_bootstrap", { seed });
  }

  async timelineStatus(seed: string): Promise<TimelineStatusWire | null> {
    return invoke<TimelineStatusWire | null>("timeline_status", { seed });
  }

  async streamsRetry(seed?: string): Promise<void> {
    await invoke("streams_retry", { seed: seed ?? null });
  }

  async stopStaleDaemon(): Promise<"stopping" | "busy" | "unsupported"> {
    return invoke<"stopping" | "busy" | "unsupported">("stop_stale_daemon");
  }

  private requireSeed(action: string): string {
    if (this.activeSeed == null) throw new Error(`${action} 需要活动标签(尚未 attach)`);
    return this.activeSeed;
  }
}

/** 外链经宿主系统浏览器打开(仅 http/https)。 */
export async function openExternalUrl(url: string): Promise<void> {
  if (!/^https?:\/\//i.test(url)) return;
  await invoke("open_external", { url });
}

/** 宿主级诊断事件(App 装配期订阅,与具体标签无关)。非 Tauri 环境返回空操作。 */
export async function listenHostDiagnostics(handlers: HostDiagnostics): Promise<() => void> {
  if (!isTauriRuntime()) return () => {};
  const unlistens: UnlistenFn[] = [];
  if (handlers.onIncompatible) {
    unlistens.push(
      await listen<Record<string, any>>("conn://incompatible", (event) => handlers.onIncompatible!(event.payload ?? {})),
    );
  }
  if (handlers.onHostError) {
    unlistens.push(
      await listen<{ message?: string }>("conn://error", (event) =>
        handlers.onHostError!(String(event.payload?.message ?? "host error")),
      ),
    );
  }
  return () => {
    for (const unlisten of unlistens.splice(0)) unlisten();
  };
}
