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
  compactStatus: "completed" | "skipped" | "failed" | "cancelled" | null;
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
    compactStatus: null,
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

function upsertTurn(state: ConversationState, turnId: string, mutate: (t: TurnView) => TurnView): ConversationState {
  const turns = state.turns.map((t) => (t.turnId === turnId ? mutate(t) : t));
  return { ...state, turns, activeTurn: turns.find((t) => t.turnId === turnId) ?? null };
}

function clearPending(state: ConversationState, turnId: string): ConversationState {
  return state.pendingDeltas.some((d) => d.turnId === turnId)
    ? { ...state, pendingDeltas: state.pendingDeltas.filter((d) => d.turnId !== turnId) }
    : state;
}

/** 追加式应用一个 round 增量（首个 delta 创建 round）。 */
function applyRoundDelta(
  state: ConversationState,
  turnId: string,
  roundNum: number,
  kind: RoundDeltaKind,
  delta: string,
): ConversationState {
  return upsertTurn(state, turnId, (t) => {
    const rounds = [...t.rounds];
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
    return { ...t, rounds, lastRoundNum: Math.max(t.lastRoundNum, roundNum) };
  });
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
  const activeTurn = turnsAll.find((t) => t.turnId === activeTurnId) ?? state.activeTurn;
  return {
    ...state,
    turns: turnsAll,
    activeTurn,
    lastUsage: usage ? {
      usage,
      contextLimit: state.lastUsage?.contextLimit ?? 0,
      model: state.lastUsage?.model ?? "",
    } : state.lastUsage,
    usageTotals: usageTotals ?? state.usageTotals,
    usageRequestCount: Number.isSafeInteger(usageRequestCount) ? Math.max(0, usageRequestCount!) : state.usageRequestCount,
    cacheReportedRequestCount: Number.isSafeInteger(cacheReportedRequestCount) ? Math.max(0, cacheReportedRequestCount!) : state.cacheReportedRequestCount,
    totalTurns: Number.isSafeInteger(totalTurns) ? Math.max(0, totalTurns!) : Math.max(state.totalTurns, turnsAll.length),
    hasMore: typeof hasMore === "boolean" ? hasMore : state.hasMore,
  };
}

