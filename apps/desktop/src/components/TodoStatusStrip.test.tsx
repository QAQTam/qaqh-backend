// @vitest-environment jsdom

import { createStore } from "solid-js";
import { render } from "@solidjs/web";
import { afterEach, describe, expect, it } from "vitest";
import TodoStatusStrip from "./TodoStatusStrip";
import type { TaskInfo } from "../lib/types";
import type { DashboardStoreData } from "../store/sessionRegistry";

const cleanups: Array<() => void> = [];
const flush = () => new Promise(resolve => setTimeout(resolve, 0));

afterEach(() => {
  cleanups.splice(0).forEach(dispose => dispose());
  document.body.innerHTML = "";
});

function mount(tasks: TaskInfo[], currentTodoId?: string | null) {
  const host = document.createElement("div");
  document.body.append(host);

  const initial: DashboardStoreData = {
    tasks,
    recentEdits: [],
    currentTodoId: currentTodoId ?? null,
    activity: [],
  };
  const [dashboard] = createStore<DashboardStoreData>(initial);

  cleanups.push(render(() =>
    <TodoStatusStrip dashboard={dashboard} />
  , host));
}

describe("TodoStatusStrip", () => {
  it("renders summary row for single pending item (no active)", async () => {
    mount([{ id: "T1", subject: "修复窗口", description: "", status: "pending" }]);
    await flush();

    const strip = document.querySelector(".todo-strip")!;
    expect(strip.textContent).not.toContain("Todo");
    expect(strip.textContent).toContain("1 待处理");
    // Progress count rendered as "0/1" in the <small> tag
    expect(strip.textContent).toContain("0/1");
    // Progress fill bar shows 0% width (all pending)
    const fill = strip.querySelector(".todo-progress-fill") as HTMLElement;
    expect(fill.style.width).toBe("0%");
    expect(strip.getAttribute("aria-expanded")).toBe("false");
    // Expand
    (strip as HTMLElement).click(); await flush();
    expect(strip.textContent).toContain("修复窗口");
    expect(strip.textContent).toContain("待处理");
    expect(strip.querySelector("[data-status='pending']")).not.toBeNull();
  });

  it("shows single-line carousel with prev/current/next arrows", async () => {
    mount([
      { id: "T1", subject: "排队", description: "", status: "completed" },
      { id: "T2", subject: "实现功能模块开发与测试验证", description: "", status: "in_progress" },
      { id: "T3", subject: "已验证", description: "", status: "pending" },
    ], "T2");
    await flush();

    const strip = document.querySelector(".todo-strip") as HTMLElement;
    expect(strip.textContent).not.toContain("Todo");
    // Current item
    expect(strip.textContent).toContain("T2");
    expect(strip.textContent).toContain("进行中");
    // Title truncated (max ~25 CJK / ~50 ASCII visual width)
    const curText = strip.querySelector(".todo-roulette-text")?.textContent ?? "";
    expect(curText.length).toBeLessThanOrEqual(22);
    // Progress: 1 completed + 1 cancelled out of 3 → 1/3
    expect(strip.textContent).toContain("1/3");
    // Arrows both visible (the roulette has 3 rows: prev, current, next)
    const rows = strip.querySelectorAll(".todo-roulette-row");
    expect(rows.length).toBe(3);
    // Expand
    (strip as HTMLElement).click(); await flush();
    expect(strip.querySelectorAll("[data-status]").length).toBe(3);
    expect(strip.textContent).toContain("已完成");
  });

  it("shows 'all done' state with green bar", async () => {
    mount([
      { id: "T1", subject: "完成项", description: "", status: "completed" },
      { id: "T2", subject: "取消项", description: "", status: "cancelled" },
    ]);
    await flush();

    const strip = document.querySelector(".todo-strip")!;
    expect(strip.textContent).toContain("✓ 全部完成 (2/2)");
    // Progress count shows 2/2
    expect(strip.textContent).toContain("2/2");
    expect(strip.classList.contains("all-done")).toBe(true);
  });

  it("hides when tasks array is empty", async () => {
    mount([]);
    await flush();
    expect(document.querySelector(".todo-strip")).toBeNull();
  });
});
