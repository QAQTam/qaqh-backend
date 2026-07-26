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
  return {
    contextTokens: usage?.prompt_tokens ?? state.session.tokensUsed,
    totalTokens: usage?.total_tokens ?? state.session.tokensUsed,
    cacheHit: usage?.prompt_cache_hit_tokens ?? 0,
    cacheMiss: usage?.prompt_cache_miss_tokens ?? 0,
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
