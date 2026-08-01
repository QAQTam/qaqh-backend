import { flush } from "solid-js";
import { expect, it, vi } from "vitest";
import { createRawSessionState } from "./sessionEventReducer";
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

it("hydrates once, remaps without replacing the entry, and removes frontend resources", () => {
  const { values, storage } = memoryStorage();
  const restored = createRawSessionState("old");
  restored.turns.push({
    turnId: "t1", userText: "restored", status: "completed", rounds: [], interactions: [],
  });
  values.set("deepx:reload:v4:old", JSON.stringify({ version: 4, state: restored }));

  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("old");
  const unlisten = vi.fn();
  entry.attachListener(unlisten);

  expect(registry.ensure("old")).toBe(entry);
  expect(entry.state().turns[0].turnId).toBe("t1");
  expect(registry.remap("old", "new")).toBe(entry);
  flush();
  expect(entry.state().seed).toBe("new");
  expect(entry.state().turns[0].turnId).toBe("t1");

  registry.remove("new");
  expect(unlisten).toHaveBeenCalledOnce();
  expect(registry.get("new")).toBeUndefined();
  expect(values.has("deepx:reload:v4:old")).toBe(false);
  expect(values.has("deepx:reload:v4:new")).toBe(false);
});

it("disposes only frontend-owned runtimes and listeners", () => {
  const { storage } = memoryStorage();
  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("seed-a");
  const unlisten = vi.fn();
  entry.attachListener(unlisten);
  registry.disposeView();
  expect(unlisten).toHaveBeenCalledOnce();
  expect(registry.entries()).toEqual([]);
});

it("keeps the new-seed snapshot when remap follows session_created reduction", () => {
  const { values, storage } = memoryStorage();
  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("old");
  entry.runtime.push({ type: "session_created", seed: "new" });

  registry.remap("old", "new");

  expect(values.has("deepx:reload:v4:old")).toBe(false);
  expect(values.has("deepx:reload:v4:new")).toBe(true);
  expect(registry.get("new")).toBe(entry);
});

it("runtime.current() is the synchronous authoritative source while the signal lags", () => {
  const { storage } = memoryStorage();
  const registry = createSessionRegistry({ storage });
  const entry = registry.ensure("old");

  entry.runtime.push({ type: "session_created", seed: "new" });

  // Solid 2（beta.28 浏览器构建）信号写入是微任务批处理：同一同步栈内
  // state() 仍可能是旧值；runtime.current() 始终立即可靠。
  // 若此断言失败，说明框架批处理行为变化——请重新评估所有
  // “push/update 后同栈读 state()” 的调用点。
  expect(entry.state().seed).toBe("old");
  expect(entry.runtime.current().seed).toBe("new");

  // 冲刷后信号收敛到最新值。
  flush();
  expect(entry.state().seed).toBe("new");
});
