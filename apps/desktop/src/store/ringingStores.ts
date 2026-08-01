// Ringing 前端三 store（PLAN 前端状态结构）。
//
// Desktop 建立：
//   ControlStore / ConversationStore / ToolStore / SessionPresentationSelector
//
// 每 store 独立维护 per-session/channel 领域状态，由 Ringing 事件驱动；
// selector 合成展示视图，**禁止合成 Agent2Ui**。
// reliable 事件到达时先 drain/覆盖同 identity replaceable 状态，再原子应用 terminal。

import type {
  RingingEventEnvelope,
  RingingEvent,
  ControlEvent,
  ConversationEvent,
  ToolEvent,
  SessionState,
  ActivityState,
} from "../lib/types/ringing";
import type { PermissionRisk } from "../lib/types";

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
  /** 切流待定（prepare 已发，commit 未到）。 */
  pendingCutover: boolean;
}

export function initialChannelConnectionState(): ChannelConnectionState {
  return {
    connected: false,
    serverEpoch: "",
    cursor: 0,
    snapshotApplied: false,
    pendingCutover: false,
  };
}

// ────────────────────────────────────────────────────────────────────────────
// ControlStore
// ────────────────────────────────────────────────────────────────────────────

export interface ActiveAskPlan {
  id: string;
  kind: "ask" | "plan";
  turnId: string;
}

export interface ControlState {
  seed: string;
  sessionState: SessionState | null;
  activity: ActivityState | null;
  agentLifecycle: "booting" | "ready" | "stopping" | "stopped" | null;
  activeAskPlan: ActiveAskPlan | null;
  lastNoticeId: string | null;
  lastFailureId: string | null;
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
        activeAskPlan: { id: event.interaction_id, kind: "ask", turnId: event.turn_id },
      };
    case "interaction_resolved":
      return state.activeAskPlan?.id === event.interaction_id
        ? { ...state, activeAskPlan: null }
        : state;
    case "plan_review_requested":
      return {
        ...state,
        activeAskPlan: { id: event.interaction_id, kind: "plan", turnId: event.turn_id },
      };
    case "plan_review_resolved":
      return state.activeAskPlan?.id === event.interaction_id
        ? { ...state, activeAskPlan: null }
        : state;
    case "operation_failed":
      return { ...state, lastFailureId: event.error.error_id };
    case "system_notice":
      return { ...state, lastNoticeId: event.notice_id };
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
}

export interface ConversationState {
  seed: string;
  activeTurn: TurnView | null;
  turns: TurnView[];
  compactStatus: "completed" | "skipped" | "failed" | "cancelled" | null;
  cancelled: boolean;
  /** 已作废的 revision（terminal 到达后旧 progress 不再渲染）。 */
  staleRevision: number;
}

export function initialConversationState(seed: string): ConversationState {
  return { seed, activeTurn: null, turns: [], compactStatus: null, cancelled: false, staleRevision: 0 };
}

function upsertTurn(state: ConversationState, turnId: string, mutate: (t: TurnView) => TurnView): ConversationState {
  const turns = state.turns.map((t) => (t.turnId === turnId ? mutate(t) : t));
  return { ...state, turns, activeTurn: turns.find((t) => t.turnId === turnId) ?? null };
}

export function conversationReducer(state: ConversationState, event: ConversationEvent): ConversationState {
  switch (event.type) {
    case "turn_started": {
      const turn: TurnView = {
        turnId: event.turn_id,
        userText: event.user_text,
        rounds: [],
        status: "running",
        lastRoundNum: 0,
      };
      return { ...state, turns: [...state.turns, turn], activeTurn: turn, cancelled: false };
    }
    case "round_delta": {
      if (!state.activeTurn || state.activeTurn.turnId !== event.turn_id) return state;
      return upsertTurn(state, event.turn_id, (t) => {
        const rounds = [...t.rounds];
        const idx = rounds.findIndex((r) => r.roundNum === event.round_num);
        if (idx < 0) {
          rounds.push({
            roundNum: event.round_num,
            thinking: event.kind === "thinking" ? event.delta : "",
            answer: event.kind === "answering" ? event.delta : "",
            isFinal: false,
          });
        } else {
          const r = rounds[idx];
          rounds[idx] = {
            ...r,
            thinking: event.kind === "thinking" ? r.thinking + event.delta : r.thinking,
            answer: event.kind === "answering" ? r.answer + event.delta : r.answer,
          };
        }
        return { ...t, rounds, lastRoundNum: Math.max(t.lastRoundNum, event.round_num) };
      });
    }
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
    case "turn_completed":
      return upsertTurn(state, event.turn_id, (t) => ({ ...t, status: "completed" }));
    case "turn_failed":
      return upsertTurn(state, event.turn_id, (t) => ({ ...t, status: "failed" }));
    case "conversation_cancelled":
      return {
        ...state,
        cancelled: true,
        activeTurn: state.activeTurn ? { ...state.activeTurn, status: "cancelled" } : null,
        staleRevision: state.staleRevision + 1,
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
  name: string;
  status: "prepared" | "running" | "finished" | "failed";
  progressTail: string;
  droppedBytes: bigint;
  truncated: boolean;
  resultSummary: string | null;
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
}

export function initialToolState(seed: string): ToolState {
  return { seed, cards: [] };
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
            name: event.name,
            status: "prepared",
            progressTail: "",
            droppedBytes: 0n,
            truncated: false,
            resultSummary: null,
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
            name: event.name,
            status: "running",
            progressTail: "",
            droppedBytes: 0n,
            truncated: false,
            resultSummary: null,
            pendingPermission: false,
            permission: null,
          },
        ],
      };
    }
    case "tool_progress":
      return patchCard(state, event.tool_call_id, {
        progressTail: event.chunk,
        droppedBytes: event.dropped_bytes,
        truncated: event.truncated,
      });
    case "tool_finished":
      return patchCard(state, event.tool_call_id, {
        status: "finished",
        resultSummary: event.result.summary,
        pendingPermission: false,
      });
    case "tool_failed":
      return patchCard(state, event.tool_call_id, {
        status: "failed",
        resultSummary: event.error.message,
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
            name: event.tool_name,
            status: "prepared",
            progressTail: "",
            droppedBytes: 0n,
            truncated: false,
            resultSummary: null,
            pendingPermission: true,
            permission,
          },
        ],
      };
    }
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
