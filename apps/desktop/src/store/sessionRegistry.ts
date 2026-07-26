import { createSignal, type Accessor } from "solid-js";
import type { RawSessionState } from "./rawSession";
import { createRawSessionState } from "./sessionEventReducer";
import {
  createSessionEventRuntime,
  loadReloadSnapshot,
  removeReloadSnapshot,
  type ReloadStorage,
  type SessionEventRuntime,
} from "./sessionEventRuntime";
import { createSessionUiState, type SessionUiState } from "./sessionUiState";

export interface SessionEntry {
  listenerSeed: string;
  state: Accessor<RawSessionState>;
  runtime: SessionEventRuntime;
  ui: SessionUiState;
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
    let unlisten: (() => void) | undefined;
    const entry: SessionEntry = {
      listenerSeed: seed,
      state,
      runtime: createSessionEventRuntime({
        initialState: initial,
        commit: setState,
        storage: options.storage,
      }),
      ui: createSessionUiState(),
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
