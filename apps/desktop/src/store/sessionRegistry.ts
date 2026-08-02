import { createSignal, createStore, reconcile, type Accessor } from "solid-js";
import type { DashboardData, RawActivityEntry, RawSessionState, RawTurn } from "./rawSession";
import { createRawSessionState } from "./rawSession";
import { createSessionUiState, type SessionUiState } from "./sessionUiState";

export type ReloadStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export type DashboardStoreData = DashboardData & { activity: RawActivityEntry[] };

export interface SessionEntry {
  listenerSeed: string;
  state: Accessor<RawSessionState>;
  /** Mutates renderer-local UI state only. Daemon events never enter here. */
  updateLocalState(update: (state: RawSessionState) => RawSessionState): void;
  ui: SessionUiState;
  /** Fine-grained reactive store for dashboard data. Leaf components read this
   *  directly instead of going through the full session state signal. */
  dashboardStore: DashboardStoreData;
  /** Native dashboard/activity snapshots update this store directly. */
  setDashboard(data: DashboardStoreData): void;
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
    // The daemon Timeline snapshot is the only transcript recovery source.
    // Do not restore a serialized legacy projection into a new renderer.
    void options.storage;
    const initial = createRawSessionState(seed);
    const [state, setState] = createSignal(initial);
    const [dashboardStore, setDashboardStore] = createStore<DashboardStoreData>(initial.dashboard);
    const [pendingSend, setPendingSend] = createSignal<RawTurn | null>(null);
    let unlisten: (() => void) | undefined;
    const entry: SessionEntry = {
      listenerSeed: seed,
      state,
      updateLocalState(update) { setState(update); },
      ui: createSessionUiState(),
      dashboardStore,
      setDashboard(data) {
        setDashboardStore(reconcile(data));
      },
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
    // Solid 2 信号写入是微任务批处理：同栈读 state() 可能滞后，必须用
    // runtime.current() 这个同步权威源（事件已同步 reduce 进本地 state）。
    const stateSeedBefore = entry.state().seed;
    const mappedSeeds = [...bySeed.entries()]
      .filter(([, candidate]) => candidate === entry)
      .map(([seed]) => seed);
    entry.updateLocalState(state => ({ ...state, seed: nextSeed }));
    for (const seed of mappedSeeds) {
      bySeed.delete(seed);
    }
    bySeed.set(nextSeed, entry);
    void stateSeedBefore;
    void listenerSeed;
    return entry;
  }

  function remove(seed: string): void {
    const entry = get(seed);
    if (!entry) return;
    entry.detachListener();
    bySeed.delete(seed);
    bySeed.delete(entry.listenerSeed);
  }

  function disposeView(): void {
    for (const entry of new Set(bySeed.values())) {
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
