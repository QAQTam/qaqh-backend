// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createRawSessionState } from "../../store/sessionEventReducer";
import { createI18n, I18nCtx } from "../../i18n";
import EnvironmentPopover from "./EnvironmentPopover";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.innerHTML = ""; });

describe("EnvironmentPopover tasks", () => {
  it("expands task details and only asks when the question control is clicked", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const onTaskAction = vi.fn();
    const task = { id: "T1", subject: "实现审批", description: "接通计划审核", status: "in_progress" };
    dispose = render(() => (
      <I18nCtx value={createI18n("zh")}><EnvironmentPopover
          session={createRawSessionState("seed-1")}
          workspace="F:/repo"
          tasks={[task]}
          onTaskAction={onTaskAction}
        /></I18nCtx>
    ), host);

    expect(host.textContent).toContain("T1");
    expect(host.textContent).toContain("实现审批");
    expect(host.textContent).toContain("进行中");
    host.querySelector<HTMLButtonElement>(".environment-task-main")!.click();
    await Promise.resolve();
    expect(host.textContent).toContain("接通计划审核");
    expect(onTaskAction).not.toHaveBeenCalled();
    host.querySelector<HTMLButtonElement>(".environment-task-question")!.click();
    expect(onTaskAction).toHaveBeenCalledWith("ask", task);
    host.querySelector<HTMLButtonElement>(".environment-task-action")!.click();
    expect(onTaskAction).toHaveBeenCalledWith("cancel", task);
  });

  it("normalizes changed files and opens their Git diff", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const onOpenDiff = vi.fn();
    const session = createRawSessionState("seed-1");
    session.environment.changedFiles = ["F:\\repo\\src\\feature\\Panel.tsx"];
    dispose = render(() => (
      <I18nCtx value={createI18n("zh")}><EnvironmentPopover
        session={session} workspace={"F:\\repo"} onOpenDiff={onOpenDiff}
      /></I18nCtx>
    ), host);

    const file = host.querySelector<HTMLButtonElement>(".environment-file")!;
    expect(file.textContent).toBe("src/feature/Panel.tsx");
    file.click();
    expect(onOpenDiff).toHaveBeenCalledWith("src/feature/Panel.tsx");
  });
});
