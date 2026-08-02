// Ringing v1 typed Desktop backend 路由。
//
// 协议模式在 Electron main 的连接协商时固定；renderer 不维护 mode、seed/channel
// 没有 per-seed/channel 开关。命令只能通过 typed backend 进入 Ringing。

export type RingingChannelName = "control" | "conversation" | "tool";

export interface RingingCommandSpec {
  channel: RingingChannelName;
  /** 返回领域命令对象（type/字段均为 snake_case）；null = 该方法暂不可经 Ringing 路由。 */
  build: (params: Record<string, unknown>) => Record<string, unknown> | null;
}

/** legacy RPC 方法 → Ringing 领域命令映射。 */
export const RINGING_COMMAND_METHODS: Record<string, RingingCommandSpec> = {
  "session.send_message": {
    channel: "conversation",
    build: (params) => {
      // 文件路径由 Electron main 读取并上传为 ContentRef；renderer 只负责
      // 构造领域命令，不把本地路径放进 Ringing wire。
      const images =
        Array.isArray(params.images) && params.images.length > 0
          ? params.images.map((img) => ({
              mime_type: String((img as { mimeType?: string; mime_type?: string }).mimeType
                ?? (img as { mimeType?: string; mime_type?: string }).mime_type
                ?? ""),
              data: String((img as { data?: string }).data ?? ""),
            }))
          : undefined;
      return {
        type: "conversation_send_message",
        text: String(params.text ?? ""),
        ...(images && images.length > 0 ? { images } : {}),
      };
    },
  },
  "session.cancel": {
    channel: "conversation",
    build: () => ({ type: "conversation_cancel" }),
  },
  "session.compact": {
    channel: "conversation",
    build: () => ({ type: "conversation_compact" }),
  },
  "session.undo_turn": {
    channel: "conversation",
    build: (params) => ({
      type: "conversation_undo_turn",
      turn_id: String(params.turnId ?? params.turn_id ?? ""),
    }),
  },
  "session.set_mode": {
    channel: "conversation",
    build: (params) => ({
      type: "conversation_set_mode",
      mode: String(params.mode ?? "normal"),
    }),
  },
  "session.load_more_turns": {
    channel: "conversation",
    build: (params) => ({
      type: "conversation_load_more",
      before_turn_id: String(params.beforeTurnId ?? params.before_turn_id ?? ""),
      count: Number(params.count ?? 20) || 20,
    }),
  },
  "session.close": {
    channel: "control",
    build: (params) => ({ type: "session_close", seed: String(params.seed ?? "") }),
  },
  "session.delete": {
    channel: "control",
    build: (params) => ({ type: "session_close", seed: String(params.seed ?? "") }),
  },
  "session.resume": {
    channel: "control",
    build: (params) => ({ type: "session_resume", seed: String(params.seed ?? "") }),
  },
  "session.new": {
    channel: "control",
    build: () => ({ type: "session_create", close_current: false }),
  },
  "interaction.ask_response": {
    channel: "control",
    build: (params) => ({
      type: "interaction_ask_respond",
      interaction_id: String(params.askId ?? params.interaction_id ?? ""),
      answers: Array.isArray(params.answers) ? params.answers : [],
    }),
  },
  "interaction.ask_dismiss": {
    channel: "control",
    build: (params) => ({
      type: "interaction_ask_dismiss",
      interaction_id: String(params.askId ?? params.interaction_id ?? ""),
    }),
  },
  "interaction.plan_review": {
    channel: "control",
    build: (params) => ({
      type: "plan_review_respond",
      interaction_id: String(params.callId ?? params.interaction_id ?? ""),
      approved: params.approved === true,
      message: typeof params.message === "string" ? params.message : null,
      autonomous: params.autonomous === true,
    }),
  },
  "interaction.permission": {
    channel: "tool",
    build: (params) => ({
      type: "tool_permission_respond",
      tool_call_id: String(params.toolCallId ?? params.tool_call_id ?? ""),
      approved: params.approved === true,
      trust_folder: params.trustFolder === true || params.trust_folder === true,
    }),
  },
  "skills.operation": {
    channel: "control",
    build: (params) => ({
      type: "skills_operation",
      operation_id: String(params.operationId ?? params.operation_id ?? ""),
      action: String(params.action ?? ""),
      name: String(params.name ?? ""),
    }),
  },
  "skills.reload": {
    channel: "control",
    build: () => ({ type: "skills_reload" }),
  },
};

/** 可经 /ringing/v1/queries 的只读方法（与后端白名单一致）。 */
export const RINGING_QUERY_METHODS: ReadonlySet<string> = new Set([
  "daemon.version",
  "session.list",
  "session.meta",
  "session.activity",
  "session.dashboard",
  "session.get_activity",
  "workspace.get",
  "workspace.status",
  "config.load",
  "skills.list_tools",
  "todo.status",
  "plan.read",
  "plan.context_stats",
  "stats.token_usage",
  "git.diff",
  "git.branch",
  "git.branches",
  "git.file_diff",
]);

function randomCommandId(): string {
  return `cmd-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/** 构造发送给 main 进程的 Ringing 命令信封（command_id 由 renderer 生成）。 */
export function buildRingingCommandEnvelope(
  seed: string,
  channel: RingingChannelName,
  command: Record<string, unknown>,
  expectedRevision?: unknown,
): { command_id: string; command: unknown; seed?: string; expected_revision?: number | null } {
  return {
    command_id: randomCommandId(),
    // The wire envelope has a top-level channel, but RingingCommand is also
    // internally tagged by channel on the Rust side. Keep both discriminators
    // aligned at this boundary instead of making every command builder repeat it.
    command: { ...command, channel },
    seed: seed || undefined,
    expected_revision: typeof expectedRevision === "number" ? expectedRevision : null,
  };
}
