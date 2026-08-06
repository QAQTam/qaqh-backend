import { flush } from "solid-js";
import { expect, it, vi } from "vitest";
import { createSessionRegistry } from "./sessionRegistry";

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    values,
    storage: {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => { values.set(key, value); },
      removeItem: (key: string) => { values.delete(key); },
    },
  };
}

it("does not hydrate legacy transcript state, remaps without replacing the entry, and removes frontend resources", () => {
  const { values, storage } = memoryStorage();
  values.set("deepx:reload:v4:old", JSON.stringify({ version: 4, state: { seed: "old", turns: [{}] } }));

  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("old");
  const unlisten = vi.fn();
  entry.attachListener(unlisten);

  expect(registry.ensure("old")).toBe(entry);
  expect(entry.state().turns).toEqual([]);
  expect(registry.remap("old", "new")).toBe(entry);
  flush();
  expect(entry.state().seed).toBe("new");
  expect(entry.state().turns).toEqual([]);

  registry.remove("new");
  expect(unlisten).toHaveBeenCalledOnce();
  expect(registry.get("new")).toBeUndefined();
  expect(values.has("deepx:reload:v4:old")).toBe(true);
});

it("disposes frontend listeners", () => {
  const { storage } = memoryStorage();
  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("seed-a");
  const unlisten = vi.fn();
  entry.attachListener(unlisten);
  registry.disposeView();
  expect(unlisten).toHaveBeenCalledOnce();
  expect(registry.entries()).toEqual([]);
});

it("remaps renderer-local metadata without a wire-event reducer", () => {
  const { storage } = memoryStorage();
  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("old");
  entry.updateLocalState(state => ({ ...state, session: { ...state.session, title: "draft" } }));
  registry.remap("old", "new");
  flush();
  expect(registry.get("new")).toBe(entry);
  expect(entry.state().session.title).toBe("draft");
});

it("updates only renderer-local metadata", () => {
  const { storage } = memoryStorage();
  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("old");

  entry.updateLocalState(state => ({ ...state, seed: "new" }));
  flush();
  expect(entry.state().seed).toBe("new");
});
