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
  conversationReducer,
  controlReducer,
  initialRingingStores,
  toolReducer,
  type RingingStores,
} from "./ringingStores";
import type { RingingEventBatch } from "../lib/types/ringing";

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
// 已切流会话（至少一个 channel 为 Ringing；reload 后从 main mode 表恢复）
const ringingSeeds = new Set<string>();

export function createRingingMonitor() {
  const [state, setState] = createSignal<RingingMonitorState>(initialState());

  /** 该 seed 是否已切流（主 UI 数据源切换依据）。 */
  function isRinging(seed: string): boolean {
    return ringingSeeds.has(seed);
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
      const anyRinging = Object.values(modes).some((m) => m.eventProtocol === "ringing");
      if (anyRinging) {
        ringingSeeds.add(seed);
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
    let appliedCount = 0;
    for (const event of batch.events) {
      // 幂等键 = (channel, to_stream_seq)：SSE cursor 保证每频道 stream_seq
      // 单调且不重发已 ack 事件；重连后 Last-Event-ID 续传天然跳过历史。
      const key = `${batch.channel}:${batch.to_stream_seq}`;
      if (shadow.applied.apply({ event_id: key } as never)) {
        dispatchToStores(shadow.stores, batch.channel, event as never);
        appliedCount += 1;
      }
    }
    if (appliedCount === 0) return;
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

  /** 切流：prepare → commit → reload（commit 后 legacy 停发该频道，UI 从 Ringing 重建）。 */
  async function cutover(seed: string, channel: ChannelName): Promise<void> {
    const api = window.deepx?.ringing;
    if (!api) throw new Error("ringing bridge unavailable");
    await api.cutoverEvents(seed, channel, "prepare");
    // prepare 后服务端建立边界；commit 原子切换 event owner
    await api.cutoverEvents(seed, channel, "commit");
    ringingSeeds.add(seed);
  }

  /** 应用频道领域快照（切流/reload 后摘要重建；完整历史依赖 ConversationSnapshot HTTP，见 PLAN）。 */
  async function loadSnapshot(seed: string, channel: ChannelName): Promise<void> {
    const api = window.deepx?.ringing;
    if (!api) return;
    try {
      const snap = (await api.snapshot(seed, channel)) as {
        channel: string;
        seed: string;
        baseline_seq: bigint;
        state_revision: bigint;
        state: Record<string, unknown>;
      };
      const s = snap.state ?? {};
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
      }
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
            },
          },
        },
      }));
    } catch (error) {
      console.warn(`[ringing] snapshot ${channel}/${seed} failed`, error);
    }
  }

  function shadowOf(seed: string): RingingStores | undefined {
    return shadowBySeed.get(seed)?.stores;
  }

  return { state, handleBatch, handleStatus, cutover, loadSnapshot, syncMode, isRinging, shadowOf };
}

// 按 batch.channel 分发到对应 reducer（事件对象为纯领域事件）
function dispatchToStores(stores: RingingStores, channel: ChannelName, event: never): void {
  const e = event as { type?: string } & Record<string, unknown>;
  if (channel === "control") stores.control = controlReducer(stores.control, e as any);
  else if (channel === "conversation") stores.conversation = conversationReducer(stores.conversation, e as any);
  else if (channel === "tool") stores.tool = toolReducer(stores.tool, e as any);
}

export type RingingMonitor = ReturnType<typeof createRingingMonitor>;
