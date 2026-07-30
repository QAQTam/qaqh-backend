// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createI18n, I18nCtx } from "../i18n";
import { request } from "../runtime/backendClient";
import TodoStatusStrip from "./TodoStatusStrip";

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
  it("renders the backend status contract without Goal controls", async () => {
    const status = {
      mode: "manual",
      pending: 1,
      in_progress: 0,
      completed: 0,
      cancelled: 0,
      total: 1,
      items: [{
        id: "T1",
        title: "修复窗口",
        description: "验证完整链路",
        status: "pending",
        complexity: "medium",
      }],
      goal_enabled: false,
    };
    requestMock.mockResolvedValueOnce(status);
    mount();

    await flush();

    const strip = document.querySelector(".todo-status-strip");
    expect(strip?.textContent).toContain("Todo 列表");
    expect(strip?.textContent).toContain("待处理 1");
    expect(strip?.textContent).not.toContain("Activate");
    expect(strip?.querySelector(".todo-status-copy")?.getAttribute("aria-expanded")).toBe("false");
    (strip?.querySelector(".todo-status-copy") as HTMLElement).click();
    await flush();
    expect(strip?.textContent).toContain("T1");
    expect(strip?.textContent).toContain("修复窗口");
    expect(strip?.textContent).toContain("待处理");
    expect(strip?.querySelector("[data-status='pending']")).not.toBeNull();
    expect(requestMock).toHaveBeenCalledTimes(1);
  });

  it("shows the active todo and all four exact item states", async () => {
    requestMock.mockResolvedValueOnce({
      mode: "manual",
      current_id: "T2",
      current_title: "实现功能",
      pending: 1,
      in_progress: 1,
      completed: 1,
      cancelled: 1,
      total: 4,
      items: [
        { id: "T1", title: "排队", description: "", status: "pending" },
        { id: "T2", title: "实现功能", description: "正在处理", status: "in_progress" },
        { id: "T3", title: "已验证", description: "", status: "completed" },
        { id: "T4", title: "不再需要", description: "", status: "cancelled" },
      ],
    });
    mount();

    await flush();
    const strip = document.querySelector(".todo-status-strip") as HTMLElement;
    expect(strip.textContent).toContain("T2: 实现功能");
    expect(strip.textContent).toContain("进行中 1 · 待处理 1 · 已完成 1 · 已取消 1");
    (strip.querySelector(".todo-status-copy") as HTMLElement).click();
    await flush();
    expect(strip.querySelectorAll("[data-status]").length).toBe(4);
    expect(strip.textContent).toContain("进行中");
    expect(strip.textContent).toContain("已完成");
    expect(strip.textContent).toContain("已取消");
  });

  it("keeps a fully terminal todo list visible with an accurate summary", async () => {
    requestMock.mockResolvedValueOnce({
      mode: "manual",
      pending: 0,
      in_progress: 0,
      completed: 1,
      cancelled: 1,
      total: 2,
      items: [
        { id: "T1", title: "完成项", description: "", status: "completed" },
        { id: "T2", title: "取消项", description: "", status: "cancelled" },
      ],
    });
    mount();

    await flush();
    const strip = document.querySelector(".todo-status-strip");
    expect(strip?.textContent).toContain("所有 Todo 均已处理");
    expect(strip?.textContent).toContain("已完成 1 · 已取消 1");
  });

  it("ignores an older response after a newer refresh completes", async () => {
    let resolveActive!: (value: unknown) => void;
    let resolveCompleted!: (value: unknown) => void;
    requestMock
      .mockImplementationOnce(() => new Promise(resolve => { resolveActive = resolve; }))
      .mockImplementationOnce(() => new Promise(resolve => { resolveCompleted = resolve; }));
    const setRefreshKey = mount();
    await flush();
    setRefreshKey("1:turn:completed");
    await flush();

    resolveCompleted({
      mode: "manual",
      pending: 0,
      in_progress: 0,
      completed: 1,
      cancelled: 0,
      total: 1,
      items: [{ id: "T1", title: "新版", description: "", status: "completed" }],
    });
    await flush();
    resolveActive({
      mode: "manual",
      pending: 1,
      in_progress: 0,
      completed: 0,
      cancelled: 0,
      total: 1,
      items: [{ id: "T1", title: "旧版", description: "", status: "pending" }],
    });
    await flush();

    expect(document.querySelector(".todo-status-strip")?.textContent).toContain("所有 Todo 均已处理");
    expect(document.querySelector(".todo-status-strip")?.textContent).not.toContain("待处理 1");
  });
});
