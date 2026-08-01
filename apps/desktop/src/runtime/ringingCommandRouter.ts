// Ringing 命令/查询路由（renderer 侧）。
//
// 命令方向协议独立于事件方向（PLAN 双协议共存）：默认 legacy；只有
// (seed, channel) 被标记为 commandProtocol=ringing 后，backendClient.request
// 才会把对应方法改走 Ringing HTTP 命令。标记来源：
// - 调试面板命令切流按钮；
// - localStorage["ringing.autoCommands"] 自动切流；
// - ringingMonitor.syncMode 从 main 的 mode 表恢复（renderer 刷新后仍生效）。
//
// 回退规则：Ringing 未连接（"ringing not connected"）时命令回退 legacy 一次
// （共存：新出旧进）；其余错误原样抛出，不允许静默吞错。

export type RingingChannelName = "control" | "conversation" | "tool";
export type CommandProtocol = "legacy" | "ringing";

const CHANNELS: readonly RingingChannelName[] = ["control", "conversation", "tool"];

const commandProtocols = new Map<string, Partial<Record<RingingChannelName, CommandProtocol>>>();

export function setCommandProtocol(
  seed: string,
  channel: RingingChannelName,
  protocol: CommandProtocol,
): void {
  const entry = commandProtocols.get(seed) ?? {};
  entry[channel] = protocol;
  commandProtocols.set(seed, entry);
}

/** 从 main 的 mode 表同步整组命令协议（reload 后恢复路由标记）。 */
export function applyCommandModes(
  seed: string,
  modes: Record<string, { eventProtocol: string; commandProtocol: string }>,
): void {
  const entry = commandProtocols.get(seed) ?? {};
  for (const channel of CHANNELS) {
    const mode = modes[channel];
    if (mode) {
      entry[channel] = mode.commandProtocol === "ringing" ? "ringing" : "legacy";
    }
  }
  commandProtocols.set(seed, entry);
}

export function commandIsRinging(seed: string, channel: RingingChannelName): boolean {
  return commandProtocols.get(seed)?.[channel] === "ringing";
}

/** 该 seed 任一频道命令已切流（决定只读查询是否走 Ringing）。 */
export function sessionCommandsRinging(seed: string): boolean {
  const entry = commandProtocols.get(seed);
  return !!entry && CHANNELS.some((channel) => entry[channel] === "ringing");
}

export function resetCommandProtocols(): void {
  commandProtocols.clear();
}

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
      // 带 files 的消息保持 legacy：文件预览展开在 daemon 侧（service.rs
      // with_file_previews），renderer 沙箱内无法复刻。
      if (Array.isArray(params.files) && params.files.length > 0) return null;
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
      ...(params.expectedRevision != null || params.expected_revision != null
        ? { expected_revision: params.expectedRevision ?? params.expected_revision }
        : {}),
    }),
  },
  "skills.operation": {
    channel: "control",
    build: (params) => {
      const name = String(params.name ?? "");
      const action = String(params.action ?? "");
      if (action === "activate") return { type: "skills_activate", name };
      if (action === "release") return { type: "skills_release", name };
      return null; // retain 等动作暂不可经 Ringing 路由
    },
  },
  "skills.reload": {
    channel: "control",
    build: () => ({ type: "skills_reload" }),
  },
};

/** 可经 /ringing/v1/query 的只读方法（与后端白名单一致）。 */
export const RINGING_QUERY_METHODS: ReadonlySet<string> = new Set([
  "daemon.version",
  "session.list",
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
): { command_id: string; command: unknown; seed?: string } {
  return {
    command_id: randomCommandId(),
    command,
    seed: seed || undefined,
  };
}
