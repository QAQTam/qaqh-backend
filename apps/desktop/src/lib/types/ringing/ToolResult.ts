import type { ContentRef } from "./ContentRef";
import type { JsonValue } from "./serde_json/JsonValue";

export type ToolStatus = "ok" | "error" | "partial" | "backgrounded" | "cancelled";

export type ToolContinuation = {
  tool: string;
  args: JsonValue;
};

export type ToolModelPayload = {
  text: string;
  truncated: boolean;
  total_tokens: number;
  continuation?: ToolContinuation | null;
};

export type ToolError = {
  code: string;
  message: string;
  retryable: boolean;
  hint?: string | null;
};

/** Canonical Ringing tool result. `status` is the only execution truth. */
export type ToolResult = {
  status: ToolStatus;
  summary: string;
  data: JsonValue;
  model: ToolModelPayload;
  output_ref?: ContentRef | null;
  error?: ToolError | null;
};
