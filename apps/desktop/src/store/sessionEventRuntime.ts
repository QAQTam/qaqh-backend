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

// rAF 在后台/隐藏窗口不触发：若长时间没有帧调度，流式提交会冻结。
// 用定时器兜底，保证最小化/后台时流式仍然推进。
const STREAM_FALLBACK_MS = 100;

// Persistence is only worth it for low-frequency terminal events; round-level
// events arrive in bursts during tool loops and would serialize the whole
// state (MBs) synchronously on every burst. A 3s throttle absorbs repeats.
const PERSIST_INTERVAL_MS = 3000;
const PERSIST_EVENT_TYPES = new Set<Agent2Ui["type"]>([
  "turn_end",
  "session_restored",
  "session_created",
  "compact_end",
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
  let lastPersistAt = 0;
  const now = options.now ?? Date.now;
  const hasFrameScheduler = options.scheduleFrame !== undefined || typeof requestAnimationFrame === "function";
  const scheduleFrame: (callback: FrameRequestCallback) => number = options.scheduleFrame ?? (typeof requestAnimationFrame === "function"
    ? requestAnimationFrame
    : ((_callback: FrameRequestCallback) => 0));
  const cancelFrame = options.cancelFrame ?? (typeof cancelAnimationFrame === "function"
    ? cancelAnimationFrame
    : (() => {}));
  let frameHandle: number | undefined;
  let fallbackHandle: ReturnType<typeof setTimeout> | undefined;
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
    if (fallbackHandle !== undefined) {
      clearTimeout(fallbackHandle);
      fallbackHandle = undefined;
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
    if (frameHandle !== undefined || fallbackHandle !== undefined) return;
    const flush = () => {
      frameHandle = undefined;
      fallbackHandle = undefined;
      if (!disposed) {
        applyPendingDeltas();
        commit();
      }
    };
    frameHandle = scheduleFrame(flush);
    // rAF 在隐藏/后台窗口不触发：定时器兜底，避免流式冻结。
    fallbackHandle = setTimeout(flush, STREAM_FALLBACK_MS);
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
      if (PERSIST_EVENT_TYPES.has(event.type) && persistenceEnabled) {
        const nowMs = now();
        if (nowMs - lastPersistAt >= PERSIST_INTERVAL_MS) {
          lastPersistAt = nowMs;
          persistenceEnabled = saveReloadSnapshot(options.storage, state);
        }
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
      if (fallbackHandle !== undefined) clearTimeout(fallbackHandle);
    },
    current() {
      applyPendingDeltas();
      return state;
    },
  };
}
