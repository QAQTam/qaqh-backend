/**
 * 前端数据模型(spec §6),按后端 timeline 协议适配:
 *
 * - `seq` = 后端 `timeline_seq`(会话内单调递增,由 daemon 分配);顺序与去重
 *   一律以它为准。
 * - [契约缺口 TS-1] 后端条目目前不带 epoch-ms 时间戳,`startedAt/endedAt` 由
 *   前端时钟采样(见 lib/time.ts 模块注释),仅用于耗时展示。
 * - 后端模型是 turn/round/block;此处把它归约为 spec 的 Turn/Step:
 *   reasoning 块 → ThinkingStep,tool 块 → ToolStep,text 块 → 答案或(多轮回合
 *   时)中间叙述 TextStep。
 */
export type Seq = number;

export type TurnStatus = "running" | "done" | "aborted" | "failed";

export interface Turn {
  /** 后端 turn_id。注意 id 可能复用,稳定去重键见 `key`。 */
  id: string;
  /** 稳定键:快照/翻页回合用 `#turn_index`,实时回合(无全局序号)用 id。 */
  key: string;
  turnIndex: number | null;
  user: { text: string };
  /** 按 seq 升序(后端真相)。 */
  steps: Step[];
  /** 最终作答:最后一个文本块;运行中指向当前流式文本块,若其后又开了工具则回退为中间叙述。 */
  answer: { text: string; startedAt?: number } | null;
  /** answer 指向的 step id;用于作答开始后回退重分类。 */
  answerStepId: string | null;
  status: TurnStatus;
  workStartedAt?: number;
  error?: { message: string };
  /** 时间线里是否被用户展开。 */
  expanded: boolean;
}

export interface ThinkingStep {
  kind: "thinking";
  id: string;
  seq: Seq;
  startedAt?: number;
  endedAt?: number;
  /** 完整思考文本,始终保留(spec §10.2)。 */
  text: string;
}

export interface TextStep {
  kind: "text";
  id: string;
  seq: Seq;
  startedAt?: number;
  endedAt?: number;
  text: string;
}

export type ToolStatus = "pending" | "running" | "success" | "error" | "denied" | "aborted" | "backgrounded";

export interface ToolOutput {
  text?: string;
  stderr?: string;
  exitCode?: number;
  truncated?: boolean;
  /** 后端 unified diff 文本(display.body.diff.unified 或旧字段 diff)。 */
  diffText?: string;
}

export interface ToolStep {
  kind: "tool";
  id: string;
  seq: Seq;
  startedAt?: number;
  endedAt?: number;
  name: string;
  /** 原始入参 JSON 串(MUST NOT 改写;展示层解析)。 */
  argsJson?: string;
  status: ToolStatus;
  output?: ToolOutput;
  error?: string;
  /** 运行中输出尾窗(16KB 有界)。 */
  progressTail?: string;
  progressTruncated?: boolean;
  /** 后端展示投影(权威,缺省时回退旧字段)。 */
  display?: ToolDisplay;
  /** 后端 metrics。 */
  elapsedMs?: number;
  permission?: ToolPermission | null;
}

export type Step = ThinkingStep | TextStep | ToolStep;

/** 后端 TimelineToolDisplay 的透明转发(类型宽松,未知变体原样保留)。 */
export interface ToolDisplay {
  summary?: string;
  header?: Record<string, any>;
  body?: Record<string, any>;
  metrics?: Record<string, any>;
  outcome?: any;
  [k: string]: any;
}

export interface ToolPermission {
  reason: string;
  paths: string[];
  category: string;
  level: number;
  risk: string;
  consequence: string;
}

/** 用户等待区间(授权/AskUser),挂在其发生时正在运行的回合上。 */
export interface Wait {
  turnKey: string | null;
  kind: "approval" | "ask";
  from: number;
  to?: number;
}

/** 会话级归约状态(reducer 的操作对象;Solid store draft)。 */
export interface SessionState {
  turns: Record<string, Turn>;
  /** 渲染顺序槽:回合或(被淘汰回合的)等高占位。 */
  slots: Slot[];
  watermark: number;
  serverEpoch: string | null;
  hasMore: boolean;
  truncatedBefore: boolean;
  /** 最旧已加载回合的全局序号(翻页游标,排他);null = 禁用翻页。 */
  oldestIndex: number | null;
  totalTurns: number;
  /** 当前正在流式输出的 reasoning 块 id(单行思考链)。 */
  activeReasoningId: string | null;
  waits: Wait[];
}

export type Slot = { kind: "turn"; key: string } | { kind: "placeholder"; key: string; height: number };

export function emptySession(): SessionState {
  return {
    turns: {},
    slots: [],
    watermark: 0,
    serverEpoch: null,
    hasMore: false,
    truncatedBefore: false,
    oldestIndex: null,
    totalTurns: 0,
    activeReasoningId: null,
    waits: [],
  };
}

/** 后端 timeline 条目(线协议形状,字段宽松)。 */
export interface TimelineEntryWire {
  timeline_seq: number;
  turn_id: string;
  round_num?: number;
  event: Record<string, any>;
}
