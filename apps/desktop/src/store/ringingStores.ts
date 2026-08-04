// Ringing 前端三 store（PLAN 前端状态结构）。
//
// Desktop 建立：
//   ControlStore / ConversationStore / ToolStore / SessionPresentationSelector
//
// 每 store 独立维护 per-session/channel 领域状态，由 Ringing 事件驱动；
// selector 合成展示视图，禁止合成旧事件协议。
// reliable 事件到达时先 drain/覆盖同 identity replaceable 状态，再原子应用 terminal。

import type {
  RingingEventEnvelope,
  RingingEvent,
  ControlEvent,
  ConversationEvent,
  ToolEvent,
  SessionState,
  ActivityState,
  AskQuestion,
  PermissionRisk,
  RoundDeltaKind,
  SkillInfo,
  SkillRuntimeInfo,
  TodoItem,
} from "../lib/types/ringing";
import type { DashboardSnapshot } from "../lib/types/ringing/DashboardSnapshot";
import type { UsageInfo } from "../lib/types/ringing/UsageInfo";
import type { ToolResult } from "../lib/types/ringing/ToolResult";
import type { StoreSetter } from "solid-js";

// ────────────────────────────────────────────────────────────────────────────
// 每频道独立维护的连接状态（PLAN：每频道独立重连、cursor、snapshot、健康状态）
// ────────────────────────────────────────────────────────────────────────────

export interface ChannelConnectionState {
  /** SSE 连接状态。 */
  connected: boolean;
  /** 服务端 epoch（SSE 重连时校验）。 */
  serverEpoch: string;
  /** 已确认的 stream cursor（Last-Event-ID 依据）。 */
  cursor: number;
  /** snapshot 是否已应用。 */
  snapshotApplied: boolean;
}

export function initialChannelConnectionState(): ChannelConnectionState {
  return {
    connected: false,
    serverEpoch: "",
    cursor: 0,
    snapshotApplied: false,
  };
}

// ────────────────────────────────────────────────────────────────────────────
// ControlStore
// ────────────────────────────────────────────────────────────────────────────

export interface ActiveAskPlan {
  id: string;
  kind: "ask" | "plan";
  turnId: string;
  questions?: AskQuestion[];
  mode?: "single" | "batch";
  planContent?: string;
  reviewType?: string;
  todoItems?: TodoItem[] | null;
}

export interface ControlState {
  seed: string;
  sessionState: SessionState | null;
  activity: ActivityState | null;
  agentLifecycle: "booting" | "ready" | "stopping" | "stopped" | null;
  activeAskPlan: ActiveAskPlan | null;
  lastNoticeId: string | null;
  lastFailureId: string | null;
  /** 最近一次 DashboardUpdated（replaceable，覆盖式）。 */
  dashboard: DashboardView | null;
  /** 完整 dashboard/activity 快照，不依赖旧 dashboard 事件。 */
  dashboardSnapshot: DashboardSnapshot | null;
  /** Native skill catalog state. */
  skills: {
    available: SkillInfo[];
    active: string[];
    catalogRevision?: string | null;
    operationRevision?: number | null;
    contextEpoch: number;
    tokenBudget: number;
    tokenUsage: number;
    runtime: SkillRuntimeInfo[];
    diagnostics: string[];
  } | null;
}

export interface DashboardView {
  hpConnected: boolean;
  sessionSeed: string;
  toolCallsTotal: number;
  toolFailures: number;
  currentPhase: string;
  streaming: boolean;
}

export function initialControlState(seed: string): ControlState {
  return {
    seed,
    sessionState: null,
    activity: null,
    agentLifecycle: null,
    activeAskPlan: null,
    lastNoticeId: null,
    lastFailureId: null,
    dashboard: null,
    dashboardSnapshot: null,
    skills: null,
  };
}

