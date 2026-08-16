// Ringing V1 wire 类型 — 与 crates/qaqh-ringing + crates/qaqh-domain 的 serde 形状严格对应。

export const SCHEMA = 'qaqh.Ringing';
export const VERSION = 1;
export const BASE = '/ringing/v1';
export const SESSION_HEADER = 'X-QAQH-Client-Session-Id';
export const CAPABILITIES = [
  'Ringing_v1',
  'Ringing_batch_v1',
  'Ringing_bootstrap_v1',
  'Ringing_command_status_v1',
] as const;

export type RingingChannel = 'control' | 'conversation' | 'tool';

// ── open ────────────────────────────────────────────────────────────────────

export interface ClientOpenRequest {
  schema: string;
  version: number;
  client_instance_id: string;
  capabilities: string[];
}

export interface ClientOpenResponse {
  schema: string;
  version: number;
  accepted: boolean;
  client_session_id: string;
  capabilities: string[];
  server_epoch: string;
  lease_ttl_ms: number;
  renew_interval_ms: number;
}

// ── command envelope ────────────────────────────────────────────────────────

export interface CommandEnvelope {
  schema: string;
  version: number;
  channel: RingingChannel;
  command_id: string;
  client_instance_id: string;
  client_session_id: string;
  seed?: string;
  expected_revision?: number;
  command: Record<string, unknown>;
}

export interface RingingCommandAck {
  command_id: string;
  status: 'accepted' | 'rejected';
  code?: string;
  message?: string;
  retry_after_ms?: number;
}

export interface RingingCommandStatus {
  command_id: string;
  state: 'accepted' | 'running' | 'succeeded' | 'failed' | 'rejected';
  payload_fingerprint: string;
  terminal_event_id?: string;
  error_code?: string;
}

// ── event envelope ──────────────────────────────────────────────────────────

export interface EventEnvelope {
  delivery: string;
  seed: string;
  stream_seq: number;
  channel_seq: number;
  session_seq: number;
  event_id: string;
  causation_id?: string;
  correlation_id?: string;
  state_revision?: number;
  server_ts?: number;
  event: DomainEvent;
}

export interface DomainEvent {
  channel: RingingChannel;
  type: string;
  [key: string]: unknown;
}

export interface ResetRequired {
  channel: RingingChannel;
  seed: string;
  earliest_available_seq: number;
  reason?: string;
}

// ── timeline ────────────────────────────────────────────────────────────────

export interface TimelinePage {
  schema: string;
  version: number;
  server_epoch: string;
  seed: string;
  snapshot: TimelineSnapshot;
  has_more: boolean;
  total_turns: number;
}

export interface TimelineSnapshot {
  watermark: number;
  turns: TimelineTurn[];
}

export interface TimelineTurn {
  turn_id: string;
  created_seq: number;
  user_text: string;
  sealed: boolean;
  state: 'running' | 'completed' | 'failed' | 'cancelled';
  failure?: { code: string; message: string };
  rounds: TimelineRound[];
}

export interface TimelineRound {
  round_num: number;
  sealed: boolean;
  is_final: boolean;
  blocks: TimelineBlock[];
}

export type TimelineBlockKind = 'reasoning' | 'text' | 'tool' | 'notice';
export type TimelineBlockState = 'open' | 'sealed';

export interface TimelineBlock {
  block_id: string;
  block_order: number;
  kind: TimelineBlockKind;
  state: TimelineBlockState;
  text: string;
  tool?: TimelineTool;
}

export type TimelineToolState = 'prepared' | 'running' | 'succeeded' | 'failed';

export interface TimelineTool {
  tool_call_id: string;
  name: string;
  state: TimelineToolState;
  summary?: string;
  args_json?: string;
  output?: string;
  progress: string;
  failure?: { code: string; message: string };
  permission?: {
    reason: string;
    paths: string[];
    category: string;
    level: number;
    risk: string;
    consequence: string;
  };
}

// ── session meta（session.list 返回）────────────────────────────────────────

export interface SessionMeta {
  seed: string;
  created_at: number;
  updated_at: number;
  model: string;
  message_count: number;
  turn_count: number;
  last_summary: string;
  archived: boolean;
  ephemeral: boolean;
  title?: string | null;
  cwd?: string | null;
  tool_mode?: string;
  running: boolean;
  workspace_id?: string | null;
}

// ── config（config.load 返回）───────────────────────────────────────────────

export interface WebConfig {
  api_key: string;
  api_key_set: boolean;
  model: string;
  base_url: string;
  provider_id: string;
  endpoint: string;
  max_tokens: number;
  context_limit: number;
  reasoning_effort: string;
  permission_level: number;
  lang?: string | null;
  workspace: { mode: string };
  subagent: { model: string; api_key_set: boolean; default_tools: string[] };
}

// ── content ─────────────────────────────────────────────────────────────────

export interface ContentRef {
  content_id: string;
  media_type: string;
  sha256: string;
  truncated: boolean;
}

// ── 常用命令构造（与 domain serde 形状一致）────────────────────────────────

export function sessionCreate(closeCurrent = false, cwd?: string): Record<string, unknown> {
  const cmd: Record<string, unknown> = {
    channel: 'control',
    type: 'session_create',
    close_current: closeCurrent,
  };
  if (cwd) cmd.cwd = cwd;
  return cmd;
}

export function sessionResume(seed: string): Record<string, unknown> {
  return { channel: 'control', type: 'session_resume', seed };
}

export function sendMessage(text: string, asSystem = false): Record<string, unknown> {
  return { channel: 'conversation', type: 'conversation_send_message', text, as_system: asSystem };
}

export function cancelTurn(turnId?: string): Record<string, unknown> {
  const cmd: Record<string, unknown> = {
    channel: 'conversation',
    type: 'conversation_cancel',
  };
  if (turnId) cmd.turn_id = turnId;
  return cmd;
}

export function permissionRespond(
  toolCallId: string,
  approved: boolean,
  trustFolder = false,
): Record<string, unknown> {
  return {
    channel: 'tool',
    type: 'tool_permission_respond',
    tool_call_id: toolCallId,
    approved,
    trust_folder: trustFolder,
  };
}

export function planReviewRespond(
  interactionId: string,
  approved: boolean,
  message?: string,
  autonomous = false,
): Record<string, unknown> {
  const cmd: Record<string, unknown> = {
    channel: 'control',
    type: 'plan_review_respond',
    interaction_id: interactionId,
    approved,
    autonomous,
  };
  if (message) cmd.message = message;
  return cmd;
}

export function askRespond(
  interactionId: string,
  answers: { question_id: string; answer: string }[],
): Record<string, unknown> {
  return {
    channel: 'control',
    type: 'interaction_ask_respond',
    interaction_id: interactionId,
    answers,
  };
}

export function askDismiss(interactionId: string): Record<string, unknown> {
  return {
    channel: 'control',
    type: 'interaction_ask_dismiss',
    interaction_id: interactionId,
  };
}
