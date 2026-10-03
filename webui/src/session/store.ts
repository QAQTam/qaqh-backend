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
import { transport, tauriHost, type ApprovalView, type StreamHandlers, type TimelineStatusWire, type TodoItemWire } from "../lib/transport";
import { now } from "../lib/time";
import { applyEntry, applySnapshot, emptySession, prependPage } from "./reducer";
import { evictForWindow, fillGapHeights } from "./pagination";
import type { SessionState, TimelineEntryWire, Wait } from "./types";
import { normalizeActivity, projectionActions, readProjection, type SessionActivity } from "./projection";

export type ConnectionKind = "connected" | "reconnecting" | "offline";

/** 一帧之内攒下来的同一种增量(按「种类 + 回合 + 块」归并)。 */
interface PendingFragment {
  turnId: string;
  blockId: string;
  /** `text_delta` 的累积文本,或 `tool_progress` 的累积输出。 */
  text: string;
  truncated: boolean;
  /** 已消费到的 seq(取批次里最大的那个)。 */
  seq: number;
  progress: boolean;
}

/** timeline 快照窗口页大小;向上翻页页大小(spec §14.2 limit=30)。 */
const SNAPSHOT_LIMIT = 50;
const PAGE_LIMIT = 30;
/** 缺口触发快照校正的最小间隔(防抖,高吞吐下不连环拉全量)。 */
const RESNAPSHOT_DEBOUNCE_MS = 2_000;

export class SessionStore {
  readonly seed: string;
  readonly state: [SessionState, (fn: (draft: SessionState) => void) => void];

  connection = createSignal<ConnectionKind>("reconnecting");
  activity = createSignal<SessionActivity | null>(null);
  title = createSignal<string | null>(null);
  pending = createSignal<ApprovalView[]>([]);
  todos = createSignal<TodoItemWire[]>([]);
  loadError = createSignal(false);
  hasNewReply = createSignal(false);
  /** 「此前已压缩」分隔锚点回合 key;null = 渲染在顶部。 */
  compactedAfter = createSignal<string | null>(null);
  loadingOlder = createSignal(false);

  /** 宿主事件反订阅。 */
  private hostUnlisten: (() => void) | null = null;
  /**
   * 帧内合并的高频增量(`text_delta` 与 `tool_progress`)。逐条写store 的代价有两层:
   * 每帧 N 次通知,以及 `tool_progress` 每次都要重做一遍 16KB 尾窗切片
   * (O(条数 × 16KB) 的字符串复制)。实测 400 个 chunk:合并前 2014ms。
   */
  private pendingFragments = new Map<string, PendingFragment>();
  /**
   * 已消费(立即应用,或进了 delta 缓冲)到的最大 seq。store 里的 `watermark` 只
   * 随非 delta 帧推进,所以缺口判定必须看这个更高的值 —— 否则同一批里的第二条
   * delta 就会被当成缺口丢掉(夹具实测:连续喂 78 条只落地 10 条,文本每 2s 跳一次)。
   */
  private consumedSeq = 0;
  private rafHandle: number | null = null;
  private lastResnapshotAt = 0;
  private waitFrom: { kind: Wait["kind"]; at: number } | null = null;
  private lastTurnCount: number | null = null;
  private disposed = false;