export function conversationReducer(state: ConversationState, event: ConversationEvent): ConversationState {
  // 任何领域事件都视为活动：刷新当前 turn 的活动时间（卡死检测依据）。
  // turn_started 本身会创建新 turn，无需（也不能）刷新旧 turn。
  if (state.activeTurn && event.type !== "turn_started") {
    const now = Date.now();
    state = {
      ...state,
      activeTurn: { ...state.activeTurn, lastActivityAt: now },
      turns: state.turns.map((t) =>
        t.turnId === state.activeTurn!.turnId ? { ...t, lastActivityAt: now } : t,
      ),
    };
  }
  switch (event.type) {
    case "turn_started": {
      const turn: TurnView = {
        turnId: event.turn_id,
        userText: event.user_text,
        rounds: [],
        status: "running",
        lastRoundNum: 0,
        startedAt: Date.now(),
        lastActivityAt: Date.now(),
      };
      let next: ConversationState = {
        ...state,
        turns: [...state.turns, turn],
        activeTurn: turn,
        cancelled: false,
      };
      // 合并乱序到达的缓冲增量（快照间隙/事件乱序时不丢字）。
      const buffered = state.pendingDeltas.filter((d) => d.turnId === event.turn_id);
      if (buffered.length > 0) {
        next = clearPending(next, event.turn_id);
        for (const d of buffered) {
          next = applyRoundDelta(next, d.turnId, d.roundNum, d.kind, d.delta);
        }
      }
      return next;
    }
    case "round_delta":
      if (state.activeTurn?.turnId !== event.turn_id) {
        // 防御乱序/快照间隙：缓冲而不是丢弃，turn_started 到达后合并。
        const pendingDeltas = [
          ...state.pendingDeltas,
          {
            turnId: event.turn_id,
            roundNum: event.round_num,
            kind: event.kind,
            delta: event.delta,
          },
        ];
        return { ...state, pendingDeltas };
      }
      return applyRoundDelta(state, event.turn_id, event.round_num, event.kind, event.delta);
    case "round_completed": {
      if (!state.activeTurn || state.activeTurn.turnId !== event.turn_id) return state;
      return upsertTurn(state, event.turn_id, (t) => {
        const rounds = [...t.rounds];
        const idx = rounds.findIndex((r) => r.roundNum === event.round_num);
        if (idx >= 0) {
          rounds[idx] = { ...rounds[idx], ...(event.thinking ? { thinking: event.thinking } : {}), ...(event.answer ? { answer: event.answer } : {}), isFinal: event.is_final };
        } else {
          rounds.push({
            roundNum: event.round_num,
            thinking: event.thinking ?? "",
            answer: event.answer ?? "",
            isFinal: event.is_final,
          });
        }
        return { ...t, rounds };
      });
    }
    case "usage_updated":
      {
      const usage = event.usage as UsageInfo;
      return {
        ...state,
        lastUsage: {
          usage,
          contextLimit: event.context_limit,
          model: event.model,
        },
        usageTotals: addUsage(state.usageTotals, usage),
        usageRequestCount: state.usageRequestCount + 1,
        cacheReportedRequestCount: state.cacheReportedRequestCount + (usage.cache_usage_reported === true ? 1 : 0),
      };
      }
    case "provider_tool_status":
      return {
        ...state,
        lastProviderToolStatus: {
          callId: event.call_id,
          toolKind: event.tool_kind,
          state: event.state,
        },
      };
    case "turn_completed":
      return upsertTurn(clearPending(state, event.turn_id), event.turn_id, (t) => ({ ...t, status: "completed" }));
    case "turn_failed":
      return upsertTurn(clearPending(state, event.turn_id), event.turn_id, (t) => ({
        ...t,
        status: "failed",
        failure: { code: event.error.code, message: event.error.message },
      }));
    case "conversation_cancelled":
      return {
        ...state,
        cancelled: true,
        activeTurn: state.activeTurn ? { ...state.activeTurn, status: "cancelled" } : null,
        staleRevision: state.staleRevision + 1,
        pendingDeltas: event.turn_id
          ? state.pendingDeltas.filter((d) => d.turnId !== event.turn_id)
          : state.pendingDeltas,
      };
    case "compact_finished":
      return { ...state, compactStatus: event.status };
    default:
      return state;
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
      const combined = event.seq_start === card.progressSeqEnd
        ? `${card.progressTail}${event.chunk}`
        : event.chunk;
      const maxTail = 128 * 1024;
      const trimmed = Math.max(0, combined.length - maxTail);
      return patchCard(state, event.tool_call_id, {
        progressTail: trimmed > 0 ? combined.slice(-maxTail) : combined,
        progressStream: event.stream === "stderr" ? "stderr" : "stdout",
        progressSeqEnd: event.seq_end,
        droppedBytes: event.dropped_bytes + trimmed,
        truncated: event.truncated || trimmed > 0,
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

/** 应用一个信封（幂等 + 按频道分发）。返回是否发生了状态变更。 */
export function applyEnvelope(
  stores: RingingStores,
  envelope: RingingEventEnvelope,
  applied: AppliedEventRegistry,
): boolean {
  if (!applied.apply(envelope)) return false;
  applyEnvelopeUnchecked(stores, envelope.event);
  return true;
}

/** 无幂等检查的应用（snapshot 重建时直接 apply）。 */
export function applyEnvelopeUnchecked(stores: RingingStores, event: RingingEvent): void {
  switch (event.channel) {
    case "control":
      stores.control = controlReducer(stores.control, event);
      break;
    case "conversation":
      stores.conversation = conversationReducer(stores.conversation, event);
      break;
    case "tool":
      stores.tool = toolReducer(stores.tool, event);
      break;
  }
}
