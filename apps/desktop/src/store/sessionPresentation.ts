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
import { createRawSessionState } from "./rawSession";
import type { RingingStores, ToolCardView, TurnView, RoundView } from "./ringingStores";
import type { UsageInfo } from "../lib/types/ringing/UsageInfo";
import type { ToolResult } from "../lib/types/ringing/ToolResult";
import type { SkillRuntimeInfo } from "./rawSession";
import { toolArgsSummary } from "../presentation/toolSemantics";

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

/**
 * 按 (turnId, roundNum) 预索引 tool cards：流式期间 presentationFor 每帧
 * 全量重建 turns，若每 round 都 filter 全部 cards，长会话下退化为
 * O(turns×rounds×cards)（数千次比较/帧）。索引后为 O(cards + turns×rounds)。
 *
 * 索引同时缓存 list 数组：toolReducer 是不可变更新，未变化的 card 对象
 * 引用稳定——同一 (seed, turnId, roundNum) 的 card 引用序列未变时复用
 * 上次的 list，使 round 投影缓存（引用比较）能命中。
 */
const indexListCache = new Map<string, { refs: ToolCardView[]; list: ToolCardView[] }>();
/** 有界：字符串键随 (seed, turn, round) 增长，超限整体淘汰（冷启动代价仅一次投影）。 */
const INDEX_CACHE_MAX = 4096;

function indexCardsByTurnRound(cards: ToolCardView[], seed: string): Map<string, ToolCardView[]> {
  // 同一 round 可以有多个工具调用（并行工具 / 同回合多工具），全部 card
  // 都必须保留。先按 (seed, turnId, roundNum) 分组（保持 cards 原始顺序），
  // 再与缓存比对整个引用序列：序列未变 → 复用上次的 list 数组（round 投影
  // 缓存靠引用比较命中）；任一 card 引用变化 → 整组重建并更新缓存。
  const groups = new Map<string, ToolCardView[]>();
  for (const card of cards) {
    const key = `${seed}:${card.turnId}:${card.roundNum}`;
    const group = groups.get(key);
    if (group) group.push(card);
    else groups.set(key, [card]);
  }
  const index = new Map<string, ToolCardView[]>();
  for (const [key, list] of groups) {
    const cached = indexListCache.get(key);
    if (
      cached
      && cached.refs.length === list.length
      && cached.refs.every((ref, i) => ref === list[i])
    ) {
      // 引用序列未变：复用数组（round 缓存因此可命中）
      index.set(key, cached.list);
    } else {
      if (indexListCache.size >= INDEX_CACHE_MAX) indexListCache.clear();
      indexListCache.set(key, { refs: list.slice(), list });
      index.set(key, list);
    }
  }
  return index;
}

/** 无 tool card 的共享空数组（热路径避免每 round 分配）。 */
const EMPTY_CARDS: ToolCardView[] = [];

function toolCallsFor(cards: ToolCardView[]) {
  return cards.map((c) => ({
    id: c.toolCallId,
    name: c.name,
    // 语义化摘要（路径/命令），取代 argsSoFar 的 JSON 原文
    args_display: toolArgsSummary(c.name, c.argsSoFar) || c.name,
    args_json: c.argsSoFar || "{}",
  }));
}

function toolResultsFor(cards: ToolCardView[]) {
  const out: Record<string, ToolResult> = {};
  for (const c of cards) {
    if (c.status === "finished" && c.result) {
      out[c.toolCallId] = c.result;
    }
  }
  return out;
}

// ── 投影缓存 ─────────────────────────────────────────────────────────────
// 流式热路径：presentationFor 每帧重建全部 turns。若每次投影都新建对象，
// Solid 无法跳过任何子树，所有组件（ProcessTimeline/ProcessDetail/Markdown
// 等）每帧全量重渲染；长思考链/长 exec 输出的 detailText join、JSON.parse
// 与 <pre> 文本替换随之 O(n²) 退化，即使不渲染 Markdown 也卡死。
//
// store 的 reducer 是不可变更新：未变化的 TurnView/RoundView/card 对象
// 引用稳定，可作为 WeakMap 键做投影缓存。tool 状态以 tool.cards 数组引用
// 作为版本信号（tool 事件必然新建数组）。
const turnProjectionCache = new WeakMap<TurnView, { toolCards: ToolCardView[]; turn: RawTurn }>();
const roundProjectionCache = new WeakMap<RoundView, { cards: ToolCardView[]; round: RawRound }>();

