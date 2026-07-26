// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createI18n, I18nCtx } from "../i18n";
import type { RawSessionState } from "../store/rawSession";
import { createRawSessionState } from "../store/sessionEventReducer";
import { createSessionUiState } from "../store/sessionUiState";
import ChatView from "./ChatView";

vi.mock("../runtime/backendClient", () => ({ request: vi.fn().mockResolvedValue(undefined) }));
vi.mock("../runtime/desktopApi", () => ({ openPath: vi.fn().mockResolvedValue(undefined) }));

const cleanups: Array<() => void> = [];

afterEach(() => {
  cleanups.splice(0).forEach(dispose => dispose());
  document.body.innerHTML = "";
});

const flush = () => new Promise(resolve => setTimeout(resolve, 0));

function mountRawChat(initial: RawSessionState) {
  const [state, setState] = createSignal(initial);
  const ui = createSessionUiState();
  ui.setWorkspace("F:/repo");
  const callbacks = {
    onAskSubmit: vi.fn().mockResolvedValue(undefined),
    onAskDismiss: vi.fn().mockResolvedValue(undefined),
    onPermissionRespond: vi.fn().mockResolvedValue(undefined),
    onPlanRespond: vi.fn().mockResolvedValue(undefined),
    onTaskAction: vi.fn().mockResolvedValue(undefined),
    onLoadMore: vi.fn().mockResolvedValue(undefined),
  };
  const host = document.createElement("div");
  document.body.append(host);
  const i18n = createI18n("zh");
  cleanups.push(render(() => (
    <I18nCtx value={i18n}>
      <ChatView
        rawSession={state}
        ui={ui}
        onLoadMore={callbacks.onLoadMore}
        onAskSubmit={callbacks.onAskSubmit}
        onAskDismiss={callbacks.onAskDismiss}
        onPermissionRespond={callbacks.onPermissionRespond}
        onPlanRespond={callbacks.onPlanRespond}
        onTaskAction={callbacks.onTaskAction}
        onUndo={vi.fn()}
        permissionLevel={2}
        onPermissionLevelChange={vi.fn()}
        onChangeWorkspace={vi.fn()}
      />
    </I18nCtx>
  ), host));
  return { host, state, setState, ui, callbacks };
}

describe("ChatView blocking interactions", () => {
  it("renders only the first raw interaction and forwards its typed id", async () => {
    const state = createRawSessionState("seed-1");
    state.pendingInteractions.push(
      {
        kind: "ask", id: "ask-1", turnId: "t1", roundNum: 0, mode: "single",
        questions: [{ id: "q1", question: "Continue?", options: ["yes"], allow_custom: false }],
      },
      { kind: "plan", id: "plan-1", turnId: "t1", content: "# Later" },
    );
    const { callbacks } = mountRawChat(state);
    const dialog = document.body.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain("Continue?");
    expect(dialog.textContent).not.toContain("Later");
    dialog.querySelector<HTMLButtonElement>(".interaction-option")!.click();
    await flush();
    dialog.querySelector<HTMLButtonElement>(".interaction-submit")!.click();
    await flush();
    expect(callbacks.onAskSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ id: "ask-1", kind: "ask" }),
      [{ question_id: "q1", answer: "yes" }],
    );
  });

  it("renders raw permission and compact completion state", async () => {
    const state = createRawSessionState("seed-1");
    state.pendingInteractions.push({
      kind: "permission", id: "call-1", turnId: "t1", toolName: "exec_run",
      reason: "Run", paths: ["F:/repo"], category: "exec", level: 1,
      risk: "high", consequence: "May execute commands",
    });
    state.compact = { active: false, text: "", turnsCompacted: 8, completionRevision: 1 };
    const { callbacks, host } = mountRawChat(state);
    document.body.querySelector<HTMLButtonElement>(".approval-high")!.click();
    await flush();
    expect(callbacks.onPermissionRespond).toHaveBeenCalledWith(
      expect.objectContaining({ id: "call-1", kind: "permission" }), true, false,
    );
    expect(host.querySelector(".compact-complete")?.textContent).toContain("8 轮对话");
  });
});
