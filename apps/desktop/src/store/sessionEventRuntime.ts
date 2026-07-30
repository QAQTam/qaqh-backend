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
  scheduleFrame?: (callback: FrameRequestCallback) => number;
  cancelFrame?: (handle: number) => void;
}): SessionEventRuntime {
  let state = options.initialState;
  let disposed = false;
  let persistenceEnabled = true;
  const now = options.now ?? Date.now;
  const hasFrameScheduler = options.scheduleFrame !== undefined || typeof requestAnimationFrame === "function";
  const scheduleFrame: (callback: FrameRequestCallback) => number = options.scheduleFrame ?? (typeof requestAnimationFrame === "function"
    ? requestAnimationFrame
    : ((_callback: FrameRequestCallback) => 0));
  const cancelFrame = options.cancelFrame ?? (typeof cancelAnimationFrame === "function"
    ? cancelAnimationFrame
    : (() => {}));
  let frameHandle: number | undefined;
  let pendingDeltas: Agent2Ui[] = [];

  const commit = () => options.commit(state);
  const applyPendingDeltas = () => {
    if (pendingDeltas.length === 0) return;
    const deltas = pendingDeltas;
    pendingDeltas = [];
    for (const delta of deltas) state = reduceAgentEvent(state, delta, now());
  };
  const flushFrame = () => {
    if (frameHandle !== undefined) {
      cancelFrame(frameHandle);
      frameHandle = undefined;
    }
    if (pendingDeltas.length === 0) return;
    applyPendingDeltas();
    commit();
  };
  const scheduleStreamCommit = () => {
    if (!hasFrameScheduler) {
      applyPendingDeltas();
      commit();
      return;
    }
    if (frameHandle !== undefined) return;
    frameHandle = scheduleFrame(() => {
      frameHandle = undefined;
      if (!disposed) {
        applyPendingDeltas();
        commit();
      }
    });
  };

  // ── Streaming delta types that should be batched per frame ──
  const STREAM_EVENT_TYPES = new Set<Agent2Ui["type"]>([
    "round_delta",
    "exec_progress",
    "tool_exec_delta",
  ]);

  return {
    push(event) {
      if (disposed) return;
      // Batch streaming events to frame boundary — each event individually
      // is cheap to reduce, but commit+reconcile per event would traverse the
      // entire state tree. One commit per frame is sufficient.
      if (STREAM_EVENT_TYPES.has(event.type)) {
        if (event.type === "round_delta") {
          const previous = pendingDeltas[pendingDeltas.length - 1];
          if (
            previous?.type === "round_delta" &&
            previous.turn_id === event.turn_id &&
            previous.round_num === event.round_num &&
            previous.kind === event.kind
          ) {
            pendingDeltas[pendingDeltas.length - 1] = { ...event, delta: previous.delta + event.delta };
          } else {
            pendingDeltas.push(event);
          }
        } else {
          pendingDeltas.push(event);
        }
        scheduleStreamCommit();
        return;
      }
      flushFrame();
      state = reduceAgentEvent(state, event, now());
      commit();
      if (IMMEDIATE_EVENT_TYPES.has(event.type) && persistenceEnabled) {
        persistenceEnabled = saveReloadSnapshot(options.storage, state);
      }
    },
    update(update) {
      if (disposed) return;
      flushFrame();
      state = update(state);
      commit();
      if (persistenceEnabled) {
        persistenceEnabled = saveReloadSnapshot(options.storage, state);
      }
    },
    flush() {
      flushFrame();
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      if (frameHandle !== undefined) cancelFrame(frameHandle);
    },
    current() {
      applyPendingDeltas();
      return state;
    },
  };
}