/** 单事件 reducer（幂等：相同 event_id 只应用一次由上层保证）。 */
export function controlReducer(state: ControlState, event: ControlEvent): ControlState {
  switch (event.type) {
    case "session_state_changed":
      return { ...state, sessionState: event.state };
    case "session_activity_changed":
      return { ...state, activity: event.state };
    case "agent_lifecycle_changed":
      return { ...state, agentLifecycle: event.state };
    case "interaction_requested":
      return {
        ...state,
        activeAskPlan: {
          id: event.interaction_id,
          kind: "ask",
          turnId: event.turn_id,
          questions: event.questions,
          mode: event.mode,
        },
      };
    case "interaction_resolved":
      return state.activeAskPlan?.id === event.interaction_id
        ? { ...state, activeAskPlan: null }
        : state;
    case "plan_review_requested":
      return {
        ...state,
        activeAskPlan: {
          id: event.interaction_id,
          kind: "plan",
          turnId: event.turn_id,
          planContent: event.plan_content,
          reviewType: event.review_type,
          todoItems: event.todo_items,
        },
      };
    case "plan_review_resolved":
      return state.activeAskPlan?.id === event.interaction_id
        ? { ...state, activeAskPlan: null }
        : state;
    case "operation_failed":
      return { ...state, lastFailureId: event.error.error_id };
    case "operation_completed":
      return state;
    case "system_notice":
      return { ...state, lastNoticeId: event.notice_id };
    case "dashboard_updated":
      return {
        ...state,
        dashboard: {
          hpConnected: event.hp_connected,
          sessionSeed: event.session_seed,
          toolCallsTotal: event.tool_calls_total,
          toolFailures: event.tool_failures,
          currentPhase: event.current_phase,
          streaming: event.streaming,
        },
      };
    case "dashboard_snapshot":
      return { ...state, dashboardSnapshot: event.snapshot };
    case "skills_updated":
      return {
        ...state,
        skills: {
          available: event.available,
          active: event.active,
          catalogRevision: event.catalog_revision,
          operationRevision: event.operation_revision,
          // 旧 daemon 事件不含新字段：serde(default) 保证新 daemon 全量，
          // 这里对缺省值兜底，避免 undefined 泄漏到渲染层。
          contextEpoch: Number(event.context_epoch ?? 0),
          tokenBudget: Number(event.token_budget ?? 0),
          tokenUsage: Number(event.token_usage ?? 0),
          runtime: event.runtime ?? [],
          diagnostics: event.diagnostics ?? [],
        },
      };
    default:
      return state;
  }
}

// ────────────────────────────────────────────────────────────────────────────
// ConversationStore
// ────────────────────────────────────────────────────────────────────────────

export interface RoundView {
  roundNum: number;
  thinking: string;
  answer: string;
  isFinal: boolean;
}

export interface TurnView {
  turnId: string;
  userText: string;
  rounds: RoundView[];
  status: "running" | "completed" | "failed" | "cancelled";
  lastRoundNum: number;
  /** turn 开始时间（本地时钟；snapshot 恢复时为恢复时刻）。 */
  startedAt?: number;
  /** 该 turn 收到最后一个领域事件的时间（卡死检测依据）。 */
  lastActivityAt?: number;
  failure?: { code: string; message: string };
}

export interface ConversationState {
  seed: string;
  activeTurn: TurnView | null;
  turns: TurnView[];
  /** turnId → turns 索引（O(1) 寻址，长会话免每事件 findIndex 扫描）。 */
  turnsById: Map<string, number>;
  compactStatus: "running" | "completed" | "skipped" | "failed" | "cancelled" | null;
  /** CompactProgress delta 累积（compact_started 重置；replaceable 追加语义）。 */
  compactText: string;
  /** CompactFinished.turns_compacted（终态后保留，UI 显示"压缩 N 轮"）。 */
  compactTurnsCompacted: number | null;
  /** 每次 CompactFinished 递增——驱动 ChatView 的"完成"横幅（4s 自动消失）。 */
  compactCompletionRevision: number;
  cancelled: boolean;
  /** 已作废的 revision（terminal 到达后旧 progress 不再渲染）。 */
  staleRevision: number;
  /** 防御乱序/快照间隙：turn_started 到达前先缓冲增量，绝不丢弃。 */
  pendingDeltas: Array<{
    turnId: string;
    roundNum: number;
    kind: RoundDeltaKind;
    delta: string;
  }>;
  /** 最近一次 UsageUpdated（replaceable，按 turn/round 覆盖）。 */
  lastUsage: { usage: UsageInfo; contextLimit: number; model: string } | null;
  usageTotals: UsageInfo;
  usageRequestCount: number;
  cacheReportedRequestCount: number;
  totalTurns: number;
  hasMore: boolean;
  /** 最近一次 ProviderToolStatus（如 web_search 状态）。 */
  lastProviderToolStatus: { callId: string; toolKind: string; state: string } | null;
}

