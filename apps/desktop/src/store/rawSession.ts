import type {
  AskMode,
  AskQuestion,
  PermissionRisk,
  RoundBlock,
  SkillInfo,
  SkillRuntimeInfo,
  TaskInfo,
  ToolCallDef,
  ToolResultDef,
  UsageInfo,
} from "../lib/types";

export type RoundPhase = "thinking" | "tool_calling" | "answering" | "complete";

export type TurnStatus = "running" | "waiting" | "completed" | "failed" | "cancelled";

export type RawProgressChunk = {
  stream: "stdout" | "stderr";
  seq: number;
  chunk: string;
};

export type RawProgress = { chunks: RawProgressChunk[] };

export type RawRound = {
  roundNum: number;
  isFinal: boolean;
  thinking: string;
  answer: string;
  blocks: RoundBlock[];
  toolCalls: ToolCallDef[];
  toolResults: Record<string, ToolResultDef>;
  progress: Record<string, RawProgress>;
  phase: RoundPhase;
};

export type InteractionRecord = {
  id: string;
  kind: "permission" | "ask" | "plan";
  resolution: string;
  at: number;
};

export type RawMetricPoint = {
  ts: number;
  /** Provider-confirmed request input tokens; not the local context estimate. */
  prompt_tokens: number;
  cache_hit: number;
  cache_miss: number;
  /** Providers commonly omit cache usage. Zero/zero is not a 0% cache hit rate. */
  cache_available: boolean;
  /** Allows Dashboard and TurnEnd to refer to the same immutable usage snapshot. */
  sample_key: string;
};

export type RawActivityEntry = {
  toolName: string;
  summary: string;
  success: boolean;
  time: string;
  args: string;
};

export type TodoActivationItem = {
  id: string;
  title: string;
  description: string;
  complexity: string;
};

type InteractionBase = { id: string; turnId: string };

export type PendingInteraction =
  | (InteractionBase & {
      kind: "permission";
      toolName: string;
      reason: string;
      paths: string[];
      category: string;
      level: number;
      risk: PermissionRisk;
      consequence: string;
    })
  | (InteractionBase & {
      kind: "ask";
      roundNum: number;
      mode: AskMode;
      questions: AskQuestion[];
    })
  | (InteractionBase & { kind: "plan"; content: string; reviewType?: string; todoItems?: TodoActivationItem[] | null });

export type DashboardData = {
  tasks: TaskInfo[];
  recentEdits: string[];
};

export type RawTurn = {
  turnId: string;
  userText: string;
  status: TurnStatus;
  startedAt?: number;
  endedAt?: number;
  stopReason?: string;
  usage?: UsageInfo;
  rounds: RawRound[];
  interactions: InteractionRecord[];
};

export type RawSessionState = {
  seed: string;
  turns: RawTurn[];
  pendingInteractions: PendingInteraction[];
  environment: {
    linesAdded: number;
    linesRemoved: number;
    filesCreated: number;
    filesDeleted: number;
    changedFiles: string[];
    /** Increments for every tool-reported write so Git views can refresh promptly. */
    gitRevision: number;
  };
  session: {
    ready: boolean;
    hasMore: boolean;
    totalTurns: number;
    tokensUsed: number;
    cacheHitPct: number;
    title?: string;
    model?: string;
    contextLimit: number;
    usage?: UsageInfo;
  };
  dashboard: DashboardData & { activity: RawActivityEntry[] };
  telemetry: RawMetricPoint[];
  skills: {
    available: SkillInfo[];
    active: string[];
    catalogRevision: string;
    contextEpoch: number;
    operationRevision: number;
    tokenBudget: number;
    tokenUsage: number;
    runtime: SkillRuntimeInfo[];
    diagnostics: string[];
  };
  notices: Array<{ level: string; message: string; at: number }>;
  compact: {
    active: boolean;
    text: string;
    turnsCompacted: number | null;
    completionRevision: number;
  };
};

export function emptyRawRound(roundNum: number): RawRound {
  return {
    roundNum,
    isFinal: false,
    thinking: "",
    answer: "",
    blocks: [],
    toolCalls: [],
    toolResults: {},
    progress: {},
    phase: "thinking",
  };
}
