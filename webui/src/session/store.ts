/**
 * 一个标签页 = 一个会话的客户端状态机。
 *
 * 宿主契约(webui-tauri):所有 per-seed 面(命令/审批/RPC/timeline 流/快照)
 * 经 Rust 宿主的类型化 IPC 转发,宿主维护「单一 active seed 持流」(plan D4);
 * 切标签 = attach(宿主切 active + 重建流)+ 快照重建(权威对齐)。后台标签
 * 不收流,状态由标签页轮询 sessions 列表(turn_count/running,后端事实)驱动
 * 状态点。重连/退避/续传责任在宿主(qaqh-client),前端不再有 SSE 循环。
 *
 * 数据单源:transcript 结构/文本只来自 timeline(宿主转发的快照+SSE 帧,
 * `watermark` 去重 + 缺口触发快照);审批只来自 approvals RPC(投影事件仅当
 * 刷新信号);会话标题/运行态来自 sessions 列表。
 */
import { createSignal, createStore } from "solid-js";
import { transport, tauriHost, type ApprovalView, type StreamHandlers } from "../lib/transport";
import { now } from "../lib/time";
import { applyEntry, applySnapshot, emptySession, prependPage } from "./reducer";
import type { SessionState, TimelineEntryWire, Wait } from "./types";

export type ConnectionKind = "connected" | "reconnecting" | "offline";

/** timeline 快照窗口页大小;向上翻页页大小(spec §14.2 limit=30)。 */
const SNAPSHOT_LIMIT = 50;
const PAGE_LIMIT = 30;
/** 缺口触发快照校正的最小间隔(防抖,高吞吐下不连环拉全量)。 */
const RESNAPSHOT_DEBOUNCE_MS = 2_000;

export class SessionStore {
  readonly seed: string;
  readonly state: [SessionState, (fn: (draft: SessionState) => void) => void];

  connection = createSignal<ConnectionKind>("reconnecting");
  activity = createSignal<"idle" | "working" | "waiting_user" | null>(null);
  title = createSignal<string | null>(null);
  pending = createSignal<ApprovalView[]>([]);
  loadError = createSignal(false);
  hasNewReply = createSignal(false);
  /** 「此前已压缩」分隔锚点回合 key;null = 渲染在顶部。 */
  compactedAfter = createSignal<string | null>(null);
  loadingOlder = createSignal(false);

  /** 宿主事件反订阅。 */
  private hostUnlisten: (() => void) | null = null;
  private pendingText = new Map<string, { turnId: string; blockId: string; delta: string }>();
  private rafHandle: number | null = null;
  private lastResnapshotAt = 0;
  private waitFrom: { kind: Wait["kind"]; at: number } | null = null;
  private lastTurnCount: number | null = null;
  private disposed = false;

  constructor(seed: string) {
    this.seed = seed;
    this.state = createStore<SessionState>(emptySession());
  }

  private get stateGet(): SessionState {
    return this.state[0];
  }

  private mutate(fn: (draft: SessionState) => void): void {
    this.state[1](fn);
  }

  private host(): NonNullable<ReturnType<typeof tauriHost>> {
    const host = tauriHost(transport);
    if (host == null) throw new Error("宿主传输后端不可用(应在 Tauri 壳内运行)");
    return host;
  }

  // ── 生命周期 ────────────────────────────────────────────────────────────────

  /** 成为活动标签:attach(宿主切 active + 重建流)→ 快照重建 → 订阅。 */
  async activate(): Promise<void> {
    if (this.disposed) return;
    this.hasNewReply[1](false);
    await transport.attach(this.seed);
    await this.resnapshot();
    void this.connectHost();
    void this.refreshApprovals();
    void this.refreshActivity();
  }

  /** 失去活动资格(别的标签接管):退订宿主事件保状态(流由宿主在新 attach 时停)。 */
  deactivate(): void {
    this.closeHost();
    this.setConnection("reconnecting");
  }

  dispose(): void {
    this.disposed = true;
    this.closeHost();
  }

  retry(): void {
    // 重连/退避在宿主侧,这里只触发宿主重建流(用户点「重试」/窗口聚焦)。
    this.setConnection("reconnecting");
    this.host()
      .streamsRetry(this.seed)
      .catch(() => this.setConnection("offline"));
  }

  private setConnection(kind: ConnectionKind): void {
    this.connection[1](kind);
  }

  private closeHost(): void {
    this.hostUnlisten?.();
    this.hostUnlisten = null;
  }

  // ── 宿主事件订阅(timeline://*, projection://*) ─────────────────────────────

