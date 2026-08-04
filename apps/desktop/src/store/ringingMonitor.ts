// Ringing v1 状态监视器（renderer 进程）。
//
// 职责：
// - 订阅 main 进程转发的 Ringing batch（整批 IPC）；
// - 按 batch.seed 路由到每会话的 RingingStores；
// - 记录频道连接状态与事件计数，供主界面派生视图使用；
// - 协议模式在连接建立时确定；reload 通过 bootstrap 恢复，不再切换频道。

import { createSignal, createStore, type Store, type StoreSetter } from "solid-js";
import {
  AppliedEventRegistry,
  applyConversationSnapshot,
  applyConversationEventToStore,
  conversationReducer,
  controlReducer,
  initialRingingStores,
  toolReducer,
  type ConversationSnapshotTurn,
  type RingingStores,
} from "./ringingStores";
import type { RingingEventBatch } from "../lib/types/ringing";
import type { ConversationEvent } from "../lib/types/ringing/ConversationEvent";

export type ChannelName = "control" | "conversation" | "tool";

export interface ChannelView {
  state: string;
  detail?: string;
}

export interface SeedView {
  applied: number;
  turns: number;
  toolCards: number;
  /** snapshot 摘要（历史恢复的尽力而为展示）。 */
  snapshot?: {
    activeTurnId?: string | null;
    lastCompletedTurnId?: string | null;
    compactStatus?: string | null;
    pendingPermissionId?: string | null;
    agentLifecycle?: string | null;
    sessionState?: string | null;
    cancelled?: boolean;
    /** ConversationSnapshot 携带的完整 turns 数量（完整渲染为前端后续工作）。 */
    snapshotTurns?: number;
  };
}

export interface RingingMonitorState {
  channels: Record<ChannelName, ChannelView>;
  /** seed → 事件计数（本进程收到并应用的事件数） */
  perSeed: Record<string, SeedView>;
  lastBatch?: { seed: string; channel: ChannelName; at: number };
  lastError?: string;
}

const initialState = (): RingingMonitorState => ({
  channels: {
    control: { state: "idle" },
    conversation: { state: "idle" },
    tool: { state: "idle" },
  },
  perSeed: {},
});

interface RingingSessionStoreEntry {
  stores: Store<RingingStores>;
  setStores: StoreSetter<RingingStores>;
  applied: AppliedEventRegistry;
}

