// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { createSignal } from "solid-js";
import { expect, it, vi } from "vitest";
import type { TurnViewModel } from "../../presentation/turnProjection";
import TurnGroup from "./TurnGroup";

vi.mock("../../i18n", () => ({
  useI18n: () => ({
    t: () => ({ review: { changedFiles: "Changed {n} files", reviewChanges: "Review changes" } }),
  }),
}));

vi.mock("../MarkdownBody", () => ({
  default: (props: { content: string; final?: boolean }) => (
    <div data-markdown-final={props.final ? "true" : "false"}>{props.content}</div>
  ),
}));

function processEntry() {
  return {
    kind: "process" as const,
    id: "process-0",
    hasTools: true,
    items: [{
      kind: "tool" as const,
      id: "read-1",
      family: "read",
      toolName: "read",
      summary: "App.tsx",
      success: true,
    }],
  };
}

it("renders protocol-ordered assistant chats outside a tool process", () => {
  const host = document.createElement("div");
  const turn: TurnViewModel = {
    turnId: "turn-1",
    userPrompt: "请修复",
    status: "completed",
    elapsedMs: 4090,
    rounds: [{
      roundNum: 0,
      isFinal: false,
      entries: [
        { kind: "assistant", id: "assistant-1", markdown: "调用前说明", streaming: false },
        processEntry(),
        { kind: "assistant", id: "assistant-2", markdown: "调用后说明", streaming: false },
      ],
    }],
    interactions: [],
  };
  const dispose = render(() => <TurnGroup turn={turn} />, host);

  const group = host.querySelector("[data-turn]")!;
  expect(group.querySelectorAll('[data-part="assistant-answer"]')).toHaveLength(2);
  const process = group.querySelector('[data-part="process"]')!;
  expect(process.textContent).not.toContain("调用前说明");
  expect(process.textContent).not.toContain("调用后说明");
  expect(group.textContent).toContain("调用前说明");
  expect(group.textContent).toContain("调用后说明");
  dispose();
});

it("renders process entries inline (not in one aggregate panel)", () => {
  const host = document.createElement("div");
  const turn: TurnViewModel = {
    turnId: "turn-summary",
    userPrompt: "完成任务",
    status: "completed",
    rounds: [{
      roundNum: 0,
      isFinal: false,
      entries: [processEntry(), { ...processEntry(), id: "process-1" }],
    }],
    interactions: [],
  };
  const dispose = render(() => <TurnGroup turn={turn} />, host);

  // Consecutive process entries are merged into one group
  expect(host.querySelectorAll('[data-part="process"]')).toHaveLength(1);
  dispose();
});

it("renders a live answer-only entry as streaming Markdown", () => {
  const host = document.createElement("div");
  const turn: TurnViewModel = {
    turnId: "turn-streaming",
    userPrompt: "生成报告",
    status: "running",
    rounds: [{
      roundNum: 0,
      isFinal: false,
      entries: [{ kind: "assistant", id: "live-answer", markdown: "输出中", streaming: true }],
    }],
    interactions: [],
  };
  const dispose = render(() => <TurnGroup turn={turn} />, host);

  expect(host.querySelector('[data-markdown-final="false"]')?.textContent).toContain("输出中");
  dispose();
});

it("keeps assistant chats visible when the turn completes", () => {
  const host = document.createElement("div");
  const turn: TurnViewModel = {
    turnId: "turn-done",
    userPrompt: "开始",
    status: "completed",
    rounds: [{
      roundNum: 0,
      isFinal: false,
      entries: [{ kind: "assistant", id: "done-a", markdown: "完成", streaming: false }],
    }],
    interactions: [],
  };
  const dispose = render(() => <TurnGroup turn={turn} />, host);

  expect(host.querySelector('[data-markdown-final="true"]')?.textContent).toContain("完成");
  dispose();
});

it("reactively renders later reasoning and assistant chunks in the same mounted entries", async () => {
  const host = document.createElement("div");
  const initial: TurnViewModel = {
    turnId: "turn-live-deltas",
    userPrompt: "继续输出",
    status: "running",
    rounds: [{
      roundNum: 0,
      isFinal: false,
      entries: [
        {
          kind: "process",
          id: "live-reasoning",
          hasTools: false,
          items: [{ kind: "reasoning", id: "reasoning-0", content: "思考一" }],
        },
        { kind: "assistant", id: "live-answer", markdown: "回答一", streaming: true },
      ],
    }],
    interactions: [],
  };
  const [turn, setTurn] = createSignal(initial);
  const dispose = render(() => <TurnGroup turn={turn()} />, host);

  host.querySelector<HTMLButtonElement>(".process-event-trigger")!.click();
  await Promise.resolve();
  expect(host.querySelector(".process-detail")?.textContent).toContain("思考一");
  expect(host.querySelector('[data-markdown-final="false"]')?.textContent).toContain("回答一");

  setTurn(current => ({
    ...current,
    rounds: [{
      ...current.rounds[0]!,
      entries: [
        {
          kind: "process",
          id: "live-reasoning",
          hasTools: false,
          items: [{ kind: "reasoning", id: "reasoning-0", content: "思考一、思考二" }],
        },
        { kind: "assistant", id: "live-answer", markdown: "回答一、回答二", streaming: true },
      ],
    }],
  }));
  await Promise.resolve();

  expect(host.querySelector(".process-detail")?.textContent).toContain("思考一、思考二");
  expect(host.querySelector('[data-markdown-final="false"]')?.textContent).toContain("回答一、回答二");
  dispose();
});