  constructor(seed: string) {
    this.seed = seed;
    // `name`:归因(attribution/diagnostics)里这条 store 的节点标成 `session.<key>`,
    // 否则热路径上的 memo 只能靠调用栈认。
    this.state = createStore<SessionState>(emptySession(), { name: "session" });
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

  /** 成为活动标签:先订阅宿主事件(attach 发出的 timeline://status 事件不丢),
   *  再 attach——宿主取的权威首页会作为 `timeline://snapshot` 推回来,所以这里
   *  不再自己补一次 `resnapshot()`(那等于一次 attach 传两页、整表重建两遍)。 */
  async activate(): Promise<void> {
    if (this.disposed) return;
    this.hasNewReply[1](false);
    await this.connectHost();
    await transport.attach(this.seed, SNAPSHOT_LIMIT);
    void this.refreshApprovals();
    void this.refreshTodos();
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
    // 退订后还挂在队列里的 rAF 回调会带着已废弃标签的缓冲写进 store(一次性悬挂,
    // 不是泄漏级问题,但关掉它就不用解释它)。
    if (this.rafHandle != null) {
      cancelAnimationFrame(this.rafHandle);
      this.rafHandle = null;
    }
    this.pendingFragments.clear();
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
        this.applyTimelineStatus(status);
      },
      onProjectionEvent: (seed, envelope) => {
        if (seed !== this.seed) return;
        this.applyProjection(envelope.stream_key as Record<string, any> | undefined, envelope.payload as Record<string, any> | undefined);
      },
      onProjectionReset: (seed) => {
        if (seed !== this.seed) return;
        // 宿主的 v2 流会自动以 snapshot cursor 重订阅;这里刷新派生信号即可。
        void this.refreshApprovals();
        void this.refreshTodos();
        void this.refreshActivity();
      },
      onTimelineSnapshot: (page) => {
        // attach 的权威首页与宿主侧缺口恢复都走这里(前端不再自己补拉首页)。
        if (page.session_id != null && page.session_id !== this.seed) return;
        this.mutate((draft) => {
          applySnapshot(draft, page);
        });
        this.rebaseConsumed(page.snapshot?.watermark ?? 0);
        this.loadError[1](false);
      },
      onIncompatible: () => {},
      onHostError: () => {},
    };
    try {
      this.hostUnlisten = await this.host().subscribe(handlers);
    } catch {
      this.setConnection("offline");
      return;
    }
    // 兜底事件竞态:宿主可能在订阅完成前已发出 open(Tauri 事件不保留给晚到
    // 订阅者),订阅成功后主动查询一次宿主侧当前状态对齐。
    try {
      const status = await this.host().timelineStatus(this.seed);
      if (status != null) this.applyTimelineStatus(status);
    } catch {
      // best-effort:后续 status 事件仍会持续驱动。
    }
  }

  /** timeline 流状态 → connection 信号(其他 seed 的状态忽略)。 */
  private applyTimelineStatus(status: TimelineStatusWire): void {
    if (status.session_id != null && status.session_id !== this.seed) return;
    if (status.status === "open") {
      this.setConnection("connected");
    } else if (status.status === "closed") {
      this.setConnection("offline");
    } else {
      this.setConnection("reconnecting");
    }
  }

  private onTimelineEntry(entry: TimelineEntryWire): void {
    if (typeof entry?.timeline_seq !== "number" || !entry.turn_id) return;
    // 「已经知道到哪」要同时看已提交的水位和已消费的 seq:delta 进缓冲时不写水位。
    const known = Math.max(this.stateGet.watermark, this.consumedSeq);
    if (entry.timeline_seq <= known) return; // 重复(重放)直接丢弃(幂等)
    if (entry.timeline_seq > known + 1 && known > 0) {
      // seq 缺口:丢弃该事件并触发一次快照校正(spec §15.2)。校正按 2s 防抖,
      // 且回合槽按 key 挂载 → 重建只更新数据,不再拆掉整屏 DOM。
      this.scheduleResnapshot();
      return;
    }
    const event = entry.event;
    if (event.type === "text_delta" || event.type === "tool_progress") {
      // 高频增量按「种类 + 块」归并到帧末再写一次(spec §6/§10.2):每帧至多一次
      // store 写入,16KB 尾窗切片也一帧只做一遍。
      const progress = event.type === "tool_progress";
      const blockId = event.block_id;
      const piece = event.type === "tool_progress" ? event.chunk : event.delta;
      // `truncated` 只有 tool_progress 带(text_delta 无此槽)。
      const truncated = event.type === "tool_progress" && event.truncated === true;
      const key = `${progress ? "p" : "t"}\u0000${entry.turn_id}\u0000${blockId}`;
      const buffered = this.pendingFragments.get(key);
      if (buffered) {
        buffered.text += piece;
        buffered.truncated = buffered.truncated || truncated;
        buffered.seq = Math.max(buffered.seq, entry.timeline_seq);
      } else {
        this.pendingFragments.set(key, { turnId: entry.turn_id, blockId, text: piece, truncated, seq: entry.timeline_seq, progress });
      }
      this.consumedSeq = Math.max(this.consumedSeq, entry.timeline_seq);
      this.scheduleFlush();
    } else {
      this.mutate((draft) => {
        applyEntry(draft, entry);
        draft.watermark = entry.timeline_seq;
      });
      this.consumedSeq = Math.max(this.consumedSeq, entry.timeline_seq);
    }
  }

  private scheduleFlush(): void {
    if (this.rafHandle != null) return;
    this.rafHandle = requestAnimationFrame(() => {
      this.rafHandle = null;
      if (this.pendingFragments.size === 0) return;
      const batch = [...this.pendingFragments.values()];
      this.pendingFragments.clear();
      this.mutate((draft) => {
        let maxSeq = draft.watermark;
        for (const item of batch) {
          // 权威快照可能已经覆盖到这段文本(resnapshot 与缓冲竞态):已被水位包含
          // 的再追加一遍就是重复字符。
          if (item.seq <= draft.watermark) continue;
          applyEntry(draft, {
            timeline_seq: item.seq,
            turn_id: item.turnId,
            event: item.progress
              ? { type: "tool_progress", block_id: item.blockId, chunk: item.text, truncated: item.truncated }
              // 合并帧:多条 delta 已并成一段,块内片段号不再有意义(顺序真相是
              // timeline_seq,reducer 也不读 fragment_seq)。
              : { type: "text_delta", block_id: item.blockId, fragment_seq: 0, delta: item.text },
          });
          maxSeq = Math.max(maxSeq, item.seq);
        }
        // 增量同样是「已消费」:水位必须跟着走,否则下一条增量会被判成缺口。
        draft.watermark = maxSeq;
      });
    });
  }

  /**
   * 权威快照落地后重基线消费指针:快照覆盖到 `pageWatermark`,缓冲里比它更新的
   * delta 仍然要保留(那部分文本快照里没有)。这里按快照重设而不是取历史最大值
   * —— 换 epoch 后 seq 可能从更小的数重新开始。
   */
  private rebaseConsumed(pageWatermark: number): void {
    // 取 max 而不是直接重设:这一页可能被 applySnapshot 当陈旧页整页丢弃(那时
    // draft.watermark 停在更高处),把消费指针拉到更低就会让重复/丢失判定错位。
    // 换 epoch 时 applySnapshot 先把 draft.watermark 重设,这里自然跟着降。
    let seq = Math.max(pageWatermark, this.stateGet.watermark);
    for (const item of this.pendingFragments.values()) seq = Math.max(seq, item.seq);
    this.consumedSeq = seq;
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
      this.rebaseConsumed(page.snapshot?.watermark ?? 0);
      this.loadError[1](false);
    } catch {
      // 快照失败不致命:流还在,下一次触发再试。
    }
  }

  // ── 投影事件(刷新信号,宿主转发) ─────────────────────────────────────────────

  /** 前端只把投影事件当刷新信号;数据一律走 approvals RPC / sessions 列表。 */
  private applyProjection(streamKey: unknown, payload: unknown): void {
    const delta = readProjection(streamKey, payload);
    if (!delta.channel || !delta.kind) return;
    for (const action of projectionActions(delta)) {
      switch (action) {
        case "activity":
          // `state` 在内层 data 里(双层 tag/content),读错层就是静默无信号。
          this.activity[1](normalizeActivity(delta.body.state, "projection"));
          break;
        case "approvals":
          void this.refreshApprovals();
          break;
        case "todos":
          void this.refreshTodos();
          break;
        case "compacted": {
          const slots = this.stateGet.slots;
          const anchor = slots.length > 0 ? slots[slots.length - 1]!.key : null;
          this.compactedAfter[1](anchor);
          break;
        }
      }
    }
  }

  async refreshActivity(): Promise<void> {
    try {
      const bootstrap = (await this.host().sessionBootstrap(this.seed)) as any;
      const control = bootstrap?.control?.state ?? {};
      this.activity[1](normalizeActivity(control.activity, "domain"));
    } catch {
      // best-effort;activity 由投影事件持续修正。
    }
  }

  // ── 待办(todo.list 单源) ────────────────────────────────────────────────────

  /** 待办清单:只读 service RPC;刷新由 dashboard_updated 事件/激活/重置驱动。 */
  async refreshTodos(): Promise<void> {
    try {
      const result = await transport.rpc<Record<string, any>>("todo.list");
      const items = result?.items;
      writeStable(this.todos, Array.isArray(items) ? (items as TodoItemWire[]) : []);
    } catch {
      return; // best-effort:下一轮信号再取
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
    // 内容未变不写信号:每次刷新都灌新数组会让授权卡整棵子树按身份重建
    // (反复重挂 = 焦点被抢 + 图形与内存持续增长),而 UI 看上去毫无变化。
    writeStable(this.pending, next);
    this.recordWaitTransition(previous, next);
    this.activity[1](next.length > 0 ? "waiting_user" : this.activityIsWorking() ? "working" : "idle");
  }

  private activityIsWorking(): boolean {
    return this.activity[0]() === "working";
  }

  /** 0→N 开区间 / N→0 闭区间,挂到当前运行回合(spec D3:已工作扣除等待)。 */
  private recordWaitTransition(previous: ApprovalView[], next: ApprovalView[]): void {
    if (previous.length === 0 && next.length > 0) {
      const kind = next[0]!.kind === "ask" || next[0]!.kind === "plan" ? "ask" : "approval";
      this.waitFrom = { kind, at: now() };
    } else if (previous.length > 0 && next.length === 0 && this.waitFrom) {
      const wait: Wait = { turnKey: this.stateGet.activeTurnKey, kind: this.waitFrom.kind, from: this.waitFrom.at, to: now() };
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

  /**
   * 窗口淘汰(spec §14.1):超出 `window` 且远在视口上方的最旧回合替换为等高
   * 占位,并释放其回合数据——只换占位不删数据的话,翻页越深 turns 全量文本
   * 越攒越多(常驻内存只增不减)。返回被淘汰的 slot key 供视图回填高度。
   */
  evictOutOfView(
    window: number,
    screens: number,
    offsetTopOf: (key: string) => number | null,
    viewportTop: number,
    viewportHeight: number,
  ): string[] {
    let evicted: string[] = [];
    this.mutate((draft) => {
      evicted = evictForWindow(draft, window, screens, offsetTopOf, viewportTop, viewportHeight);
    });
    return evicted;
  }

  /**
   * 时间线展开态(spec §7.3)。MUST 走 setter:Solid 2 的 store 代理直写
   * (`state[0].turns[k].expanded = …`)会被静默丢弃,读回来还是旧值。
   */
  toggleTurnExpanded(key: string): void {
    this.mutate((draft) => {
      const turn = draft.turns[key];
      if (turn) turn.expanded = !turn.expanded;
    });
  }

  /** 回合淘汰占位高度回填(由视图量完布局后一次写入整段;逐格写会通知每个下标)。 */
  setPlaceholderHeights(heights: Array<[key: string, height: number]>): void {
    if (heights.length === 0) return;
    const byKey = new Map(heights);
    this.mutate((draft) => {
      fillGapHeights(draft, byKey);
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


/**
 * 内容未变则不写信号:每次刷新都灌新对象会让 `For` 按身份重建整棵子树
 * (授权卡反复重挂 = 焦点被抢 + 反应图形与内存只增不减),而 UI 毫无变化。
 */
function writeStable<T>(signal: readonly [() => T, (value: T) => void], next: T): void {
  const current: T = signal[0]();
  if (current === next) return;
  if (JSON.stringify(current) === JSON.stringify(next)) return;
  signal[1](next);
}
