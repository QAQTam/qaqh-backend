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
import type { TimelineEntry } from "../api/qaqh/TimelineEntry";
import type { TimelineToolDisplay } from "../api/qaqh/TimelineToolDisplay";
import type { TimelineToolPermission } from "../api/qaqh/TimelineToolPermission";

/**
 * wire 契约的本地别名。形状由 ts-rs 从 Rust 单一真相生成(`just ts-export` →
 * `src/api/qaqh/`),这里**不再手抄字段**——后端加槽/改语义时,漂移会变成
 * typecheck 失败而不是静默读空。
 */
export type ToolDisplay = TimelineToolDisplay;
export type ToolPermission = TimelineToolPermission;
export type TimelineEntryWire = TimelineEntry;

export type Seq = number;

export type TurnStatus = "running" | "done" | "aborted" | "failed";

export interface Turn {
  /** 后端 turn_id。注意 id 可能复用,稳定去重键见 `key`。 */
  id: string;
  /**
   * 稳定键:快照/翻页回合用 `#turn_index`,实时回合(无全局序号)用 id。
   * 权威页一旦把序号对上这个回合,键**保持本地那个**(换键 = 换 DOM 行,
   * 正在流式的回合会闪断并丢滚动位置),序号记到 `turnIndex` 上。
   */
  key: string;
  /** 会话内全局序号;实时路径开出来的回合先为 null,由权威页补记(也是翻页游标)。 */
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
  /** 该 reasoning 块所属回合 key:思考链只读这一个回合,不遍历 turns。 */
  activeReasoningTurnKey: string | null;
  /** 运行中的回合数(发送/授权判定的唯一依据,避免遍历 turns)。 */
  runningTurns: number;
  /** 失败回合数(标签状态点用,同样避免遍历 turns)。 */
  failedTurns: number;
  /** 当前运行中回合的 key;无运行中回合为 null(等待区间归属判定)。 */
  activeTurnKey: string | null;
  waits: Wait[];
}

/**
 * 渲染顺序槽:回合,或**一段**连续被淘汰的回合。
 *
 * 占位为什么是一段而不是一格:被淘汰的回合永远是相邻前缀,一格一个占位会让
 * `slots` 随翻页无界增长(实测 201 槽里 151 是占位)——每次前插都要重写约 170 个
 * 下标节点、`<For>` 每次重排整表、DOM 里多挂 150 个空 div。并成一段后
 * `slots ≈ 窗口 + 1`。
 */
export type Slot = { kind: "turn"; key: string } | { kind: "gap"; key: string; spans: GapSpan[] };

/** 段内一个被淘汰回合的等高占位高度(逐回合钳过 2 屏,合并后不再重钳)。 */
export interface GapSpan {
  key: string;
  height: number;
}

export function gapHeight(spans: GapSpan[]): number {
  return spans.reduce((sum, span) => sum + span.height, 0);
}

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
    activeReasoningTurnKey: null,
    runningTurns: 0,
    failedTurns: 0,
    activeTurnKey: null,
    waits: [],
  };
}
