import type { Agent2Ui } from "../lib/types";

const EVENT_TYPES: ReadonlySet<Agent2Ui["type"]> = new Set([
  "turn_start", "turn_end", "round_delta", "round_complete", "tool_results",
  "tool_exec_delta", "session_restored", "more_turns", "session_created", "error",
  "tool_notice", "plan_submitted", "plan_resolved", "dashboard", "done",
  "compact_start", "compact_end", "compact_delta", "cancelled", "shutdown_ack",
  "ready", "audit_record", "exec_progress", "tool_call_preview", "code_delta",
  "pong", "skills_changed", "skill_operation_resolved", "permission_request", "ask_user", "ask_resolved",
  "ask_rejected",
]);

export function parseAgentEvent(payload: unknown): Agent2Ui {
  if (typeof payload !== "object" || payload === null || Array.isArray(payload)) {
    throw new Error("agent event must be an object");
  }
  const type = (payload as { type?: unknown }).type;
  if (typeof type !== "string" || !EVENT_TYPES.has(type as Agent2Ui["type"])) {
    throw new Error(`unknown Agent2Ui event type: ${String(type)}`);
  }
  return payload as Agent2Ui;
}
