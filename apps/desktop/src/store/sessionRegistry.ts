import { createSignal, createStore, reconcile, type Accessor } from "solid-js";
import type { DashboardData, RawActivityEntry, RawSessionState, RawTurn } from "./rawSession";
import { createRawSessionState } from "./sessionEventReducer";
import {
  createSessionEventRuntime,
  loadReloadSnapshot,
  removeReloadSnapshot,
  type ReloadStorage,
  type SessionEventRuntime,
} from "./sessionEventRuntime";
import { createSessionUiState, type SessionUiState } from "./sessionUiState";

export type DashboardStoreData = DashboardData & { activity: RawActivityEntry[] };

export interface SessionEntry {
  listenerSeed: string;
  state: Accessor<RawSessionState>;
  runtime: SessionEventRuntime;
  ui: SessionUiState;
  /** Reactive store mirror of state. Components read fine-grained fields
   *  directly from this store instead of going through the signal. */
  sessionStore: RawSessionState;
  /** Fine-grained reactive store for dashboard data. Leaf components read this
   *  directly instead of going through the full session state signal. */
  dashboardStore: DashboardStoreData;
  /** Optimistic pending send: set immediately when user sends a message,
   *  cleared automatically when turn_start arrives. */
  pendingSend: Accessor<RawTurn | null>;
  setPendingSend: (turn: RawTurn | null) => void;
  hasListener(): boolean;
  attachListener(unlisten: () => void): void;
  detachListener(): void;
}

export function createSessionRegistry(options: { storage: ReloadStorage }) {
  const bySeed = new Map<string, SessionEntry>();

  function get(seed: string): SessionEntry | undefined {
    return bySeed.get(seed) ?? [...bySeed.values()].find(entry => entry.state().seed === seed);
  }

  function ensure(seed: string): SessionEntry {
    const existing = get(seed);
    if (existing) return existing;
    const initial = loadReloadSnapshot(options.storage, seed) ?? createRawSessionState(seed);
    const [state, setState] = createSignal(initial);
    const [sessionStore, setSessionStore] = createStore<RawSessionState>(initial);
    const [dashboardStore, setDashboardStore] = createStore<DashboardStoreData>(initial.dashboard);
    const [pendingSend, setPendingSend] = createSignal<RawTurn | null>(null);
    let prevTurnCount = initial.turns.length;
    let prevTurnsRef = initial.turns;
    let prevDashboardRef = initial.dashboard;
    let unlisten: (() => void) | undefined;
    const entry: SessionEntry = {
      listenerSeed: seed,
      state,
      runtime: createSessionEventRuntime({
        initialState: initial,
        commit: (newState) => {
          setState(newState);
          const isStreaming = newState.turns.some(
            t => t.status === "running" || t.status === "waiting",
          );

          if (isStreaming) {
            // Streaming path: only update turns + dashboard subtrees.
            // The rest (environment, session, skills, telemetry, notices,
            // compact) are unchanged during text/tool streaming. Reconcile
            // on terminal events covers them.
            if (newState.turns !== prevTurnsRef) {
              setSessionStore(s => {
                // Only update the last turn (streaming target) — avoids
                // walking all historical turns every frame.
                const lastIdx = s.turns.length - 1;
                if (lastIdx >= 0 && lastIdx < newState.turns.length) {
                  s.turns[lastIdx] = newState.turns[lastIdx];
                }
                // Append new turns that arrived this frame
                for (let i = s.turns.length; i < newState.turns.length; i++) {
                  s.turns.push(newState.turns[i]);
                }
              });
              prevTurnsRef = newState.turns;
            }
            if (newState.dashboard !== prevDashboardRef) {
              setDashboardStore(reconcile(newState.dashboard));
              prevDashboardRef = newState.dashboard;
            }
          } else {
            // Terminal path: full reconcile all subtrees
            setSessionStore(reconcile(newState));
            setDashboardStore(reconcile(newState.dashboard));
            prevTurnsRef = newState.turns;
            prevDashboardRef = newState.dashboard;
          }

          // Auto-clear optimistic pending send when turn_start adds a new turn
          if (pendingSend() && newState.turns.length > prevTurnCount) {
            setPendingSend(null);
          }
          prevTurnCount = newState.turns.length;
        },
        storage: options.storage,
      }),
      ui: createSessionUiState(),
      sessionStore,
      dashboardStore,
      pendingSend,
      setPendingSend,
      hasListener: () => unlisten !== undefined,
      attachListener(next) {
        unlisten?.();
        unlisten = next;
      },
      detachListener() {
        const current = unlisten;
        unlisten = undefined;
        current?.();
      },
    };
    bySeed.set(seed, entry);
    return entry;
  }

  function findByListenerSeed(seed: string): SessionEntry | undefined {
    return [...bySeed.values()].find(entry => entry.listenerSeed === seed);
  }

  function remap(listenerSeed: string, nextSeed: string): SessionEntry {
    const entry = findByListenerSeed(listenerSeed) ?? ensure(listenerSeed);
    const stateSeedBefore = entry.state().seed;
    const mappedSeeds = [...bySeed.entries()]
      .filter(([, candidate]) => candidate === entry)
      .map(([seed]) => seed);
    entry.runtime.update(state => ({ ...state, seed: nextSeed }));
    for (const seed of mappedSeeds) {
      bySeed.delete(seed);
      if (seed !== nextSeed) removeReloadSnapshot(options.storage, seed);
    }
    bySeed.set(nextSeed, entry);
    if (stateSeedBefore !== nextSeed) removeReloadSnapshot(options.storage, stateSeedBefore);
    if (listenerSeed !== nextSeed) removeReloadSnapshot(options.storage, listenerSeed);
    return entry;
  }

  function remove(seed: string): void {
    const entry = get(seed);
    if (!entry) return;
    entry.detachListener();
    entry.runtime.dispose();
    bySeed.delete(seed);
    bySeed.delete(entry.listenerSeed);
    removeReloadSnapshot(options.storage, seed);
    removeReloadSnapshot(options.storage, entry.listenerSeed);
  }

  function disposeView(): void {
    for (const entry of new Set(bySeed.values())) {
      entry.runtime.dispose();
      entry.detachListener();
    }
    bySeed.clear();
  }

  return {
    ensure,
    get,
    findByListenerSeed,
    remap,
    remove,
    entries: () => [...new Set(bySeed.values())],
    disposeView,
  };
}
