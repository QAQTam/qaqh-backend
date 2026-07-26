// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { describe, expect, it, vi } from "vitest";

import AskUserPrompt from "./AskUserPrompt";
import CompactStatusRow from "./CompactStatusRow";
import InteractionDock from "./InteractionDock";
import PermissionPrompt, { type PermissionRequest } from "./PermissionPrompt";

function flush() {
  return new Promise((r) => setTimeout(r, 20));
}

// ── Test data ──

const permReq = (risk: PermissionRequest["risk"]): PermissionRequest => ({
  tool_call_id: "call-1",
  tool_name: "exec_run",
  reason: "Run a build command",
  paths: ["F:/repo"],
  category: "exec",
  level: 1,
  risk,
  consequence: "May execute arbitrary system commands.",
});

// ═══════════════════════════════════════════════════════════
// AskUserPrompt
// ═══════════════════════════════════════════════════════════
describe("AskUserPrompt", () => {
  it("1. disables submit when required question is unanswered", async () => {
    const host = document.createElement("div");
    document.body.append(host);

    const dispose = render(
      () => (
        <AskUserPrompt
          questions={[
            { id: "q1", question: "Proceed?", options: ["yes", "no"], allow_custom: false },
          ]}
          onSubmit={vi.fn()}
          onDismiss={vi.fn()}
        />
      ),
      host,
    );
    await flush();

    const submit = host.querySelector<HTMLButtonElement>(".interaction-submit");
    expect(submit).toBeTruthy();
    expect(submit!.disabled).toBe(true);

    dispose();
    host.remove();
  });

  it("2. submit produces correct AskAnswer[]", async () => {
    const onSubmit = vi.fn();
    const host = document.createElement("div");
    document.body.append(host);

    const dispose = render(
      () => (
        <AskUserPrompt
          questions={[
            { id: "q1", question: "A or B?", options: ["A", "B"], allow_custom: false },
          ]}
          onSubmit={onSubmit}
          onDismiss={vi.fn()}
        />
      ),
      host,
    );
    await flush();

    // Click option A
    const btnA = [...host.querySelectorAll(".interaction-option")].find(
      (b) => b.textContent?.trim() === "A",
    );
    expect(btnA).toBeTruthy();
    (btnA as HTMLElement).click();
    await flush();

    // Submit
    const submit = host.querySelector<HTMLButtonElement>(".interaction-submit")!;
    expect(submit.disabled).toBe(false);
    submit.click();
    await flush();

    expect(onSubmit).toHaveBeenCalledWith([{ question_id: "q1", answer: "A" }]);

    dispose();
    host.remove();
  });

  it("3. prevents double-submit during pending callback", async () => {
    let resolve: () => void;
    const onSubmit = vi.fn().mockImplementation(
      () => new Promise<void>((r) => { resolve = r; }),
    );

    const host = document.createElement("div");
    document.body.append(host);

    const dispose = render(
      () => (
        <AskUserPrompt
          questions={[
            { id: "q1", question: "OK?", options: ["ok"], allow_custom: false },
          ]}
          onSubmit={onSubmit}
          onDismiss={vi.fn()}
        />
      ),
      host,
    );
    await flush();

    // Select option
    const btn = host.querySelector(".interaction-option") as HTMLElement;
    btn.click();
    await flush();

    // Submit twice
    const submit = host.querySelector<HTMLButtonElement>(".interaction-submit")!;
    submit.click();
    submit.click();
    await flush();

    expect(onSubmit).toHaveBeenCalledTimes(1);

    // Resolve and check button recovered
    resolve!();
    await flush();

    dispose();
    host.remove();
  });
});

