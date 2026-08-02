import type {
  AskMode,
  AskQuestion,
  PermissionRisk,
  SkillInfo,
} from "../lib/types/ringing";
import type { UsageInfo } from "../lib/types/ringing/UsageInfo";
import type { ToolResult } from "../lib/types/ringing/ToolResult";

/** Renderer-local display records. These deliberately do not mirror wire events. */
export type ToolCallDef = {
  id: string;
  name: string;
  args_display: string;
  args_json: string;
};

export type RoundBlockState = "open" | "sealed";
export type RoundBlockMeta = {
  blockId?: string;
  blockOrder?: number;
  state?: RoundBlockState;
};

export type RoundBlock =
  | ({ type: "reasoning"; content: string } & RoundBlockMeta)
  | ({ type: "text"; content: string } & RoundBlockMeta)
  | ({ type: "tool"; card: ToolCallDef } & RoundBlockMeta)
  | ({ type: "web_search"; action: string } & RoundBlockMeta)
  | ({ type: "notice"; message: string } & RoundBlockMeta);

export type TaskInfo = { id: string; subject: string; description: string; status: string };

export type SkillRuntimeInfo = {
  name: string;
  description: string;
  state: string;
  source: string;
  token_count: number;
  error?: string;
};

export type RoundPhase = "thinking" | "tool_calling" | "answering" | "complete";

export type TurnStatus = "running" | "waiting" | "completed" | "failed" | "cancelled";

export type TurnFailure = { code: string; message: string };

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
  toolResults: Record<string, ToolResult>;
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
  completion_tokens: number;
  total_tokens: number;
  reasoning_tokens: number;
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
  currentTodoId?: string | null;
};

export type RawTurn = {
  turnId: string;
  userText: string;
  status: TurnStatus;
  startedAt?: number;
  endedAt?: number;
  stopReason?: string;
  failure?: TurnFailure;
  usage?: UsageInfo;
  rounds: RawRound[];
  interactions: InteractionRecord[];
};

export type RawSessionState = {
  seed: string;
  turns: RawTurn[];
  /**
   * Transient provider-retry state. It is deliberately outside a turn so
   * retries do not mutate transcript content or turn terminal status.
   */
  providerRetry: {
    turnId: string;
    roundNum: number;
    attempt: number;
    maxRetries: number;
    delaySecs: number;
  } | null;
  pendingInteractions: PendingInteraction[];
  environment: {
    linesAdded: number;
    linesRemoved: number;
    filesCreated: number;
    filesDeleted: number;
    changedFiles: string[];
    /** Increments for every tool-reported write so Git views can refresh promptly. */
    gitRevision: number;
    /** If true, the cache prefix key changed since last turn. */
    cachePrefixChanged: boolean;
    /** Component names that changed (e.g. "system_prompt", "catalog"). */
    cacheChangeReasons: string[];
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
    usageTotals: UsageInfo;
    usageByRequest: Record<string, UsageInfo>;
    usageRequestCount: number;
    cacheReportedRequestCount: number;
    /** Increments on every Dashboard event so views can refresh. */
    dashboardRevision: number;
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

const emptyUsage = (): UsageInfo => ({
  prompt_tokens: 0,
  completion_tokens: 0,
  total_tokens: 0,
  prompt_cache_hit_tokens: 0,
  prompt_cache_miss_tokens: 0,
  reasoning_tokens: 0,
  cache_usage_reported: false,
});

/** A renderer-local shell. Authoritative transcript/control data arrives on Ringing. */
export function createRawSessionState(seed: string): RawSessionState {
  return {
    seed,
    turns: [],
    providerRetry: null,
    pendingInteractions: [],
    environment: {
      linesAdded: 0,
      linesRemoved: 0,
      filesCreated: 0,
      filesDeleted: 0,
      changedFiles: [],
      gitRevision: 0,
      cachePrefixChanged: false,
      cacheChangeReasons: [],
    },
    session: {
      ready: false,
      hasMore: false,
      totalTurns: 0,
      tokensUsed: 0,
      cacheHitPct: 0,
      contextLimit: 0,
      usageTotals: emptyUsage(),
      usageByRequest: {},
      usageRequestCount: 0,
      cacheReportedRequestCount: 0,
      dashboardRevision: 0,
    },
    dashboard: { tasks: [], recentEdits: [], activity: [], currentTodoId: null },
    telemetry: [],
    skills: {
      available: [], active: [], catalogRevision: "", contextEpoch: 0,
      operationRevision: 0, tokenBudget: 0, tokenUsage: 0, runtime: [], diagnostics: [],
    },
    notices: [],
    compact: { active: false, text: "", turnsCompacted: null, completionRevision: 0 },
  };
}

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
