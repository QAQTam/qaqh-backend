// Native Ringing V1 timeline wire model. This intentionally has no Ringing channel,
// per-channel sequence, or legacy event payload: one session transcript advances
// through exactly one ordered cursor.

export type TimelineToolState = "prepared" | "running" | "succeeded" | "failed";
export type TimelineTurnState = "running" | "completed" | "failed" | "cancelled";
export type TimelineBlockKind = "reasoning" | "text" | "tool" | "notice";
export type TimelineBlockState = "open" | "sealed";

export type TimelineFailure = { code: string; message: string };
export type TimelineToolPermission = {
  reason: string;
  paths: string[];
  category: string;
  level: number;
  risk: string;
  consequence: string;
};
export type TimelineTool = {
  tool_call_id: string;
  name: string;
  state: TimelineToolState;
  summary?: string;
  args_json?: string;
  output?: string;
  progress?: string;
  failure?: TimelineFailure;
  permission?: TimelineToolPermission;
};
export type TimelineBlock = {
  block_id: string;
  block_order: number;
  kind: TimelineBlockKind;
  state: TimelineBlockState;
  text?: string;
  tool?: TimelineTool;
};
export type TimelineRound = {
  round_num: number;
  sealed: boolean;
  is_final: boolean;
  blocks: TimelineBlock[];
};
export type TimelineTurn = {
  turn_id: string;
  user_text: string;
  sealed: boolean;
  state: TimelineTurnState;
  failure?: TimelineFailure;
  rounds: TimelineRound[];
};
export type TimelineSnapshot = { watermark: number; turns: TimelineTurn[] };

export type TimelineEvent =
  | { type: "turn_opened"; user_text: string }
  | { type: "block_opened"; block: TimelineBlock }
  | { type: "text_delta"; block_id: string; fragment_seq: number; delta: string }
  | { type: "tool_updated"; block_id: string; tool: TimelineTool }
  | { type: "tool_progress"; block_id: string; chunk: string }
  | { type: "block_sealed"; block_id: string }
  | { type: "round_sealed"; is_final: boolean }
  | { type: "turn_sealed"; state: TimelineTurnState; failure?: TimelineFailure };

export type TimelineEntry = {
  timeline_seq: number;
  turn_id: string;
  round_num?: number;
  event: TimelineEvent;
};

export type TimelineSnapshotResponse = {
  schema: "deepx.Ringing";
  version: 1;
  server_epoch: string;
  seed: string;
  snapshot: TimelineSnapshot;
};

export type TimelineSseFrame = {
  schema: "deepx.Ringing";
  version: 1;
  server_epoch: string;
  seed: string;
  entry: TimelineEntry;
};
