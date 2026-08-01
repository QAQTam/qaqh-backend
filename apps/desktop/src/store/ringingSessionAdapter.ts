// SessionPresentationSelector 的适配层：
// 把 RingingStores（Conversation/Tool/Control 领域状态）投影为
// RawSessionState 形状，使 ChatView 及其子组件在切流后零改动渲染。
//
// 原则：
// - turns 是核心渲染数据，全量投影（ToolState.cards → rounds.toolCalls/results）；
// - pendingInteractions 从 ToolState.permission 详情构造（切流后权限 UI 正常）；
// - 其余 RawSessionState 字段以 createRawSessionState 初始值为基底合并；
// - 本模块不持有状态：输入 stores + seed，输出 RawSessionState（纯函数）。

import type {
  RawRound,
  RawSessionState,
  RawTurn,
  TurnStatus,
} from "./rawSession";
import { createRawSessionState } from "./sessionEventReducer";
import type { RingingStores, ToolCardView } from "./ringingStores";
import type { UsageInfo } from "../lib/types";

function mapTurnStatus(status: string): TurnStatus {
  switch (status) {
    case "completed":
      return "completed";
    case "failed":
      return "failed";
    case "cancelled":
      return "cancelled";
    default:
      return "running";
  }
}

/** 该回合的工具卡片（ToolCardView 无 roundNum，按 turnId 聚合）。 */
function cardsForTurn(cards: ToolCardView[], turnId: string): ToolCardView[] {
  return cards.filter((c) => c.turnId === turnId);
}

function toolCallsFor(cards: ToolCardView[]) {
  return cards.map((c) => ({
    id: c.toolCallId,
    name: c.name,
    args_display: "",
    args_json: "{}",
  }));
}

function toolResultsFor(cards: ToolCardView[]) {
  const out: Record<string, { tool_call_id: string; output: string; success: boolean }> = {};
  for (const c of cards) {
    if (c.status === "finished" || c.status === "failed") {
      out[c.toolCallId] = {
        tool_call_id: c.toolCallId,
        output: c.resultSummary ?? "",
        success: c.status === "finished",
      };
    }
  }
  return out;
}

export function projectRingingToRawSession(
  seed: string,
  stores: RingingStores,
  usage?: UsageInfo,
): RawSessionState {
  const base = createRawSessionState(seed);
  const conv = stores.conversation;
  const tool = stores.tool;

  const turns: RawTurn[] = conv.turns.map((tv) => {
    const cards = cardsForTurn(tool.cards, tv.turnId);
    const rounds: RawRound[] = tv.rounds.map((rv) => ({
      roundNum: rv.roundNum,
      isFinal: rv.isFinal,
      thinking: rv.thinking ?? "",
      answer: rv.answer ?? "",
      blocks: [],
      toolCalls: toolCallsFor(cards),
      toolResults: toolResultsFor(cards),
      progress: {},
      phase: rv.isFinal ? "complete" : "answering",
    }));
    return {
      turnId: tv.turnId,
      userText: tv.userText,
      status: mapTurnStatus(tv.status),
      rounds,
      interactions: [],
    };
  });

  const merged: RawSessionState = {
    ...base,
    seed,
    turns,
    session: {
      ...base.session,
      ready: stores.control.agentLifecycle === "ready",
      usage: usage ?? base.session.usage,
    },
    compact: {
      ...base.compact,
      active: conv.compactStatus === "completed" ? false : conv.compactStatus !== null,
      turnsCompacted: null,
    },
  };

  // Tool 频道卡片 → pendingInteractions（permission 类，切流后权限 UI 数据源）
  const pendingPermissions = tool.cards
    .filter((c) => c.pendingPermission && c.permission)
    .map((c) => ({
      id: c.toolCallId,
      turnId: c.turnId,
      kind: "permission" as const,
      toolName: c.name,
      reason: c.permission!.reason,
      paths: c.permission!.paths,
      category: c.permission!.category,
      level: c.permission!.level,
      risk: c.permission!.risk,
      consequence: c.permission!.consequence,
    }));
  merged.pendingInteractions = [...pendingPermissions];

  return merged;
}