// ═══════════════════════════════════════════════════════════
// PermissionPrompt
// ═══════════════════════════════════════════════════════════
describe("PermissionPrompt", () => {
  it("4. high-risk approval uses red solid background class", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(
      () => <PermissionPrompt request={permReq("high")} onRespond={vi.fn()} />,
      host,
    );
    await flush();

    const approve = host.querySelector<HTMLButtonElement>(".interaction-approve");
    expect(approve).toBeTruthy();
    // Should have high-risk class
    expect(approve!.classList.contains("approval-high")).toBe(true);
    // Should contain red background
    const style = getComputedStyle(approve!);
    // The class should make it red; test presence of class + text
    expect(approve!.textContent).toContain("批准");
    // High risk should NOT be a plain "批准" — should express consequence
    // (we use "批准并执行" for exec, "批准" for low/medium)
    expect(approve!.textContent?.trim()).not.toBe("批准");

    dispose();
    host.remove();
  });

  it("5. low-risk approval does NOT use red class", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(
      () => <PermissionPrompt request={permReq("low")} onRespond={vi.fn()} />,
      host,
    );
    await flush();

    const approve = host.querySelector<HTMLButtonElement>(".interaction-approve");
    expect(approve!.classList.contains("approval-high")).toBe(false);

    dispose();
    host.remove();
  });

  it("6. shows paths and consequence text", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(
      () => <PermissionPrompt request={permReq("high")} onRespond={vi.fn()} />,
      host,
    );
    await flush();

    const text = host.textContent ?? "";
    expect(text).toContain("F:/repo");
    expect(text).toContain("May execute arbitrary system commands.");

    dispose();
    host.remove();
  });

  it("6b. shows the authoritative category and risk", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(
      () => <PermissionPrompt request={permReq("high")} onRespond={vi.fn()} />,
      host,
    );
    await flush();

    expect(host.querySelector("[data-permission-category]")?.textContent).toContain("exec");
    expect(host.querySelector("[data-permission-risk]")?.textContent).toContain("high");

    dispose();
    host.remove();
  });
});

// ═══════════════════════════════════════════════════════════
// CompactStatusRow
// ═══════════════════════════════════════════════════════════
describe("CompactStatusRow", () => {
  it("7. shows active / complete / failed states correctly", async () => {
    const host = document.createElement("div");
    document.body.append(host);

    const [status, setStatus] = createSignal<"active" | "complete" | "failed">("active");
    const [text, setText] = createSignal("正在整理上下文…");
    const turnsCompacted = 42;

    const dispose = render(
      () => (
        <CompactStatusRow
          active={status() === "active"}
          text={text()}
          status={status()}
          turnsCompacted={status() === "complete" ? turnsCompacted : undefined}
        />
      ),
      host,
    );
    await flush();

    // Active state
    expect(host.textContent).toContain("整理");
    expect(host.querySelector(".compact-active")).toBeTruthy();

    // Complete
    setStatus("complete");
    setText("已压缩 42 轮对话");
    await flush();
    expect(host.textContent).toContain("42");
    expect(host.querySelector(".compact-complete")).toBeTruthy();

    // Failed
    setStatus("failed");
    setText("压缩失败");
    await flush();
    expect(host.textContent).toContain("失败");
    expect(host.querySelector(".compact-failed")).toBeTruthy();

    dispose();
    host.remove();
  });

  it("7b. supplies an active fallback and expands text from its own button", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const onExpand = vi.fn();
    const longText = `${"摘要内容".repeat(20)}结尾标记`;
    const [text, setText] = createSignal("");
    const dispose = render(
      () => (
        <CompactStatusRow
          active
          text={text()}
          status="active"
          onExpand={onExpand}
        />
      ),
      host,
    );
    await flush();

    expect(host.textContent).toContain("正在整理上下文");
    setText(longText);
    await flush();
    expect(host.textContent).not.toContain("结尾标记");

    host.querySelector<HTMLButtonElement>(".compact-expand-btn")!.click();
    await flush();
    expect(host.textContent).toContain("结尾标记");
    expect(onExpand).toHaveBeenCalledTimes(1);

    dispose();
    host.remove();
  });
});

// ═══════════════════════════════════════════════════════════
// InteractionDock
// ═══════════════════════════════════════════════════════════
describe("InteractionDock", () => {
  it("8. does NOT use fullscreen overlay class", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(
      () => (
        <InteractionDock>
          <div data-child>test</div>
        </InteractionDock>
      ),
      host,
    );
    await flush();

    const dock = host.querySelector(".interaction-dock");
    expect(dock).toBeTruthy();

    // Must NOT be a fullscreen overlay
    expect(dock!.classList.contains("modal-overlay")).toBe(false);
    expect(dock!.classList.contains("ask-overlay")).toBe(false);

    // Position should be relative/static (not fixed covering the screen)
    const position = getComputedStyle(dock!).position;
    expect(position).not.toBe("fixed");

    dispose();
    host.remove();
  });
});
