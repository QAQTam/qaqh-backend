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
  return {
    contextTokens: usage?.prompt_tokens ?? totals.prompt_tokens ?? state.session.tokensUsed,
    totalTokens: usage?.total_tokens ?? totals.total_tokens ?? state.session.tokensUsed,
    cacheHit: usage?.prompt_cache_hit_tokens ?? totals.prompt_cache_hit_tokens,
    cacheMiss: usage?.prompt_cache_miss_tokens ?? totals.prompt_cache_miss_tokens,
    promptTokens: usage?.prompt_tokens ?? totals.prompt_tokens,
    completionTokens: usage?.completion_tokens ?? totals.completion_tokens,
    reasoningTokens: usage?.reasoning_tokens ?? totals.reasoning_tokens,
    requestTotalTokens: usage?.total_tokens ?? totals.total_tokens ?? 0,
    cacheAvailable: (cacheReported || state.session.cacheReportedRequestCount > 0) && (cacheSampleTokens > 0 || sessionCacheSampleTokens > 0),
    cacheHitPct: cacheSampleTokens > 0
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