export function initialConversationState(seed: string): ConversationState {
  const usageTotals: UsageInfo = {
    prompt_tokens: 0,
    completion_tokens: 0,
    total_tokens: 0,
    prompt_cache_hit_tokens: 0,
    prompt_cache_miss_tokens: 0,
    reasoning_tokens: 0,
    cache_usage_reported: false,
  };
  return {
    seed,
    activeTurn: null,
    turns: [],
    turnsById: new Map(),
    compactStatus: null,
    compactText: "",
    compactTurnsCompacted: null,
    compactCompletionRevision: 0,
    cancelled: false,
    staleRevision: 0,
    pendingDeltas: [],
    lastUsage: null,
    usageTotals,
    usageRequestCount: 0,
    cacheReportedRequestCount: 0,
    totalTurns: 0,
    hasMore: false,
    lastProviderToolStatus: null,
  };
}

/** 服务器快照里的中立 turn 形状（conversation_snapshot.rs 的 neutral_turn）。 */
export interface ConversationSnapshotTurn {
  turn_id: string;
  user_text?: string;
  rounds?: Array<{
    round_num: number;
    is_final?: boolean;
    thinking?: string | null;
    answer?: string | null;
  }>;
}

function addUsage(left: UsageInfo, right: UsageInfo): UsageInfo {
  return {
    prompt_tokens: left.prompt_tokens + right.prompt_tokens,
    completion_tokens: left.completion_tokens + right.completion_tokens,
    total_tokens: left.total_tokens + right.total_tokens,
    prompt_cache_hit_tokens: left.prompt_cache_hit_tokens + right.prompt_cache_hit_tokens,
    prompt_cache_miss_tokens: left.prompt_cache_miss_tokens + right.prompt_cache_miss_tokens,
    reasoning_tokens: left.reasoning_tokens + right.reasoning_tokens,
    cache_usage_reported: left.cache_usage_reported === true || right.cache_usage_reported === true,
  };
}

/**
 * 把服务器 ConversationSnapshot 合并进本地 store。
 *
 * 快照是 cursor reset/bootstrap 后的权威恢复点。不能只补缺失 turn：如果
 * renderer 在 SSE 缺口中已经创建了同一 turn，保留它的局部文本会永久吞掉
 * 缺失的 thinking/answer。snapshot baseline 之后的 live events 会继续追加。
 */
export function applyConversationSnapshot(
  state: ConversationState,
  turns: ConversationSnapshotTurn[],
  activeTurnId: string | null,
  usage?: UsageInfo | null,
  usageTotals?: UsageInfo | null,
  usageRequestCount?: number,
  cacheReportedRequestCount?: number,
  totalTurns?: number,
  hasMore?: boolean,
  model?: string | null,
  contextLimit?: number,
): ConversationState {
  const snapshots = new Map<string, TurnView>();
  for (const raw of turns) {
    if (!raw?.turn_id) continue;
    const rounds: RoundView[] = (raw.rounds ?? []).map((r) => ({
      roundNum: r.round_num,
      thinking: r.thinking ?? "",
      answer: r.answer ?? "",
      isFinal: Boolean(r.is_final),
    }));
    snapshots.set(raw.turn_id, {
      turnId: raw.turn_id,
      userText: raw.user_text ?? "",
      rounds,
      status: raw.turn_id === activeTurnId ? "running" : "completed",
      lastRoundNum: rounds.reduce((max, r) => Math.max(max, r.roundNum), 0),
      startedAt: Date.now(),
      lastActivityAt: Date.now(),
    });
  }
  const existingIds = new Set(state.turns.map((turn) => turn.turnId));
  const turnsAll = [
    ...state.turns.map((turn) => snapshots.get(turn.turnId) ?? turn),
    ...[...snapshots.values()].filter((turn) => !existingIds.has(turn.turnId)),
  ];
  const turnsById = new Map<string, number>();
  turnsAll.forEach((turn, index) => turnsById.set(turn.turnId, index));
  const activeTurn = turnsAll.find((t) => t.turnId === activeTurnId) ?? state.activeTurn;
  return {
    ...state,
    turns: turnsAll,
    turnsById,
    activeTurn,
    lastUsage: usage ? {
      usage,
      // 快照优先（daemon bootstrap 携带会话实际 model 与当前 context_limit）；
      // 旧值兜底，避免快照缺字段时清空已有数据。
      contextLimit: contextLimit ?? state.lastUsage?.contextLimit ?? 0,
      model: model || state.lastUsage?.model || "",
    } : state.lastUsage,
    usageTotals: usageTotals ?? state.usageTotals,
    usageRequestCount: Number.isSafeInteger(usageRequestCount) ? Math.max(0, usageRequestCount!) : state.usageRequestCount,
    cacheReportedRequestCount: Number.isSafeInteger(cacheReportedRequestCount) ? Math.max(0, cacheReportedRequestCount!) : state.cacheReportedRequestCount,
    totalTurns: Number.isSafeInteger(totalTurns) ? Math.max(0, totalTurns!) : Math.max(state.totalTurns, turnsAll.length),
    hasMore: typeof hasMore === "boolean" ? hasMore : state.hasMore,
  };
}

