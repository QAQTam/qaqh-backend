export type BatchableControlMessage = {
  type: string;
  seq?: number;
  seed?: string;
  event?: unknown;
  [key: string]: unknown;
};

type AgentEvent = {
  type?: string;
  turn_id?: string;
  round_num?: number;
  kind?: string;
  id?: string;
  tool_call_id?: string;
  stream?: string;
  delta?: string;
  [key: string]: unknown;
};

/** Coalesces transient stream revisions without delaying reliable events. */
export class ControlEventBatcher {
  private readonly pending: Array<{ key: string; message: BatchableControlMessage }> = [];

  push(message: BatchableControlMessage): BatchableControlMessage[] {
    const event = agentEvent(message);
    const key = event ? transientKey(message, event) : undefined;
    if (!event || !key) {
      return message.type === "event" || message.type === "snapshot" || message.type === "shutdown"
        ? [...this.flush(), message]
        : [message];
    }

    const previous = this.pending.at(-1);
    const previousMessage = previous?.key === key ? previous.message : undefined;
    if (previousMessage && event.type === "round_delta") {
      const previousEvent = agentEvent(previousMessage)!;
      previous!.message = {
        ...message,
        event: {
          ...event,
          delta: `${String(previousEvent.delta ?? "")}${String(event.delta ?? "")}`,
        },
      };
    } else if (previousMessage && event.type === "exec_progress") {
      const previousEvent = agentEvent(previousMessage)!;
      previous!.message = {
        ...message,
        event: {
          ...event,
          chunk: `${String(previousEvent.chunk ?? "")}${String(event.chunk ?? "")}`,
        },
      };
    } else if (previousMessage && event.type === "tool_call_preview") {
      previous!.message = message;
    } else {
      this.pending.push({ key, message });
    }
    return [];
  }

  flush(): BatchableControlMessage[] {
    const messages = this.pending.map(entry => entry.message);
    this.pending.length = 0;
    return messages;
  }

  get size(): number {
    return this.pending.length;
  }
}

function agentEvent(message: BatchableControlMessage): AgentEvent | undefined {
  return message.type === "event" && message.event && typeof message.event === "object"
    ? message.event as AgentEvent
    : undefined;
}

function transientKey(message: BatchableControlMessage, event: AgentEvent): string | undefined {
  if (event.type === "round_delta") {
    return [message.seed, event.type, event.turn_id, event.round_num, event.kind].join(":");
  }
  if (event.type === "tool_call_preview") {
    return [message.seed, event.type, event.turn_id, event.id].join(":");
  }
  if (event.type === "exec_progress") {
    return [message.seed, event.type, event.tool_call_id, event.stream].join(":");
  }
  return undefined;
}
