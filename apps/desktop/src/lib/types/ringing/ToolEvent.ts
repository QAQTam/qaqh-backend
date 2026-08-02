// Ringing tool-domain events. Keep terminal failures inside ToolResult.status.
import type { ContentRef } from "./ContentRef";
import type { NoticeLevel } from "./NoticeLevel";
import type { PermissionCategory } from "./PermissionCategory";
import type { PermissionRisk } from "./PermissionRisk";
import type { ToolResult } from "./ToolResult";

export type ToolEvent =
  | { type: "tool_call_prepared"; tool_call_id: string; turn_id: string; round_num: number; name: string; args_so_far: string }
  | { type: "tool_started"; tool_call_id: string; turn_id: string; round_num: number; name: string }
  | { type: "tool_progress"; tool_call_id: string; turn_id: string; round_num: number; stream: string; seq_start: number; seq_end: number; chunk: string; dropped_bytes: number; truncated: boolean }
  | { type: "tool_finished"; tool_call_id: string; turn_id: string; round_num: number; result: ToolResult }
  | { type: "tool_permission_requested"; tool_call_id: string; turn_id: string; round_num: number; tool_name: string; reason: string; paths: string[]; category: PermissionCategory; level: number; risk: PermissionRisk; consequence: string }
  | { type: "tool_notice"; tool_call_id?: string | null; level: NoticeLevel; message: string }
  | { type: "audit_recorded"; tool_name: string; result_summary: string; success: boolean; time: string; args_ref?: ContentRef | null }
  | { type: "code_changed"; lines_added: number; lines_removed: number; files_created: number; files_deleted: number; file?: string | null };
