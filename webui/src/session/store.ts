/**
 * 一个标签页 = 一个会话的客户端状态机。
 *
 * 网关契约:所有 per-seed 面(命令/审批/RPC/timeline SSE/快照)都限定在网关的
 * **单一 active session** 上,因此只有活动标签持有连接;切标签 = attach(网关侧
 * 切 active)+ 快照重建(权威对齐)。后台标签不收流,状态由标签页轮询
 * sessions 列表(turn_count/running,后端事实)驱动状态点。
 *
 * 数据单源:transcript 结构/文本只来自 timeline(快照+SSE,`last_event_id` 续传,
 * `watermark` 去重 + 缺口触发快照);审批只来自 approvals RPC(投影事件仅当刷新
 * 信号);会话标题/运行态来自 sessions 列表。
 */
import { createSignal, createStore } from "solid-js";
import { transport, type ApprovalView } from "../lib/transport";
import { backoffDelayMs, exhausted } from "../lib/reconnect";
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

  private sources = new Map<"timeline" | "events", EventSource>();
  private attempts: Record<"timeline" | "events", number> = { timeline: 0, events: 0 };
  private timers: Record<"timeline" | "events", ReturnType<typeof setTimeout> | null> = { timeline: null, events: null };
  private lastTimelineEventId = "";
  private lastEventsCursor: string | null = null;
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

  // ── 生命周期 ────────────────────────────────────────────────────────────────

  /** 成为活动标签:attach(网关 active 切换)→ 快照重建 → 建流。 */
  async activate(): Promise<void> {
    if (this.disposed) return;
    this.hasNewReply[1](false);
    await transport.attach(this.seed);
    await this.resnapshot();
    this.connectAll();
    void this.refreshApprovals();
    void this.refreshActivity();
  }

  /** 失去活动资格(别的标签接管):停流保状态。 */
  deactivate(): void {
    this.closeStreams();
    this.setConnection("reconnecting");
  }

  dispose(): void {
    this.disposed = true;
    this.closeStreams();
  }

  retry(): void {
    this.attempts.timeline = 0;
    this.attempts.events = 0;
    this.connectAll();
  }

  private setConnection(kind: ConnectionKind): void {
    this.connection[1](kind);
  }

  private connectAll(): void {
    this.connectTimeline();
    this.connectEvents();
  }

  private closeStreams(): void {
    for (const source of this.sources.values()) source.close();
    this.sources.clear();
    if (this.timers.timeline) clearTimeout(this.timers.timeline);
    if (this.timers.events) clearTimeout(this.timers.events);
    this.timers.timeline = null;
    this.timers.events = null;
  }

  // ── timeline SSE(结构性事实 + 文本) ────────────────────────────────────────

  private connectTimeline(): void {
    const previous = this.sources.get("timeline");
    previous?.close();
    const source = new EventSource(transport.timelineSseUrl(this.seed, this.lastTimelineEventId || null));
    this.sources.set("timeline", source);
    this.setConnection(this.attempts.timeline > 0 ? "reconnecting" : "reconnecting");
    source.addEventListener("timeline.entry", (event) => {
      const messageId = (event as MessageEvent).lastEventId;
      if (messageId) this.lastTimelineEventId = messageId;
      let parsed: TimelineEntryWire;
      try {
        parsed = JSON.parse((event as MessageEvent).data) as TimelineEntryWire;
      } catch {
        return;
      }
      this.onTimelineEntry(parsed);
    });
    source.addEventListener("ringing.stream_terminated", () => {
      // 服务端缓冲溢出,要求客户端 re-baseline:丢弃游标,快照校正后重连。
      this.lastTimelineEventId = "";
      this.attempts.timeline = 0;
      void this.resnapshot().then(() => this.connectTimeline());
    });
    source.onopen = () => {
      const hadFailures = this.attempts.timeline > 0;
      this.attempts.timeline = 0;
      this.setConnection("connected");
      // 重连成功后以快照为准恢复(spec §15.2):重放窗口可能不够/epoch 可能已变。
      if (hadFailures) void this.resnapshot();
    };
    source.onerror = () => {
      source.close();
      this.sources.delete("timeline");
      this.scheduleReconnect("timeline");
    };
  }

  private scheduleReconnect(kind: "timeline" | "events"): void {
    this.attempts[kind] += 1;
    if (exhausted(this.attempts[kind])) {
      this.setConnection("offline");
      return;
    }
    this.setConnection("reconnecting");
    const delay = backoffDelayMs(this.attempts[kind]);
    this.timers[kind] = setTimeout(() => {
      this.timers[kind] = null;
      if (this.disposed) return;
      if (kind === "timeline") this.connectTimeline();
      else this.connectEvents();
    }, delay);
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

  /** 快照重建(attach/重连/缺口/流终止后的权威对齐)。 */
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

  // ── 投影事件流(刷新信号) ─────────────────────────────────────────────────────

  private connectEvents(): void {
    const previous = this.sources.get("events");
    previous?.close();
    const source = new EventSource(transport.eventsUrl(this.seed, this.lastEventsCursor));
    this.sources.set("events", source);
    source.addEventListener("ringing.event", (event) => {
      let envelope: Record<string, any>;
      try {
        envelope = JSON.parse((event as MessageEvent).data) as Record<string, any>;
      } catch {
        return;
      }
      if (typeof envelope.cursor === "string" && envelope.cursor) this.lastEventsCursor = envelope.cursor;
      if (envelope.session_id && envelope.session_id !== this.seed) return;
      this.applyProjection(envelope.stream_key as Record<string, any> | undefined, envelope.payload as Record<string, any> | undefined);
    });
    source.addEventListener("ringing.reset_required", () => {
      this.lastEventsCursor = null;
      void this.refreshApprovals();
      void this.refreshActivity();
    });
    source.onopen = () => {
      this.attempts.events = 0;
    };
    source.onerror = () => {
      source.close();
      this.sources.delete("events");
      // cursor 过期(400 cursor_expired)时降级纯实时,全量信号兜底。
      this.lastEventsCursor = null;
      this.scheduleReconnect("events");
    };
  }

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
      const bootstrap = (await transport.call<any>(`/__gateway/ringing/sessions/${encodeURIComponent(this.seed)}/bootstrap`)) as any;
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
