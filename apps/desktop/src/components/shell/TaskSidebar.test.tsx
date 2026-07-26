// @vitest-environment jsdom
import { render } from "@solidjs/web";
import { expect, it, vi } from "vitest";
import type { SessionMeta } from "../../lib/types";
import TaskSidebar, { taskTitle } from "./TaskSidebar";

const session = (seed: string, summary = ""): SessionMeta => ({ seed, last_summary: summary, created_at: 0n, updated_at: 0n, model: "", message_count: 0, turn_count: 0, compact_skip: 0, mode: 0, skills: { version: 2, context_epoch: 0n, operation_revision: 0n, entries: [] }, turso_backed: false, running: false });

it("uses dashboard title, summary, then seed", () => {
  expect(taskTitle(session("abcdef12", "Summary"), "Named")).toBe("Named");
  expect(taskTitle(session("abcdef12", "Summary"))).toBe("Summary");
  expect(taskTitle(session("abcdef12"))).toBe("abcdef12");
});

it("renders sessions once without open tabs", () => {
  const host = document.createElement("div");
  const dispose = render(() => <TaskSidebar sessions={[session("a"), session("b")]} activeSeed="a" onNew={vi.fn()} onOpen={vi.fn()} onDelete={vi.fn()} onHome={vi.fn()} onSkills={vi.fn()} onSettings={vi.fn()} />, host);
  expect(host.querySelectorAll("[data-task-session]")).toHaveLength(2);
  expect(host.querySelector(".open-tabs")).toBeNull();
  dispose();
});

it("uses the brand as a persistent home navigation control", () => {
  const host = document.createElement("div");
  const onHome = vi.fn();
  const dispose = render(() => <TaskSidebar sessions={[]} activeSeed="" onNew={vi.fn()} onOpen={vi.fn()} onDelete={vi.fn()} onHome={onHome} onSkills={vi.fn()} onSettings={vi.fn()} />, host);
  (host.querySelector(".task-sidebar-brand") as HTMLButtonElement).click();
  expect(onHome).toHaveBeenCalledOnce();
  dispose();
});

it("shows authoritative activity independently for every session", () => {
  const host = document.createElement("div");
  const dispose = render(() => <TaskSidebar
    sessions={[session("a"), session("b")]}
    activities={{
      a: { seed: "a", state: "working", seq: 2, updated_at: 2 },
      b: { seed: "b", state: "disconnected", seq: 4, updated_at: 4 },
    }}
    activeSeed="a"
    onNew={vi.fn()}
    onOpen={vi.fn()}
    onDelete={vi.fn()}
    onHome={vi.fn()}
    onSkills={vi.fn()}
    onSettings={vi.fn()}
  />, host);

  expect(host.querySelector('[data-session-activity="working"]')?.getAttribute("aria-label")).toBe("正在工作");
  expect(host.querySelector('[data-session-activity="disconnected"]')?.getAttribute("aria-label")).toBe("Agent 已断开");
  dispose();
});
