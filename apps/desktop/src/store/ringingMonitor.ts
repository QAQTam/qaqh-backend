// Ringing 影子监视器（renderer 进程）。
//
// 职责：
// - 订阅 main 进程转发的 Ringing batch（整批 IPC）；
// - 按 batch.seed 路由到每会话的 RingingStores（影子状态，不参与主渲染）；
// - 记录频道连接状态与事件计数，供调试面板展示；
// - 提供切流动作（prepare → commit → reload 页面，让 UI 从 Ringing store 重建）。
//
// 影子模式：legacy 主渲染不受影响，Ringing 事件旁路进 store——
// 这是"新实现真实运行"的零风险验证入口。

import { createSignal } from "solid-js";
import {
  AppliedEventRegistry,
  applyConversationSnapshot,
  conversationReducer,
  controlReducer,
  initialRingingStores,
  toolReducer,
  type ConversationSnapshotTurn,
  type RingingStores,
} from "./ringingStores";
import type { RingingEventBatch } from "../lib/types/ringing";
import { applyCommandModes } from "../runtime/ringingCommandRouter";

export type ChannelName = "control" | "conversation" | "tool";

export interface ChannelView {
  state: string;
  detail?: string;
}

export interface SeedView {
  applied: number;
  turns: number;
  toolCards: number;
  /** snapshot 摘要（切流后历史恢复的尽力而为展示）。 */
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

// 每会话影子 store（不入主渲染树；调试面板只读展示）
const shadowBySeed = new Map<string, { stores: RingingStores; applied: AppliedEventRegistry }>();
const appliedBySeed = new Map<string, number>();
// 每 (seed, channel) 在途 snapshot 去重：daemon 下线时避免并发请求风暴
const snapshotInflight = new Set<string>();
/** 每 (seed, channel) 已应用快照的 baseline_seq：其前事件已包含在快照内，跳过。 */
const baselineBySeed = new Map<string, Partial<Record<ChannelName, number>>>();

export function createRingingMonitor() {
  const [state, setState] = createSignal<RingingMonitorState>(initialState());
  // 已切流会话集合与数据版本：必须是 signal（App.tsx rawSession 依赖它们，
  // 影子 store 是普通对象，版本信号是 ChatView 感知变化的唯一依赖）。
  const [ringingSeeds, setRingingSeeds] = createSignal<ReadonlySet<string>>(new Set());
  const [ringingVersion, setRingingVersion] = createSignal(0);

  /** 该 seed 是否已切流（主 UI 数据源切换依据）。 */
  function isRinging(seed: string): boolean {
    return ringingSeeds().has(seed);
  }

  /** 从 main 进程 sessionChannelMode 表同步切流状态（reload 后恢复）。 */
  async function syncMode(seed: string): Promise<void> {
    const api = window.deepx?.ringing;
    if (!api) return;
    try {
      const modes = (await api.mode(seed)) as Record<
        string,
        { eventProtocol: string; commandProtocol: string }
      >;
      applyCommandModes(seed, modes);
      const anyRinging = Object.values(modes).some((m) => m.eventProtocol === "ringing");
      if (anyRinging) {
        setRingingSeeds((prev) => new Set(prev).add(seed));
        setRingingVersion((v) => v + 1);
        const chans = (Object.entries(modes)
          .filter(([, m]) => m.eventProtocol === "ringing")
          .map(([c]) => c) as ChannelName[]);
        for (const ch of chans) void loadSnapshot(seed, ch);
      }
    } catch (error) {
      console.warn(`[ringing] syncMode ${seed} failed`, error);
    }
  }

  function ensureShadow(seed: string) {
    if (!shadowBySeed.has(seed)) {
      shadowBySeed.set(seed, {
        stores: initialRingingStores(seed),
        applied: new AppliedEventRegistry(),
      });
      appliedBySeed.set(seed, 0);
    }
    return shadowBySeed.get(seed)!;
  }

  function handleBatch(batch: RingingEventBatch): void {
    const seed = batch.seed;
    if (!seed) {
      setState((s) => ({ ...s, lastError: "batch missing seed" }));
      return;
    }
    const shadow = ensureShadow(seed);
    const baseline = baselineBySeed.get(seed)?.[batch.channel];
    const seq = Number(batch.to_stream_seq);
    let appliedCount = 0;
    for (const event of batch.events) {
      // 快照已覆盖的事件（≤ baseline_seq）不重复应用，避免与恢复的 turns 双计。
      if (baseline !== undefined && seq <= baseline) continue;
      // 幂等键 = (channel, to_stream_seq)：SSE cursor 保证每频道 stream_seq
      // 单调且不重发已 ack 事件；重连后 Last-Event-ID 续传天然跳过历史。
      const key = `${batch.channel}:${batch.to_stream_seq}`;
      if (shadow.applied.apply({ event_id: key } as never)) {
        dispatchToStores(shadow.stores, batch.channel, event as never);
        appliedCount += 1;
      }
    }
    if (appliedCount === 0) return;
    setRingingVersion((v) => v + 1);
    appliedBySeed.set(seed, (appliedBySeed.get(seed) ?? 0) + appliedCount);
    const conv = shadow.stores.conversation;
    const tool = shadow.stores.tool;
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

  /** 切流：prepare → commit → reload（commit 后 legacy 停发该频道，UI 从 Ringing 重建）。 */
  async function cutover(seed: string, channel: ChannelName): Promise<void> {
    const api = window.deepx?.ringing;
    if (!api) throw new Error("ringing bridge unavailable");
    await api.cutoverEvents(seed, channel, "prepare");
    // prepare 后服务端建立边界；commit 原子切换 event owner
    await api.cutoverEvents(seed, channel, "commit");
    setRingingSeeds((prev) => new Set(prev).add(seed));
    setRingingVersion((v) => v + 1);
  }

  /** 应用频道领域快照（摘要重建；ConversationSnapshot 现携带完整 turns）。 */
  function applySnapshotPayload(
    seed: string,
    channel: string,
    snap: { state?: Record<string, unknown>; baseline_seq?: unknown } | null,
  ): void {
    const s = (snap?.state ?? {}) as Record<string, string | boolean | null | unknown>;
    ensureShadow(seed);
    // 摘要字段应用到影子 store（尽力而为；turns/cards 完整数据由 SSE 增量补齐）
    const shadow = shadowBySeed.get(seed)!;
    const sc = s as Record<string, string | boolean | null>;
    if (channel === "control") {
      shadow.stores.control.agentLifecycle = (sc.agent_lifecycle as any) ?? null;
      shadow.stores.control.sessionState = (sc.session_state as any) ?? null;
    } else if (channel === "conversation") {
      shadow.stores.conversation.compactStatus = (sc.compact_status as any) ?? null;
      shadow.stores.conversation.cancelled = sc.cancelled === true;
      // 快照携带完整 turns（neutral JSON）：只补缺失 turn，保留流式现场，
      // 并恢复 activeTurn 使后续 round_delta 能继续追加（修复快照后吞字）。
      const turns = Array.isArray(s.turns) ? (s.turns as unknown as ConversationSnapshotTurn[]) : [];
      const activeTurnId = (sc.active_turn as string | null) ?? null;
      shadow.stores.conversation = applyConversationSnapshot(
        shadow.stores.conversation,
        turns,
        activeTurnId,
      );
    }
    const baselineSeq = Number(snap?.baseline_seq);
    if (Number.isFinite(baselineSeq) && baselineSeq > 0) {
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

  /** 拉取频道领域快照（切流/reload 后摘要重建）。 */
  async function loadSnapshot(seed: string, channel: ChannelName): Promise<void> {
    const api = window.deepx?.ringing;
    if (!api) return;
    const key = `${seed}/${channel}`;
    if (snapshotInflight.has(key)) return;
    snapshotInflight.add(key);
    try {
      const snap = (await api.snapshot(seed, channel)) as {
        state?: Record<string, unknown>;
        baseline_seq?: unknown;
      };
      applySnapshotPayload(seed, channel, snap);
    } catch (error) {
      console.warn(`[ringing] snapshot ${channel}/${seed} failed`, error);
    } finally {
      snapshotInflight.delete(key);
    }
  }

  function shadowOf(seed: string): RingingStores | undefined {
    return shadowBySeed.get(seed)?.stores;
  }

  // cursor 超出保留窗口时，main 进程拉取权威 snapshot 后经 IPC 推送
  window.deepx?.ringing.onSnapshot?.((update) =>
    applySnapshotPayload(update.seed, update.channel, update.snapshot as {
      state?: Record<string, unknown>;
      baseline_seq?: unknown;
    }),
  );

  return {
    state,
    ringingVersion,
    handleBatch,
    handleStatus,
    applyStatusSnapshot,
    cutover,
    loadSnapshot,
    syncMode,
    isRinging,
    shadowOf,
  };
}

function normalizeStatus(
  status: { state: string; detail?: string } | null | undefined,
): { state: string; detail?: string } {
  return status ?? { state: "idle" };
}

// 按 batch.channel 分发到对应 reducer（事件对象为纯领域事件）
function dispatchToStores(stores: RingingStores, channel: ChannelName, event: never): void {
  const e = event as { type?: string } & Record<string, unknown>;
  if (channel === "control") stores.control = controlReducer(stores.control, e as any);
  else if (channel === "conversation") stores.conversation = conversationReducer(stores.conversation, e as any);
  else if (channel === "tool") stores.tool = toolReducer(stores.tool, e as any);
}

export type RingingMonitor = ReturnType<typeof createRingingMonitor>;