function projectRound(rv: RoundView, cards: ToolCardView[]): RawRound {
  const cached = roundProjectionCache.get(rv);
  if (cached && cached.cards === cards) return cached.round;
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
  const round: RawRound = {
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
  roundProjectionCache.set(rv, { cards, round });
  return round;
}

function projectTurnView(
  seed: string,
  tv: TurnView,
  toolCards: ToolCardView[],
  cardsIndex: Map<string, ToolCardView[]>,
): RawTurn {
  const cached = turnProjectionCache.get(tv);
  if (cached && cached.toolCards === toolCards) return cached.turn;
  const rounds: RawRound[] = tv.rounds.map((rv) =>
    projectRound(rv, cardsIndex.get(`${seed}:${tv.turnId}:${rv.roundNum}`) ?? EMPTY_CARDS),
  );
  const turn: RawTurn = {
    turnId: tv.turnId,
    userText: tv.userText,
    status: mapTurnStatus(tv.status),
    failure: tv.failure,
    startedAt: tv.startedAt,
    lastActivityAt: tv.lastActivityAt,
    rounds,
    interactions: [],
  };
  turnProjectionCache.set(tv, { toolCards, turn });
  return turn;
}

export function selectRingingPresentation(
  seed: string,
  stores: RingingStores,
  fallback?: RawSessionState,
  options?: { includeTurns?: boolean },
): RawSessionState {
  const base = fallback ?? createRawSessionState(seed);
  const conv = stores.conversation;
  const tool = stores.tool;
  const includeTurns = options?.includeTurns !== false;
  const typedUsage = conv.lastUsage?.usage as UsageInfo | undefined;
  const hasRingingUsage = conv.lastUsage !== null
    || conv.usageRequestCount > 0
    || conv.usageTotals.total_tokens > 0;
  const usageTotals = hasRingingUsage ? conv.usageTotals : base.session.usageTotals;
  const usageRequestCount = hasRingingUsage ? conv.usageRequestCount : base.session.usageRequestCount;
  const cacheReportedRequestCount = hasRingingUsage
    ? conv.cacheReportedRequestCount
    : base.session.cacheReportedRequestCount;

  const turns: RawTurn[] = includeTurns ? (() => {
    // 流式热路径：先建索引，再经 turn/round 投影缓存复用未变化的
    // RawTurn/RawRound 对象（引用稳定 → Solid 跳过未变化子树）。
    const cardsIndex = indexCardsByTurnRound(tool.cards, seed);
    return conv.turns.map((tv) => projectTurnView(seed, tv, tool.cards, cardsIndex));
  })() : base.turns;

  const merged: RawSessionState = {
    ...base,
    seed,
    turns,
    session: {
      ...base.session,
      ready: stores.control.agentLifecycle === "ready",
      model: conv.lastUsage?.model ?? base.session.model,
      contextLimit: conv.lastUsage?.contextLimit ?? base.session.contextLimit,
      usage: typedUsage ?? base.session.usage,
      usageTotals,
      usageRequestCount,
      cacheReportedRequestCount,
      tokensUsed: usageTotals.total_tokens,
      cacheHitPct: usageTotals.cache_usage_reported === true
        ? (() => {
          const total = usageTotals.prompt_cache_hit_tokens + usageTotals.prompt_cache_miss_tokens;
          return total > 0 ? usageTotals.prompt_cache_hit_tokens * 100 / total : 0;
        })()
        : base.session.cacheHitPct,
      hasMore: conv.hasMore || base.session.hasMore,
      totalTurns: Math.max(conv.totalTurns, turns.length, base.session.totalTurns),
    },
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

  const skillsView = selectSkillsPresentation(stores);
  if (skillsView) merged.skills = skillsView;

  return merged;
}

/** skills 域的空投影（无 Ringing store 时的兜底初始值）。 */
export function emptySkillsPresentation(): RawSessionState["skills"] {
  return {
    available: [],
    active: [],
    catalogRevision: "",
    contextEpoch: 0,
    operationRevision: 0,
    tokenBudget: 0,
    tokenUsage: 0,
    runtime: [],
    diagnostics: [],
  };
}

/**
 * skills 域独立投影：SkillsView 只读此域，不必触发 turns 全量投影。
 * 依赖版本信号由调用方（App）建立；此处为纯函数。
 */
export function selectSkillsPresentation(
  stores: RingingStores,
): RawSessionState["skills"] | null {
  const skills = stores.control.skills;
  if (!skills) return null;
  const active = new Set(skills.active);
  // 事件携带的 runtime 是权威生命周期（catalog/requested/active/unavailable）。
  // 旧 daemon 事件没有该字段时退回合成视图（仅 active/catalog 两态）。
  const runtime: SkillRuntimeInfo[] = (skills.runtime ?? []).length > 0
    ? skills.runtime
    : skills.available.map(skill => ({
      name: skill.name,
      description: skill.description,
      source: skill.source,
      state: active.has(skill.name) ? "active" : "catalog",
      token_count: 0,
    }));
  return {
    available: skills.available,
    active: skills.active,
    catalogRevision: skills.catalogRevision ?? "",
    operationRevision: skills.operationRevision ?? 0,
    contextEpoch: skills.contextEpoch ?? 0,
    tokenBudget: skills.tokenBudget ?? 0,
    tokenUsage: skills.tokenUsage ?? 0,
    runtime,
    diagnostics: skills.diagnostics ?? [],
  };
}