// ────────────────────────────────────────────────────────────────────────────
// Store path 应用器（Solid 2.0 定向更新 · C1 单一实现）
// ────────────────────────────────────────────────────────────────────────────
// 领域事件 → ConversationState draft 的可变应用（唯一实现，已删除不可变
// conversationReducer 双实现）。**元素级替换**：只重建变化的 turn/round 对象，
// 不复制整个 turns 数组。长会话下每事件分配从 O(turns) 降为 O(1)，未变化的
// 元素引用天然稳定（投影缓存命中、Solid 跳过未变化子树）。事件应用后由上层
// （ringingMonitor）bump ringingVersion 驱动投影刷新——元素级写入不会通知
// "读 turns 数组整体"的表达式。

function applyRoundDeltaToTurn(
  turn: TurnView,
  roundNum: number,
  kind: RoundDeltaKind,
  delta: string,
): TurnView {
  const rounds = [...turn.rounds];
  const idx = rounds.findIndex((r) => r.roundNum === roundNum);
  if (idx < 0) {
    rounds.push({
      roundNum,
      thinking: kind === "thinking" ? delta : "",
      answer: kind === "answering" ? delta : "",
      isFinal: false,
    });
  } else {
    const r = rounds[idx];
    rounds[idx] = {
      ...r,
      thinking: kind === "thinking" ? r.thinking + delta : r.thinking,
      answer: kind === "answering" ? r.answer + delta : r.answer,
    };
  }
  return { ...turn, rounds, lastRoundNum: Math.max(turn.lastRoundNum, roundNum) };
}

/** turnId → turns 索引（O(1)）。索引可能因外部替换过期，校验失败回退线性扫描并修复。 */
function turnIndex(state: Pick<ConversationState, "turns" | "turnsById">, turnId: string): number {
  const idx = state.turnsById.get(turnId);
  if (idx !== undefined && state.turns[idx]?.turnId === turnId) return idx;
  const found = state.turns.findIndex((t) => t.turnId === turnId);
  if (found >= 0) state.turnsById.set(turnId, found);
  return found;
}

export function applyConversationEventToStore(
  setStores: StoreSetter<RingingStores>,
  event: ConversationEvent,
): void {
  setStores((draft) => {
    applyConversationEventToDraft(draft.conversation as ConversationState, event);
  });
}