  private async connectHost(): Promise<void> {
    if (this.disposed) return;
    this.closeHost();
    this.setConnection("reconnecting");
    const handlers: StreamHandlers = {
      onTimelineEntry: (seed, entry) => {
        if (seed === this.seed) this.onTimelineEntry(entry as TimelineEntryWire);
      },
      onTimelineStatus: (status) => {
        if (status.session_id != null && status.session_id !== this.seed) return;
        if (status.status === "open") {
          this.setConnection("connected");
        } else if (status.status === "closed") {
          this.setConnection("offline");
        } else {
          this.setConnection("reconnecting");
        }
      },
      onProjectionEvent: (seed, envelope) => {
        if (seed !== this.seed) return;
        this.applyProjection(envelope.stream_key as Record<string, any> | undefined, envelope.payload as Record<string, any> | undefined);
      },
      onProjectionReset: (seed) => {
        if (seed !== this.seed) return;
        // 宿主的 v2 流会自动以 snapshot cursor 重订阅;这里刷新派生信号即可。
        void this.refreshApprovals();
        void this.refreshActivity();
      },
      onTimelineSnapshot: (page) => {
        // 宿主侧缺口恢复推来的权威快照(前端 watermark 去重天然兜底)。
        if (page.session_id != null && page.session_id !== this.seed) return;
        this.mutate((draft) => {
          applySnapshot(draft, page);
        });
      },
      onIncompatible: () => {},
      onHostError: () => {},
    };
    try {
      this.hostUnlisten = await this.host().subscribe(handlers);
    } catch {
      this.setConnection("offline");
    }
  }

  private onTimelineEntry(entry: TimelineEntryWire): void {
    if (typeof entry?.timeline_seq !== "number" || !entry.turn_id) return;
    const watermark = this.stateGet.watermark;
    if (entry.timeline_seq <= watermark) return; // 重复(重放)直接丢弃(幂等)
    if (entry.timeline_seq > watermark + 1 && watermark > 0) {
      // seq 缺口:丢弃该事件并触发一次快照校正(spec §15.2)。
      this.scheduleResnapshot();
      return;
    }
    if (entry.event?.type === "text_delta") {
      // 文本增量 rAF 合并:每帧至多写一次 store(spec §6/§10.2)。
      const key = `${entry.turn_id}\u0000${String(entry.event.block_id ?? "")}`;
      const buffered = this.pendingText.get(key);
      const delta = typeof entry.event.delta === "string" ? entry.event.delta : "";
      if (buffered) buffered.delta += delta;
      else this.pendingText.set(key, { turnId: entry.turn_id, blockId: String(entry.event.block_id ?? ""), delta });
      this.scheduleFlush();
    } else {
      this.mutate((draft) => {
        applyEntry(draft, entry);
        draft.watermark = entry.timeline_seq;
      });
    }
  }

  private scheduleFlush(): void {
    if (this.rafHandle != null) return;
    this.rafHandle = requestAnimationFrame(() => {
      this.rafHandle = null;
      if (this.pendingText.size === 0) return;
      const batch = [...this.pendingText.values()];
      this.pendingText.clear();
      this.mutate((draft) => {
        for (const item of batch) {
          applyEntry(draft, {
            timeline_seq: draft.watermark + 1,
            turn_id: item.turnId,
            event: { type: "text_delta", block_id: item.blockId, delta: item.delta },
          });
          // delta 应用不推进 watermark(真实 seq 由下一条非 delta 帧推进);
          // 合成 seq 仅用于排序,与真实流保持同调。
        }
      });
    });
  }

  private scheduleResnapshot(): void {
    const at = Date.now();
    if (at - this.lastResnapshotAt < RESNAPSHOT_DEBOUNCE_MS) return;
    this.lastResnapshotAt = at;
    void this.resnapshot();
  }

  /** 快照重建(attach/缺口校正后的权威对齐)。 */
  async resnapshot(): Promise<void> {
    try {
      const page = await transport.timelinePage(this.seed, `?limit=${SNAPSHOT_LIMIT}`);
      this.mutate((draft) => {
        applySnapshot(draft, page);
      });
      this.loadError[1](false);
    } catch {
      // 快照失败不致命:流还在,下一次触发再试。
    }
  }

  // ── 投影事件(刷新信号,宿主转发) ─────────────────────────────────────────────

  /** 前端只把投影事件当刷新信号;数据一律走 approvals RPC / sessions 列表。 */
  private applyProjection(streamKey: unknown, payload: unknown): void {
    const key = (streamKey ?? {}) as Record<string, any>;
    const body = (payload ?? {}) as Record<string, any>;
    const delta = (body.data ?? {}) as Record<string, any>;
    const kind = typeof delta.kind === "string" ? delta.kind : "";
    if (key.data === "control") {
      switch (kind) {
        case "activity":
          this.activity[1](normalizeActivity(delta.state));
          break;
        case "interaction_requested":
        case "interaction_resolved":
        case "interaction_expired":
          void this.refreshApprovals();
          break;
        default:
          break;
      }
      return;
    }
    if (key.data === "tool") {
      if (kind === "tool_intent" || kind === "tool_finished") void this.refreshApprovals();
      return;
    }
    if (key.data === "conversation") {
      if (kind === "turn_finished" || kind === "turn_interrupted") void this.refreshApprovals();
      if (kind === "compaction_applied") {
        const turns = Object.values(this.stateGet.turns);
        const anchor = turns.length > 0 ? turns[turns.length - 1]!.key : null;
        this.compactedAfter[1](anchor);
      }
    }
  }

