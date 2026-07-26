import type { Agent2Ui } from "../lib/types";

export interface AgentEventEffects {
  onSessionCreated(seed: string): void;
  onSessionRestored(seed: string): void;
  onDashboard(): void;
  onError(message: string): void;
  onCancelled(): void;
  onInteractionSettled(id: string): void;
  onReducerError(event: Agent2Ui, error: unknown): void;
}

export function dispatchAgentEvent(
  event: Agent2Ui,
  target: { push(event: Agent2Ui): void },
  effects: AgentEventEffects,
): void {
  try {
    target.push(event);
  } catch (error) {
    effects.onReducerError(event, error);
    return;
  }

  switch (event.type) {
    case "session_created": effects.onSessionCreated(event.seed); return;
    case "session_restored": effects.onSessionRestored(event.seed); return;
    case "dashboard": effects.onDashboard(); return;
    case "error": effects.onError(event.message); return;
    case "cancelled": effects.onCancelled(); return;
    case "ask_resolved": effects.onInteractionSettled(event.ask_id); return;
    case "ask_rejected":
      effects.onInteractionSettled(event.ask_id);
      effects.onError(event.message);
      return;
    case "plan_resolved": effects.onInteractionSettled(event.call_id); return;
    case "turn_start": case "turn_end": case "round_delta": case "round_complete":
    case "tool_results": case "tool_exec_delta": case "more_turns": case "tool_notice":
    case "plan_submitted": case "done": case "compact_start":
    case "compact_end": case "compact_delta": case "shutdown_ack": case "ready":
    case "audit_record": case "exec_progress": case "tool_call_preview": case "code_delta":
    case "pong": case "skills_changed": case "skill_operation_resolved": case "permission_request": case "ask_user":
      return;
    default: {
      const unreachable: never = event;
      throw new Error(`unhandled Agent2Ui side effect: ${JSON.stringify(unreachable)}`);
    }
  }
}