/** 领域事件 → ConversationState draft（C1 单一实现；生产与测试共用）。 */
export function applyConversationEventToDraft(
  conv: ConversationState,
  event: ConversationEvent,
): void {
  // 任何领域事件都视为活动：刷新当前 turn 的活动时间（卡死检测依据）。
  // turn_started 本身会创建新 turn，无需（也不能）刷新旧 turn。
  if (conv.activeTurn && event.type !== "turn_started") {
      const now = Date.now();
      conv.activeTurn = { ...conv.activeTurn, lastActivityAt: now };
      const activeIdx = turnIndex(conv, conv.activeTurn!.turnId);
      if (activeIdx >= 0) {
        conv.turns[activeIdx] = { ...conv.turns[activeIdx], lastActivityAt: now };
      }
    }
    switch (event.type) {
      case "turn_started": {
        // 合并乱序到达的缓冲增量（快照间隙/事件乱序时不丢字）。
        const buffered = conv.pendingDeltas.filter((d) => d.turnId === event.turn_id);
        let turn: TurnView = {
          turnId: event.turn_id,
          userText: event.user_text,
          rounds: [],
          status: "running",
          lastRoundNum: 0,
          startedAt: Date.now(),
          lastActivityAt: Date.now(),
        };
        if (buffered.length > 0) {
          conv.pendingDeltas = conv.pendingDeltas.filter((d) => d.turnId !== event.turn_id);
          for (const d of buffered) {
            turn = applyRoundDeltaToTurn(turn, d.roundNum, d.kind, d.delta);
          }
        }
        conv.turns.push(turn);
        conv.turnsById.set(turn.turnId, conv.turns.length - 1);
        conv.activeTurn = turn;
        conv.cancelled = false;
        return;
      }
      case "round_delta":
        if (conv.activeTurn?.turnId !== event.turn_id) {
          // 防御乱序/快照间隙：缓冲而不是丢弃，turn_started 到达后合并。
          conv.pendingDeltas = [
            ...conv.pendingDeltas,
            {
              turnId: event.turn_id,
              roundNum: event.round_num,
              kind: event.kind,
              delta: event.delta,
            },
          ];
          return;
        }
        {
          const idx = turnIndex(conv, event.turn_id);
          if (idx < 0) {
            // activeTurn 匹配但 turn 缺失（不应发生）：缓冲保底，不丢字。
            conv.pendingDeltas = [
              ...conv.pendingDeltas,
              {
                turnId: event.turn_id,
                roundNum: event.round_num,
                kind: event.kind,
                delta: event.delta,
              },
            ];
            return;
          }
          const updated = applyRoundDeltaToTurn(conv.turns[idx], event.round_num, event.kind, event.delta);
          conv.turns[idx] = updated;
          // activeTurn 与 turns 元素必须指向同一对象（与 reducer 的 upsertTurn 一致）。
          if (conv.activeTurn?.turnId === event.turn_id) conv.activeTurn = updated;
        }
        return;
      case "block_checkpoint": {
        // A1 replaceable 完整值：覆盖写入；turn 未就绪时忽略（下次 checkpoint 自愈）。
        if (conv.activeTurn?.turnId !== event.turn_id) return;
        const idx = turnIndex(conv, event.turn_id);
        if (idx < 0) return;
        const turn = conv.turns[idx];
        const rounds = [...turn.rounds];
        const rIdx = rounds.findIndex((r) => r.roundNum === event.round_num);
        if (rIdx >= 0) {
          const r = rounds[rIdx];
          rounds[rIdx] = {
            ...r,
            thinking: event.kind === "thinking" ? event.text : r.thinking,
            answer: event.kind === "answering" ? event.text : r.answer,
          };
        } else {
          rounds.push({
            roundNum: event.round_num,
            thinking: event.kind === "thinking" ? event.text : "",
            answer: event.kind === "answering" ? event.text : "",
            isFinal: false,
          });
        }
        const updated = {
          ...turn,
          rounds,
          lastRoundNum: Math.max(turn.lastRoundNum, event.round_num),
        };
        conv.turns[idx] = updated;
        if (conv.activeTurn?.turnId === event.turn_id) conv.activeTurn = updated;
        return;
      }
      case "round_completed": {
        if (!conv.activeTurn || conv.activeTurn.turnId !== event.turn_id) return;
        const idx = turnIndex(conv, event.turn_id);
        if (idx < 0) return;
        const turn = conv.turns[idx];
        const rounds = [...turn.rounds];
        const rIdx = rounds.findIndex((r) => r.roundNum === event.round_num);
        if (rIdx >= 0) {
          rounds[rIdx] = {
            ...rounds[rIdx],
            ...(event.thinking ? { thinking: event.thinking } : {}),
            ...(event.answer ? { answer: event.answer } : {}),
            isFinal: event.is_final,
          };
        } else {
          rounds.push({
            roundNum: event.round_num,
            thinking: event.thinking ?? "",
            answer: event.answer ?? "",
            isFinal: event.is_final,
          });
        }
        const updated = { ...turn, rounds };
        conv.turns[idx] = updated;
        if (conv.activeTurn?.turnId === event.turn_id) conv.activeTurn = updated;
        return;
      }
      case "usage_updated": {
        const usage = event.usage as UsageInfo;
        conv.lastUsage = {
          usage,
          contextLimit: event.context_limit,
          model: event.model,
        };
        conv.usageTotals = addUsage(conv.usageTotals, usage);
        conv.usageRequestCount += 1;
        conv.cacheReportedRequestCount += usage.cache_usage_reported === true ? 1 : 0;
        return;
      }
      case "provider_tool_status":
        conv.lastProviderToolStatus = {
          callId: event.call_id,
          toolKind: event.tool_kind,
          state: event.state,
        };
        return;
      case "turn_completed":
        conv.pendingDeltas = conv.pendingDeltas.filter((d) => d.turnId !== event.turn_id);
        {
          const idx = turnIndex(conv, event.turn_id);
          if (idx >= 0) {
            // StoreNode 窄化后 status 联合仍可能被宽化：显式标注 TurnView。
            const updated: TurnView = { ...(conv.turns[idx] as TurnView), status: "completed" };
            conv.turns[idx] = updated;
            if (conv.activeTurn?.turnId === event.turn_id) conv.activeTurn = updated;
          }
        }
        return;
      case "turn_failed":
        conv.pendingDeltas = conv.pendingDeltas.filter((d) => d.turnId !== event.turn_id);
        {
          const failure = { code: event.error.code, message: event.error.message };
          const idx = turnIndex(conv, event.turn_id);
          if (idx >= 0) {
            const updated: TurnView = { ...(conv.turns[idx] as TurnView), status: "failed", failure };
            conv.turns[idx] = updated;
            if (conv.activeTurn?.turnId === event.turn_id) conv.activeTurn = updated;
          }
        }
        return;
      case "conversation_cancelled":
        conv.cancelled = true;
        if (conv.activeTurn) conv.activeTurn = { ...conv.activeTurn, status: "cancelled" };
        conv.staleRevision += 1;
        if (event.turn_id) {
          conv.pendingDeltas = conv.pendingDeltas.filter((d) => d.turnId !== event.turn_id);
        }
        return;
      case "compact_started":
        conv.compactStatus = "running";
        conv.compactText = "";
        return;
      case "compact_progress":
        conv.compactText += event.delta;
        return;
      case "compact_finished":
        conv.compactStatus = event.status;
        conv.compactTurnsCompacted = event.turns_compacted ?? null;
        conv.compactCompletionRevision += 1;
        return;
      default:
        return;
    }
  }