  async refreshActivity(): Promise<void> {
    try {
      const bootstrap = (await this.host().sessionBootstrap(this.seed)) as any;
      const control = bootstrap?.control?.state ?? {};
      this.activity[1](normalizeActivity(control.activity));
    } catch {
      // best-effort;activity 由投影事件持续修正。
    }
  }

  // ── 审批(单源:approvals RPC) ───────────────────────────────────────────────

  async refreshApprovals(): Promise<void> {
    let list: ApprovalView[];
    try {
      list = await transport.approvals();
    } catch {
      return; // best-effort:下一轮信号再取
    }
    const previous = this.pending[0]();
    const next = Array.isArray(list) ? list : [];
    this.pending[1](next);
    this.recordWaitTransition(previous, next);
    this.activity[1](next.length > 0 ? "waiting_user" : this.activityIsWorking() ? "working" : "idle");
  }

  private activityIsWorking(): boolean {
    return this.activity[0]() === "working";
  }

  /** 0→N 开区间 / N→0 闭区间,挂到当前运行回合(spec D3:已工作扣除等待)。 */
  private recordWaitTransition(previous: ApprovalView[], next: ApprovalView[]): void {
    const runningTurn = Object.values(this.stateGet.turns).find((turn) => turn.status === "running");
    if (previous.length === 0 && next.length > 0) {
      const kind = next[0]!.kind === "ask" || next[0]!.kind === "plan" ? "ask" : "approval";
      this.waitFrom = { kind, at: now() };
    } else if (previous.length > 0 && next.length === 0 && this.waitFrom) {
      const wait: Wait = { turnKey: runningTurn?.key ?? null, kind: this.waitFrom.kind, from: this.waitFrom.at, to: now() };
      this.mutate((draft) => {
        draft.waits.push(wait);
      });
      this.waitFrom = null;
    }
  }

  async respondApproval(challengeId: string, decision: string, payload: Record<string, unknown> = {}): Promise<void> {
    await transport.respondApproval(challengeId, decision, payload);
    await this.refreshApprovals();
  }

  // ── 动作 ────────────────────────────────────────────────────────────────────

  async sendMessage(text: string): Promise<void> {
    await transport.command("conversation", {
      channel: "conversation",
      type: "conversation_send_message",
      text,
      images: [],
      as_system: false,
    });
  }

  async cancelTurn(): Promise<void> {
    await transport.command("conversation", { channel: "conversation", type: "conversation_cancel" });
  }

  async createSession(): Promise<void> {
    await transport.command("control", { channel: "control", type: "session_create", close_current: false });
  }

  /** 向上翻页(spec §14.2)。返回是否真的加载了内容(调用方做滚动补偿)。 */
  async loadOlder(): Promise<boolean> {
    const state = this.stateGet;
    if (this.loadingOlder[0]() || !state.hasMore || state.oldestIndex == null) return false;
    this.loadingOlder[1](true);
    this.loadError[1](false);
    try {
      const page = await transport.timelinePage(this.seed, `?limit=${PAGE_LIMIT}&before_index=${state.oldestIndex}`);
      this.mutate((draft) => {
        prependPage(draft, page);
      });
      return true;
    } catch {
      this.loadError[1](true); // 顶部「加载失败,点击重试」,不自动重试
      return false;
    } finally {
      this.loadingOlder[1](false);
    }
  }

  /** 回合淘汰占位高度回填(由视图测量后写入)。 */
  setPlaceholderHeight(key: string, height: number): void {
    this.mutate((draft) => {
      for (let i = 0; i < draft.slots.length; i += 1) {
        const slot = draft.slots[i]!;
        if (slot.kind === "placeholder" && slot.key === key) slot.height = height;
      }
    });
  }

  /** sessions 列表(轮询)驱动:标题与后台「有新回复」点。 */
  applySessionMeta(meta: { title?: string | null; turn_count?: number; running?: boolean }): void {
    if (typeof meta.title === "string" && meta.title) this.title[1](meta.title);
    if (typeof meta.turn_count === "number") {
      const known = this.lastTurnCount;
      if (known != null && meta.turn_count > known) this.hasNewReply[1](true);
      this.lastTurnCount = meta.turn_count;
    }
  }
}

function normalizeActivity(state: unknown): "idle" | "working" | "waiting_user" | null {
  if (state === "working" || state === "waiting_user" || state === "idle") return state;
  return null;
}