export function createRingingMonitor() {
  // Each monitor owns its session registry. Keeping these maps inside the
  // app instance prevents disposed/test monitors from leaking Ringing state
  // into a later renderer view.
  const storesBySeed = new Map<string, RingingSessionStoreEntry>();
  const appliedBySeed = new Map<string, number>();
  // 每 (seed, channel) 在途 snapshot 去重：daemon 下线时避免并发请求风暴
  const snapshotInflight = new Set<string>();
  /** SessionCreate 的因果事件可能先于 ACK 到达，先缓存再由调用方领取。 */
  const createdSeedsByCommand = new Map<string, string>();
  const createdSeedWaiters = new Map<string, Set<{
    resolve: (seed: string) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  }>>();
  /** 每 (seed, channel) 已应用快照的 baseline_seq：其前事件已包含在快照内，跳过。 */
  const baselineBySeed = new Map<string, Partial<Record<ChannelName, number>>>();
  const [state, setState] = createSignal<RingingMonitorState>(initialState());
  // 已建立 Ringing V1 typed store 的会话集合与数据版本。
  const [ringingSeeds, setRingingSeeds] = createSignal<ReadonlySet<string>>(new Set());
  const [ringingVersion, setRingingVersion] = createSignal(0);

  /** 该 seed 是否已由 Ringing V1 bootstrap 激活（主 UI 数据源切换依据）。 */
  function hasStores(seed: string): boolean {
    return ringingSeeds().has(seed);
  }

  function ensureStores(seed: string) {
    if (!storesBySeed.has(seed)) {
      const [stores, setStores] = createStore(initialRingingStores(seed));
      storesBySeed.set(seed, {
        stores,
        setStores,
        applied: new AppliedEventRegistry(),
      });
      appliedBySeed.set(seed, 0);
    }
    return storesBySeed.get(seed)!;
  }

  function handleBatch(batch: RingingEventBatch): void {
    const seed = batch.seed;
    if (!seed) {
      setState((s) => ({ ...s, lastError: "batch missing seed" }));
      return;
    }
    setRingingSeeds((prev) => prev.has(seed) ? prev : new Set(prev).add(seed));
    const storesEntry = ensureStores(seed);
    const baseline = baselineBySeed.get(seed)?.[batch.channel];
    let appliedCount = 0;
    for (const envelope of batch.envelopes) {
      const event = envelope.event as { type?: string; state?: string };
      if (
        batch.channel === "control"
        && event.type === "session_state_changed"
        && event.state === "created"
        && envelope.causation_id
      ) {
        const commandId = envelope.causation_id;
        const waiters = createdSeedWaiters.get(commandId);
        if (waiters) {
          createdSeedsByCommand.delete(commandId);
          createdSeedWaiters.delete(commandId);
          for (const waiter of waiters) {
            clearTimeout(waiter.timer);
            waiter.resolve(seed);
          }
        } else {
          createdSeedsByCommand.set(commandId, seed);
        }
      }
      // 快照已覆盖的事件（≤ baseline_stream_seq）不重复应用，避免与恢复的 turns 双计。
      if (baseline !== undefined && envelope.stream_seq <= baseline) continue;
      // 幂等键 = (channel, to_stream_seq)：SSE cursor 保证每频道 stream_seq
      // 单调且不重发已 ack 事件；重连后 Last-Event-ID 续传天然跳过历史。
      if (storesEntry.applied.apply(envelope)) {
        dispatchToStores(
          storesEntry.stores,
          storesEntry.setStores,
          batch.channel,
          envelope.event as never,
        );
        appliedCount += 1;
      }
    }
    if (appliedCount === 0) return;
    setRingingVersion((v) => v + 1);
    appliedBySeed.set(seed, (appliedBySeed.get(seed) ?? 0) + appliedCount);
    const conv = storesEntry.stores.conversation;
    const tool = storesEntry.stores.tool;
    setState((s) => ({
      ...s,
      perSeed: {
        ...s.perSeed,
        [seed]: {
          applied: appliedBySeed.get(seed) ?? 0,
          turns: conv.turns.length,
          toolCards: tool.cards.length,
        },
      },
      lastBatch: { seed, channel: batch.channel, at: Date.now() },
    }));
  }

  /** 等待 SessionCreate 的因果创建事件，并返回事件信封中的真实 seed。 */
  function waitForSessionCreated(commandId: string, timeoutMs = 15_000): Promise<string> {
    const cached = createdSeedsByCommand.get(commandId);
    if (cached) {
      createdSeedsByCommand.delete(commandId);
      return Promise.resolve(cached);
    }
    return new Promise((resolve, reject) => {
      const waiter = {
        resolve,
        reject,
        timer: setTimeout(() => {
          const waiters = createdSeedWaiters.get(commandId);
          waiters?.delete(waiter);
          if (waiters?.size === 0) createdSeedWaiters.delete(commandId);
          reject(new Error("timed out waiting for Ringing session creation event"));
        }, timeoutMs),
      };
      const waiters = createdSeedWaiters.get(commandId) ?? new Set();
      waiters.add(waiter);
      createdSeedWaiters.set(commandId, waiters);
    });
  }

  /** 标记该连接的 session 使用 Ringing v1，并以三频道 bootstrap 建立基线。 */
  async function activate(seed: string): Promise<void> {
    if (!seed) return;
    const api = window.deepx?.ringing;
    if (!api) return;
    const statuses = await api.status().catch(() => null);
    const connected = statuses
      ? Object.values(statuses).some(status =>
        status?.state === "open" || status?.state === "connected" || status?.state === "connecting",
      )
      : false;
    if (!connected) return;
    setRingingSeeds((prev) => prev.has(seed) ? prev : new Set(prev).add(seed));
    setRingingVersion((v) => v + 1);
    if (api.bootstrap) {
      const bootstrap = await api.bootstrap(seed) as {
        control?: { state?: Record<string, unknown>; baseline_stream_seq?: unknown };
        conversation?: { state?: Record<string, unknown>; baseline_stream_seq?: unknown };
        tool?: { state?: Record<string, unknown>; baseline_stream_seq?: unknown };
      };
      for (const channel of ["control", "conversation", "tool"] as ChannelName[]) {
        applySnapshotPayload(seed, channel, bootstrap[channel] ?? null);
      }
    } else {
      // Test/debug bridges from before the Ringing V1 bootstrap IPC can still recover
      // through the same main-owned per-channel snapshot endpoint.
      await Promise.all(([
        "control", "conversation", "tool",
      ] as ChannelName[]).map((channel) => loadSnapshot(seed, channel)));
    }
  }

  function handleStatus(update: { channel: string; status: unknown }): void {
    const channel = update.channel as ChannelName;
    const s = update.status as { state?: string; reason?: string; cursor?: number; serverEpoch?: string };
    setState((prev) => ({
      ...prev,
      channels: {
        ...prev.channels,
        [channel]: {
          state: s?.state ?? "unknown",
          detail: s?.reason ?? (s?.cursor !== undefined ? `cursor=${s.cursor}` : undefined),
        },
      },
    }));
  }

  /** 应用主进程当前状态快照（renderer 晚订阅时会错过初始 open，必须主动拉取）。 */
  function applyStatusSnapshot(
    statuses: Record<string, { state: string; detail?: string } | null> | null | undefined,
  ): void {
    if (!statuses) return;
    setState((prev) => ({
      ...prev,
      channels: {
        control: normalizeStatus(statuses.control),
        conversation: normalizeStatus(statuses.conversation),
        tool: normalizeStatus(statuses.tool),
      },
    }));
  }

  /** 应用频道领域快照（摘要重建；ConversationSnapshot 现携带完整 turns）。 */
  function applySnapshotPayload(
    seed: string,
    channel: string,
    snap: { state?: Record<string, unknown>; baseline_stream_seq?: unknown } | null,
  ): void {
    const s = (snap?.state ?? {}) as Record<string, string | boolean | null | unknown>;
    ensureStores(seed);
    // 摘要字段应用到 typed store（尽力而为；turns/cards 完整数据由 SSE 增量补齐）
    const storesEntry = storesBySeed.get(seed)!;
    const sc = s as Record<string, string | boolean | null>;
    if (channel === "control") {
      storesEntry.setStores((draft) => {
        draft.control.agentLifecycle = (sc.agent_lifecycle as any) ?? null;
        draft.control.sessionState = (sc.session_state as any) ?? null;
      });
    } else if (channel === "conversation") {
      const nextConversation = applyConversationSnapshot(
        {
          ...storesEntry.stores.conversation,
          compactStatus: (sc.compact_status as any) ?? null,
          cancelled: sc.cancelled === true,
        },
        Array.isArray(s.turns) ? (s.turns as unknown as ConversationSnapshotTurn[]) : [],
        (sc.active_turn as string | null) ?? null,
        (s.usage as any) ?? null,
        (s.usage_totals as any) ?? null,
        asSafeNonNegativeInt(s.usage_requests),
        asSafeNonNegativeInt(s.cache_reported_requests),
        asSafeNonNegativeInt(s.total_turns),
        typeof s.has_more === "boolean" ? s.has_more : undefined,
        (sc.model as string | null) ?? null,
        asSafeNonNegativeInt(s.context_limit),
      );
      // 快照携带完整 turns（neutral JSON）：只补缺失 turn，保留流式现场，
      // 并恢复 activeTurn 使后续 round_delta 能继续追加（修复快照后吞字）。
      storesEntry.setStores((draft) => { draft.conversation = nextConversation; });
    }
    const baselineSeq = Number(snap?.baseline_stream_seq);
    if (Number.isSafeInteger(baselineSeq) && baselineSeq >= 0) {
      const existing = baselineBySeed.get(seed) ?? {};
      existing[channel as ChannelName] = baselineSeq;
      baselineBySeed.set(seed, existing);
    }
    const turns = Array.isArray(s.turns) ? s.turns.length : undefined;
    setRingingVersion((v) => v + 1);
    setState((prev) => ({
      ...prev,
      perSeed: {
        ...prev.perSeed,
        [seed]: {
          ...(prev.perSeed[seed] ?? { applied: 0, turns: 0, toolCards: 0 }),
          snapshot: {
            activeTurnId: (sc.active_turn as string | null) ?? null,
            lastCompletedTurnId: (sc.last_completed_turn as string | null) ?? null,
            compactStatus: (sc.compact_status as string | null) ?? null,
            pendingPermissionId: (sc.pending_permission as string | null) ?? null,
            agentLifecycle: (sc.agent_lifecycle as string | null) ?? null,
            sessionState: (sc.session_state as string | null) ?? null,
            cancelled: sc.cancelled === true,
            snapshotTurns: turns,
          },
        },
      },
    }));
  }

  /** 拉取频道领域快照（bootstrap 后摘要重建）。 */
  async function loadSnapshot(seed: string, channel: ChannelName): Promise<void> {
    const api = window.deepx?.ringing;
    if (!api) return;
    const key = `${seed}/${channel}`;
    if (snapshotInflight.has(key)) return;
    snapshotInflight.add(key);
    try {
      const snap = (await api.snapshot(seed, channel)) as {
        state?: Record<string, unknown>;
        baseline_stream_seq?: unknown;
      };
      applySnapshotPayload(seed, channel, snap);
    } catch (error) {
      console.warn(`[ringing] snapshot ${channel}/${seed} failed`, error);
    } finally {
      snapshotInflight.delete(key);
    }
  }

  function storesFor(seed: string): RingingStores | undefined {
    return storesBySeed.get(seed)?.stores;
  }

  // cursor 超出保留窗口时，main 进程拉取权威 snapshot 后经 IPC 推送
  window.deepx?.ringing.onSnapshot?.((update) =>
    applySnapshotPayload(update.seed, update.channel, update.snapshot as {
      state?: Record<string, unknown>;
        baseline_stream_seq?: unknown;
    }),
  );

  return {
    state,
    ringingVersion,
    handleBatch,
    handleStatus,
    applyStatusSnapshot,
    activate,
    loadSnapshot,
    hasStores,
    storesFor,
    waitForSessionCreated,
  };
}