// ────────────────────────────────────────────────────────────────────────────
// ToolStore
// ────────────────────────────────────────────────────────────────────────────

export interface ToolCardView {
  toolCallId: string;
  turnId: string;
  roundNum: number;
  name: string;
  argsSoFar: string;
  status: "prepared" | "running" | "finished" | "failed";
  progressStream: "stdout" | "stderr";
  progressTail: string;
  progressSeqEnd: number;
  droppedBytes: number;
  truncated: boolean;
  result: ToolResult | null;
  /** Canonical terminal status; lifecycle `status` above only tracks delivery. */
  pendingPermission: boolean;
  /** 权限请求详情（ToolPermissionRequested 完整字段；pendingPermission 为 false 后可保留）。 */
  permission: {
    reason: string;
    paths: string[];
    category: string;
    level: number;
    risk: PermissionRisk;
    consequence: string;
  } | null;
}

export interface ToolState {
  seed: string;
  cards: ToolCardView[];
  /** 最近工具域通知（有界，最多保留 50 条）。 */
  notices: Array<{ level: string; message: string }>;
  /** 最近审计记录（有界，最多保留 50 条）。 */
  audits: Array<{ toolName: string; resultSummary: string; success: boolean; time: string }>;
}

export function initialToolState(seed: string): ToolState {
  return { seed, cards: [], notices: [], audits: [] };
}

