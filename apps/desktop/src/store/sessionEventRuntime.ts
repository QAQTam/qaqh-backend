import type { Agent2Ui } from "../lib/types";
import type { RawSessionState } from "./rawSession";
import { reduceAgentEvent } from "./sessionEventReducer";

export type ReloadStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

const SNAPSHOT_VERSION = 4;
const SNAPSHOT_PREFIX = "deepx:reload:v4:";
const LEGACY_SNAPSHOT_PREFIXES = ["deepx:reload:v1:", "deepx:reload:v2:", "deepx:reload:v3:"];
const MAX_RELOAD_TURNS = 20;
const MAX_PROGRESS_CHUNKS = 200;
const MAX_RELOAD_CHARS = 512 * 1024;

const IMMEDIATE_EVENT_TYPES = new Set<Agent2Ui["type"]>([
  "turn_start",
  "turn_end",
  "round_complete",
  "tool_results",
  "session_restored",
  "more_turns",
  "session_created",
  "error",
  "permission_request",
  "ask_user",
  "ask_resolved",
  "ask_rejected",
  "plan_submitted",
  "plan_resolved",
  "compact_start",
  "compact_end",
  "cancelled",
  "done",
  "ready",
]);

function reloadKey(seed: string): string {
  return `${SNAPSHOT_PREFIX}${seed}`;
}

function compactReloadState(state: RawSessionState): RawSessionState {
  return {
    ...state,
    turns: state.turns.slice(-MAX_RELOAD_TURNS).map(turn => ({
      ...turn,
      rounds: turn.rounds.map(round => ({
        ...round,
        progress: Object.fromEntries(
          Object.entries(round.progress).map(([id, progress]) => [
            id,
            { chunks: progress.chunks.slice(-MAX_PROGRESS_CHUNKS) },
          ]),
        ),
      })),
    })),
  };
}

function saveReloadSnapshot(storage: ReloadStorage, state: RawSessionState): boolean {
  try {
    const serialized = JSON.stringify({
      version: SNAPSHOT_VERSION,
      state: compactReloadState(state),
    });
    if (serialized.length > MAX_RELOAD_CHARS) {
      storage.removeItem(reloadKey(state.seed));
      return false;
    }
    storage.setItem(reloadKey(state.seed), serialized);
    return true;
  } catch {
    // The daemon owns the canonical snapshot. A local reload cache is only an
    // optimization, so a full WebView quota must never become a per-delta error
    // loop or slow down streaming. Drop this cache and disable it for the view.
    try {
      storage.removeItem(reloadKey(state.seed));
    } catch {
      // Storage can also reject removal in private/locked-down WebViews.
    }
    return false;
  }
}

export function loadReloadSnapshot(
  storage: ReloadStorage,
  seed: string,
): RawSessionState | undefined {
  try {
    for (const prefix of LEGACY_SNAPSHOT_PREFIXES) storage.removeItem(`${prefix}${seed}`);
    const raw = storage.getItem(reloadKey(seed));
    if (!raw) return undefined;
    if (raw.length > MAX_RELOAD_CHARS) {
      storage.removeItem(reloadKey(seed));
      return undefined;
    }
    const parsed = JSON.parse(raw) as { version?: number; state?: RawSessionState };
    if (
      parsed.version !== SNAPSHOT_VERSION ||
      parsed.state?.seed !== seed ||
      !Array.isArray(parsed.state.turns)
    ) {
      storage.removeItem(reloadKey(seed));
      return undefined;
    }
    return parsed.state;
  } catch {
    storage.removeItem(reloadKey(seed));
    return undefined;
  }
}

export function removeReloadSnapshot(storage: ReloadStorage, seed: string): void {
  storage.removeItem(reloadKey(seed));
}

export interface SessionEventRuntime {
  push(event: Agent2Ui): void;
  update(update: (state: RawSessionState) => RawSessionState): void;
  flush(): void;
  dispose(): void;
  current(): RawSessionState;
}

export function createSessionEventRuntime(options: {
  initialState: RawSessionState;
  commit: (state: RawSessionState) => void;
  storage: ReloadStorage;
  now?: () => number;
}): SessionEventRuntime {
  let state = options.initialState;
  let disposed = false;
  let persistenceEnabled = true;
  const now = options.now ?? Date.now;

  return {
    push(event) {
      if (disposed) return;
      state = reduceAgentEvent(state, event, now());
      options.commit(state);
      if (IMMEDIATE_EVENT_TYPES.has(event.type) && persistenceEnabled) {
        persistenceEnabled = saveReloadSnapshot(options.storage, state);
      }
    },
    update(update) {
      if (disposed) return;
      state = update(state);
      options.commit(state);
      if (persistenceEnabled) {
        persistenceEnabled = saveReloadSnapshot(options.storage, state);
      }
    },
    flush() {
      // State is committed synchronously on every push/update.
      // No-op kept for interface compatibility.
    },
    dispose() {
      if (disposed) return;
      disposed = true;
    },
    current() {
      return state;
    },
  };
}