function normalizeStatus(
  status: { state: string; detail?: string } | null | undefined,
): { state: string; detail?: string } {
  return status ?? { state: "idle" };
}

function asSafeNonNegativeInt(value: unknown): number | undefined {
  const number = typeof value === "number" ? value : Number(value);
  return Number.isSafeInteger(number) && number >= 0 ? number : undefined;
}

// 按 batch.channel 分发到对应 reducer（事件对象为纯领域事件）
function dispatchToStores(
  stores: Store<RingingStores>,
  setStores: StoreSetter<RingingStores>,
  channel: ChannelName,
  event: never,
): void {
  const e = event as { type?: string } & Record<string, unknown>;
  // 必须在 setStores 函数式更新内部基于**最新 draft** reduce：同一 batch
  // 的多个事件（或快速连续的 handleBatch）会排队多个 setter，若在循环外
  // 用旧 state 计算，后一个结果会覆盖前一个（事件丢失 → resume 后
  // transcript 空白，必须切换 session 强制重渲染才显示残缺内容）。
  if (channel === "control") {
    setStores((draft) => { draft.control = controlReducer(draft.control, e as never); });
  } else if (channel === "conversation") {
    // 热路径：path 定向更新（元素级替换，不复制 turns 数组）。
    applyConversationEventToStore(setStores, e as ConversationEvent);
  } else if (channel === "tool") {
    setStores((draft) => { draft.tool = toolReducer(draft.tool, e as never); });
  }
}

export type RingingMonitor = ReturnType<typeof createRingingMonitor>;
