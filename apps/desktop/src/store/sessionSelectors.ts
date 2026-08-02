import type { PendingInteraction, RawSessionState, RawTurn } from "./rawSession";

export function activeTurn(state: RawSessionState): RawTurn | undefined {
  return [...state.turns].reverse().find(turn =>
    turn.status === "running" || turn.status === "waiting",
  );
}

/**
 * 卡死判定阈值：running/waiting 的 turn 超过该时长没有任何领域事件，
 * 视为会话卡死（典型场景：工具调用未返回 result 且 worker 无法收尾）。
 */
export const SESSION_STALL_TIMEOUT_MS = 4 * 60 * 1000;

function lastActivityOf(turn: RawTurn): number | undefined {
  return turn.lastActivityAt ?? turn.startedAt;
}

/** 会话是否处于卡死状态（有 running turn 但长时间无事件）。 */
export function isSessionStalled(state: RawSessionState): boolean {
  const turn = activeTurn(state);
  if (!turn) return false;
  const last = lastActivityOf(turn);
  if (last == null) return false;
  return Date.now() - last >= SESSION_STALL_TIMEOUT_MS;
}

export function isSessionStreaming(state: RawSessionState): boolean {
  const turn = activeTurn(state);
  if (!turn) return false;
  const last = lastActivityOf(turn);
  // 无时间戳（旧数据/恢复间隙）：保守按 streaming 处理，避免误发。
  if (last == null) return true;
  return Date.now() - last < SESSION_STALL_TIMEOUT_MS;
}

export function activeInteraction(state: RawSessionState): PendingInteraction | null {
  return state.pendingInteractions[0] ?? null;
}

export function sessionUsage(state: RawSessionState) {
  const usage = state.session.usage;
  const totals = state.session.usageTotals;
  const cacheReported = Boolean(usage?.cache_usage_reported);
  const cacheSampleTokens =
    (usage?.prompt_cache_hit_tokens ?? 0) + (usage?.prompt_cache_miss_tokens ?? 0);
  const sessionCacheSampleTokens =
    totals.prompt_cache_hit_tokens + totals.prompt_cache_miss_tokens;
  const requestTotalTokens = usage?.total_tokens ?? 0;
  const requestCacheSampleTokens = cacheSampleTokens;
  return {
    contextTokens: usage?.prompt_tokens ?? state.session.tokensUsed,
    totalTokens: usage?.total_tokens ?? state.session.tokensUsed,
    cacheHit: usage?.prompt_cache_hit_tokens ?? 0,
    cacheMiss: usage?.prompt_cache_miss_tokens ?? 0,
    promptTokens: usage?.prompt_tokens ?? 0,
    completionTokens: usage?.completion_tokens ?? 0,
    reasoningTokens: usage?.reasoning_tokens ?? 0,
    requestTotalTokens,
    cacheAvailable: cacheReported && requestCacheSampleTokens > 0,
    cacheHitPct: requestCacheSampleTokens > 0
      ? (usage?.prompt_cache_hit_tokens ?? 0) * 100 / cacheSampleTokens
      : sessionCacheSampleTokens > 0
        ? totals.prompt_cache_hit_tokens * 100 / sessionCacheSampleTokens
        : null,
    cacheSampleTokens,
    sessionCacheAvailable: state.session.cacheReportedRequestCount > 0,
    sessionCacheHitPct: state.session.cacheReportedRequestCount > 0
      ? sessionCacheSampleTokens > 0
        ? totals.prompt_cache_hit_tokens * 100 / sessionCacheSampleTokens
        : 0
      : null,
    sessionCacheSampleTokens,
    cacheReportedRequestCount: state.session.cacheReportedRequestCount,
    totals,
    requestCount: state.session.usageRequestCount,
    contextLimit: state.session.contextLimit,
    model: state.session.model ?? "",
  };
}

export function failedPrompt(state: RawSessionState): string | null {
  return [...state.turns].reverse().find(turn => turn.status === "failed")?.userText ?? null;
}

export function canLoadMore(state: RawSessionState): boolean {
  return state.session.hasMore && state.turns.length > 0;
}
