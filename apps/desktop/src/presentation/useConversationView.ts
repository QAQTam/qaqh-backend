import type { Accessor } from "solid-js";
import type { Agent2Ui, TurnData } from "../lib/types";
import type { RawSessionState } from "../store/rawSession";
import {
  createRawSessionState,
  reduceAgentEvent,
} from "../store/sessionEventReducer";
import { projectTurn, type TurnViewModel } from "./turnProjection";

export type SessionProjector = (state: RawSessionState) => TurnViewModel[];

/** Keeps unchanged turns referentially stable during high-frequency streaming updates. */
export function createSessionProjector(): SessionProjector {
  const cache = new WeakMap<RawSessionState["turns"][number], TurnViewModel>();
  return state => state.turns.map(turn => {
    const existing = cache.get(turn);
    if (existing) return existing;
    const projected = projectTurn(turn);
    cache.set(turn, projected);
    return projected;
  });
}

export function projectSession(state: RawSessionState): TurnViewModel[] {
  return state.turns.map(projectTurn);
}

export function useConversationView(
  rawSession: Accessor<RawSessionState>,
): Accessor<TurnViewModel[]> {
  const project = createSessionProjector();
  return () => project(rawSession());
}

function omitTransientTiming(view: TurnViewModel): TurnViewModel {
  return {
    ...view,
    elapsedMs: undefined,
  };
}

export function viewsFromRestore(
  seed: string,
  turns: TurnData[],
): TurnViewModel[] {
  const restored = reduceAgentEvent(
    createRawSessionState(seed),
    {
      type: "session_restored",
      seed,
      turns,
      tokens_used: 0,
      cache_hit_pct: 0,
      total_turns: turns.length,
      has_more: false,
    },
    0,
  );

  return projectSession(restored).map(omitTransientTiming);
}

export function viewsFromEvents(
  seed: string,
  events: Agent2Ui[],
  now = Date.now(),
): TurnViewModel[] {
  const state = events.reduce(
    (current, event) => reduceAgentEvent(current, event, now),
    createRawSessionState(seed),
  );

  return projectSession(state).map(omitTransientTiming);
}
