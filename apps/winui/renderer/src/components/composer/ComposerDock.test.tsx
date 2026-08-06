// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { afterEach, expect, it, vi } from "vitest";
import { createFollowUpQueue } from "../../store/followUpQueue";
import ComposerDock from "./ComposerDock";

vi.mock("../../runtime/desktopApi", () => ({
  openImageDialog: vi.fn(),
  readFileBase64: vi.fn(),
  readTextFile: vi.fn(),
}));

afterEach(() => {
  document.body.innerHTML = "";
});

it("keeps the draft and displays an immediate send failure", async () => {
  const onSend = vi.fn().mockRejectedValue(new Error("Ringing command failed"));
  const queue = createFollowUpQueue("seed", vi.fn().mockResolvedValue(undefined));
  const host = document.createElement("div");
  document.body.append(host);
  const dispose = render(() => <ComposerDock
    isStreaming={() => false}
    hasPendingGate={() => false}
    queue={queue}
    onSend={onSend}
    onStop={vi.fn().mockResolvedValue(undefined)}
    mode="code"
    onModeChange={vi.fn()}
    permissionLevel={2}
    onPermissionLevelChange={vi.fn()}
  />, host);

  const textarea = host.querySelector("textarea")!;
  textarea.value = "keep this draft";
  textarea.dispatchEvent(new InputEvent("input", { bubbles: true }));
  // Solid 2 signal writes become observable on the next microtask.
  await Promise.resolve();
  host.querySelector<HTMLButtonElement>(".composer-send")!.click();
  await new Promise(resolve => setTimeout(resolve, 0));

  expect(onSend).toHaveBeenCalledWith("keep this draft", [], undefined);
  expect(host.querySelector('[role="alert"]')?.textContent).toContain("Ringing command failed");
  expect(textarea.value).toBe("keep this draft");
  dispose();
});
