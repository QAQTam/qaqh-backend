// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { describe, expect, it, vi } from "vitest";
import PermissionPrompt, { approvalClass, type PermissionRequest } from "./PermissionPrompt";

const request = (risk: PermissionRequest["risk"]): PermissionRequest => ({
  tool_call_id: "call-1",
  tool_name: "exec_run",
  reason: "Run a command",
  paths: ["F:/repo"],
  category: "exec",
  level: 1,
  risk,
  consequence: "May execute arbitrary actions.",
});

describe("PermissionPrompt", () => {
  it.each([
    ["low", "approval-low"],
    ["medium", "approval-medium"],
    ["high", "approval-high"],
  ] as const)("maps %s risk to %s styling", (risk, className) => {
    const host = document.createElement("div");
    const dispose = render(() => (
      <PermissionPrompt request={request(risk)} onRespond={vi.fn()} />
    ), host);
    expect(host.querySelector("[data-approve]")?.classList.contains(className)).toBe(true);
    dispose();
  });

  it("keeps rejection neutral when approval is high risk", () => {
    const host = document.createElement("div");
    const dispose = render(() => (
      <PermissionPrompt request={request("high")} onRespond={vi.fn()} />
    ), host);
    expect(host.querySelector("[data-reject]")?.classList.contains("approval-high")).toBe(false);
    expect(approvalClass("high")).toBe("approval-high");
    dispose();
  });

  it("shows the active position in a permission batch", () => {
    const host = document.createElement("div");
    const dispose = render(() => (
      <PermissionPrompt
        request={request("medium")}
        progress={{ current: 2, total: 4 }}
        onRespond={vi.fn()}
      />
    ), host);

    expect(host.textContent).toContain("第 2/4 项");
    dispose();
  });
});
