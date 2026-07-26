// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createI18n, I18nCtx } from "../i18n";
import { request } from "../runtime/backendClient";
import TodoStatusStrip from "./GoalStatusStrip";

vi.mock("../runtime/backendClient", () => ({ request: vi.fn() }));

const requestMock = vi.mocked(request);
const cleanups: Array<() => void> = [];
const flush = () => new Promise(resolve => setTimeout(resolve, 0));

afterEach(() => {
  cleanups.splice(0).forEach(dispose => dispose());
  document.body.innerHTML = "";
  vi.resetAllMocks();
});

function mount() {
  const [refreshKey, setRefreshKey] = createSignal("1:turn:running");
  const host = document.createElement("div");
  document.body.append(host);
  cleanups.push(render(() => (
    <I18nCtx value={createI18n("zh")}>
      <TodoStatusStrip seed="todo-seed" refreshKey={refreshKey()} />
    </I18nCtx>
  ), host));
  return setRefreshKey;
}

describe("TodoStatusStrip", () => {
  it("retracts after the finishing turn refreshes a completed goal", async () => {
    requestMock
      .mockResolvedValueOnce({
        mode: "goal",
        current_id: "step-1",
        current_title: "实现",
        completed: 0,
        total: 1,
        items: [],
        auto_turns: 0,
      })
      .mockResolvedValueOnce({
        mode: "completed",
        completed: 1,
        total: 1,
        items: [],
        auto_turns: 0,
      });
    const setRefreshKey = mount();

    await flush();
    expect(document.querySelector(".todo-status-strip")?.textContent).toContain("Goal 模式");

    setRefreshKey("1:turn:completed");
    await flush();
    expect(document.querySelector(".todo-status-strip")).toBeNull();
    expect(requestMock).toHaveBeenCalledTimes(2);
  });

  it("does not render a terminal goal restored from disk", async () => {
    requestMock.mockResolvedValueOnce({
      mode: "completed",
      completed: 0,
      total: 2,
      items: [],
      auto_turns: 0,
    });
    mount();

    await flush();
    expect(document.querySelector(".todo-status-strip")).toBeNull();
  });

  it("ignores an older active response that arrives after completion", async () => {
    let resolveActive!: (value: unknown) => void;
    let resolveCompleted!: (value: unknown) => void;
    requestMock
      .mockImplementationOnce(() => new Promise(resolve => { resolveActive = resolve; }))
      .mockImplementationOnce(() => new Promise(resolve => { resolveCompleted = resolve; }));
    const setRefreshKey = mount();
    await flush();
    setRefreshKey("1:turn:completed");
    await flush();

    resolveCompleted({ mode: "completed", completed: 1, total: 1, items: [], auto_turns: 0 });
    await flush();
    resolveActive({ mode: "goal", completed: 0, total: 1, items: [], auto_turns: 0 });
    await flush();

    expect(document.querySelector(".todo-status-strip")).toBeNull();
  });
});
