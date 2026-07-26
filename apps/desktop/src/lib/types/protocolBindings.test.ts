import { expect, it } from "vitest";
import type { Agent2Ui } from "./Agent2Ui";

type EventOf<T extends Agent2Ui["type"]> = Extract<Agent2Ui, { type: T }>;

it("exposes both generated plan lifecycle variants", () => {
  const submitted: EventOf<"plan_submitted"> = {
    type: "plan_submitted",
    call_id: "plan-1",
    plan_content: "# Plan",
  };
  const resolved: EventOf<"plan_resolved"> = {
    type: "plan_resolved",
    call_id: "plan-1",
    approved: true,
  };

  expect(submitted.type).toBe("plan_submitted");
  expect(resolved.type).toBe("plan_resolved");
});
