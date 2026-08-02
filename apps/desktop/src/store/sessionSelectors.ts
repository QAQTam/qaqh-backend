import type { PendingInteraction, RawSessionState, RawTurn } from "./rawSession";

export function activeTurn(state: RawSessionState): RawTurn | undefined {
  return [...state.turns].reverse().find(turn =>
    turn.status === "running" || turn.status === "waiting",
  );
}

export function isSessionStreaming(state: RawSessionState): boolean {
  return activeTurn(state) !== undefined;
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