export function toolReducer(state: ToolState, event: ToolEvent): ToolState {
  switch (event.type) {
    case "tool_call_prepared": {
      const existing = state.cards.find((c) => c.toolCallId === event.tool_call_id);
      if (existing) return state;
      return {
        ...state,
        cards: [
          ...state.cards,
          {
            toolCallId: event.tool_call_id,
            turnId: event.turn_id,
            roundNum: event.round_num,
            name: event.name,
            argsSoFar: event.args_so_far,
            status: "prepared",
            progressStream: "stdout",
            progressTail: "",
            progressSeqEnd: 0,
            droppedBytes: 0,
            truncated: false,
            result: null,
            pendingPermission: false,
            permission: null,
          },
        ],
      };
    }
    case "tool_started": {
      const existing = state.cards.find((c) => c.toolCallId === event.tool_call_id);
      if (existing) return patchCard(state, event.tool_call_id, { status: "running" });
      return {
        ...state,
        cards: [
          ...state.cards,
          {
            toolCallId: event.tool_call_id,
            turnId: event.turn_id,
            roundNum: event.round_num,
            name: event.name,
            argsSoFar: "",
            status: "running",
            progressStream: "stdout",
            progressTail: "",
            progressSeqEnd: 0,
            droppedBytes: 0,
            truncated: false,
            result: null,
            pendingPermission: false,
            permission: null,
          },
        ],
      };
    }
    case "tool_progress": {
      const card = state.cards.find((candidate) => candidate.toolCallId === event.tool_call_id);
      if (!card) return state;
      // A2 尾部协议：事件携带完整渲染尾部（后端保证 ≤4KB），总是替换而非拼接；
      // seq 不连续 / 丢 chunk 由下一次尾部自愈（丢字风险消除）。
      return patchCard(state, event.tool_call_id, {
        progressTail: event.chunk,
        progressStream: event.stream === "stderr" ? "stderr" : "stdout",
        progressSeqEnd: event.seq_end,
        droppedBytes: event.dropped_bytes,
        truncated: event.truncated,
      });
    }
    case "tool_finished":
      return patchCard(state, event.tool_call_id, {
        status: "finished",
        result: event.result,
        pendingPermission: false,
      });
    case "tool_permission_requested": {
      const permission = {
        reason: event.reason,
        paths: event.paths,
        category: event.category,
        level: event.level,
        risk: event.risk,
        consequence: event.consequence,
      };
      const existing = state.cards.find((c) => c.toolCallId === event.tool_call_id);
      if (existing) return patchCard(state, event.tool_call_id, { pendingPermission: true, permission });
      return {
        ...state,
        cards: [
          ...state.cards,
          {
            toolCallId: event.tool_call_id,
            turnId: event.turn_id,
            roundNum: event.round_num,
            name: event.tool_name,
            argsSoFar: "",
            status: "prepared",
            progressStream: "stdout",
            progressTail: "",
            progressSeqEnd: 0,
            droppedBytes: 0,
            truncated: false,
            result: null,
            pendingPermission: true,
            permission,
          },
        ],
      };
    }
    case "tool_notice":
      return {
        ...state,
        notices: [
          ...state.notices,
          { level: event.level, message: event.message },
        ].slice(-50),
      };
    case "audit_recorded":
      return {
        ...state,
        audits: [
          ...state.audits,
          {
            toolName: event.tool_name,
            resultSummary: event.result_summary,
            success: event.success,
            time: event.time,
          },
        ].slice(-50),
      };
    default:
      return state;
  }
}

function patchCard(state: ToolState, toolCallId: string, patch: Partial<ToolCardView>): ToolState {
  return {
    ...state,
    cards: state.cards.map((c) => (c.toolCallId === toolCallId ? { ...c, ...patch } : c)),
  };
}

// ────────────────────────────────────────────────────────────────────────────
// 统一 reducer（按 envelope.channel 分发���+ 幂等应用
// ────────────────────────────────────────────────────────────────────────────

export interface RingingStores {
  control: ControlState;
  conversation: ConversationState;
  tool: ToolState;
}

export function initialRingingStores(seed: string): RingingStores {
  return {
    control: initialControlState(seed),
    conversation: initialConversationState(seed),
    tool: initialToolState(seed),
  };
}

/** 已应用 event_id 集合（幂等：相同 event_id 至少一次投递但只应用一次）。 */
export class AppliedEventRegistry {
  private applied = new Set<string>();
  private readonly maxEntries = 16384;

  apply(envelope: RingingEventEnvelope): boolean {
    if (this.applied.has(envelope.event_id)) return false;
    this.applied.add(envelope.event_id);
    if (this.applied.size > this.maxEntries) {
      // 有界：淘汰最早（Set 迭代序 = 插入序）
      const first = this.applied.values().next().value;
      if (first !== undefined) this.applied.delete(first);
    }
    return true;
  }
}

