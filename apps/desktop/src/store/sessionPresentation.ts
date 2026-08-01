// SessionPresentationSelector：
// 从 RingingStores（Conversation/Tool/Control 领域状态）构建展示模型。
//
// 原则：
// - turns 是核心渲染数据，全量构建（ToolState.cards → rounds.toolCalls/results）；
// - pendingInteractions 从 Control/Tool 两个领域状态构造；
// - 其余 RawSessionState 字段以 createRawSessionState 初始值为基底合并；
// - 本模块不持有状态：输入 stores + seed，输出 RawSessionState（纯函数）。

import type {
  PendingInteraction,
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

function cardsForRound(cards: ToolCardView[], turnId: string, roundNum: number): ToolCardView[] {
  return cards.filter((c) => c.turnId === turnId && c.roundNum === roundNum);
}

function toolCallsFor(cards: ToolCardView[]) {
  return cards.map((c) => ({
    id: c.toolCallId,
    name: c.name,
    args_display: c.argsSoFar,
    args_json: c.argsSoFar || "{}",
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

export function selectRingingPresentation(
  seed: string,
  stores: RingingStores,
): RawSessionState {
  const base = createRawSessionState(seed);
  const conv = stores.conversation;
  const tool = stores.tool;
  const typedUsage = conv.lastUsage?.usage as UsageInfo | undefined;

  const turns: RawTurn[] = conv.turns.map((tv) => {
    const rounds: RawRound[] = tv.rounds.map((rv) => {
      const cards = cardsForRound(tool.cards, tv.turnId, rv.roundNum);
      const progress = Object.fromEntries(
        cards
          .filter((card) => card.progressTail.length > 0)
          .map((card) => [
            card.toolCallId,
            {
              chunks: [{
                stream: card.progressStream,
                seq: card.progressSeqEnd,
                chunk: card.progressTail,
              }],
            },
          ]),
      );
      const hasActiveTool = cards.some((card) => card.status === "prepared" || card.status === "running");
      return {
        roundNum: rv.roundNum,
        isFinal: rv.isFinal,
        thinking: rv.thinking ?? "",
        answer: rv.answer ?? "",
        blocks: [],
        toolCalls: toolCallsFor(cards),
        toolResults: toolResultsFor(cards),
        progress,
        phase: rv.isFinal ? "complete" : hasActiveTool ? "tool_calling" : "answering",
      };
    });
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
      hasMore: false,
      totalTurns: turns.length,
      model: conv.lastUsage?.model ?? base.session.model,
      contextLimit: conv.lastUsage?.contextLimit ?? base.session.contextLimit,
      usage: typedUsage ?? base.session.usage,
      usageTotals: base.session.usageTotals,
    },
    telemetry: [],
    compact: {
      ...base.compact,
      active: conv.compactStatus === "completed" ? false : conv.compactStatus !== null,
      turnsCompacted: null,
    },
  };

  const pendingPermissions: PendingInteraction[] = tool.cards
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
  const activeAskPlan = stores.control.activeAskPlan;
  const pendingAskPlan: PendingInteraction[] = activeAskPlan
    ? activeAskPlan.kind === "ask"
      ? [{
          id: activeAskPlan.id,
          turnId: activeAskPlan.turnId,
          kind: "ask",
          roundNum: conv.turns.find((turn) => turn.turnId === activeAskPlan.turnId)?.lastRoundNum ?? 0,
          mode: activeAskPlan.mode ?? "single",
          questions: activeAskPlan.questions ?? [],
        }]
      : [{
          id: activeAskPlan.id,
          turnId: activeAskPlan.turnId,
          kind: "plan",
          content: activeAskPlan.planContent ?? "",
          reviewType: activeAskPlan.reviewType,
          todoItems: activeAskPlan.todoItems,
        }]
    : [];
  merged.pendingInteractions = [...pendingPermissions, ...pendingAskPlan];

  return merged;
}
