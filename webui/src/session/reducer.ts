/**
 * Timeline reducer:后端 timeline 事件/快照 → SessionState(spec §6 纯函数)。
 *
 * 单源原则:结构与顺序事实只来自后端(`timeline_seq`/事件),这里不发明任何
 * 领域事实;时钟采样仅用于耗时(见 lib/time.ts TS-1)。所有函数直接变更传入
 * 的 draft(Solid store setter 的 draft 语义),测试里就是普通对象。
 */
import { emptySession, type SessionState, type Step, type TimelineEntryWire, type ToolDisplay, type ToolOutput, type ToolPermission, type ToolStatus, type Turn } from "./types";
import { now } from "../lib/time";

/** 实时输出尾窗上限:运行中工具的 progress 只保留尾部 16KB。 */
const PROGRESS_WINDOW = 16 * 1024;

function turnKeyOf(turnIndex: number | null | undefined, turnId: string): string {
  return typeof turnIndex === "number" ? `#${turnIndex}` : turnId;
}

function ensureTurn(draft: SessionState, entry: TimelineEntryWire): Turn {
  const existing = draft.turns[entry.turn_id];
  if (existing) return existing;
  const turn: Turn = {
    id: entry.turn_id,
    key: turnKeyOf(null, entry.turn_id),
    turnIndex: null,
    user: { text: "" },
    steps: [],
    answer: null,
    answerStepId: null,
    status: "running",
    workStartedAt: now(),
    expanded: false,
  };
  draft.turns[turn.id] = turn;
  draft.slots.push({ kind: "turn", key: turn.key });
  return turn;
}

function findStep(turn: Turn, blockId: string): Step | undefined {
  return turn.steps.find((step) => step.id === blockId);
}

function ensureReasoning(turn: Turn, blockId: string, seq: number): Step {
  const existing = findStep(turn, blockId);
  if (existing) return existing;
  const step = { kind: "thinking" as const, id: blockId, seq, startedAt: now(), text: "" };
  insertBySeq(turn, step);
  return step;
}

function ensureTool(turn: Turn, blockId: string, seq: number, tool: Record<string, any> | undefined): Step {
  const existing = findStep(turn, blockId);
  if (existing) return existing;
  const step = {
    kind: "tool" as const,
    id: blockId,
    seq,
    startedAt: now(),
    name: String(tool?.name ?? ""),
    argsJson: typeof tool?.args_json === "string" ? tool.args_json : tool?.argsJson,
    status: normalizeToolStatus(tool?.state ?? "prepared"),
    display: (tool?.display as ToolDisplay | undefined) ?? undefined,
    elapsedMs: typeof tool?.metrics?.elapsed_ms === "number" ? tool.metrics.elapsed_ms : undefined,
    permission: normalizePermission(tool?.permission),
    progressTruncated: tool?.progress_truncated === true,
  };
  if (step.status === "running" || step.status === "pending") step.startedAt = now();
  insertBySeq(turn, step);
  return step;
}

function ensureText(turn: Turn, blockId: string, seq: number): Extract<Step, { kind: "text" }> {
  const existing = findStep(turn, blockId);
  if (existing?.kind === "text") return existing;
  const step = { kind: "text" as const, id: blockId, seq, startedAt: now(), text: "" };
  insertBySeq(turn, step);
  return step;
}

function insertBySeq(turn: Turn, step: Step): void {
  let index = turn.steps.length;
  while (index > 0 && (turn.steps[index - 1]!.seq ?? 0) > (step.seq ?? 0)) index -= 1;
  turn.steps.splice(index, 0, step);
}

export function normalizeToolStatus(state: unknown): ToolStatus {
  switch (state) {
    case "prepared":
      return "pending";
    case "running":
      return "running";
    case "succeeded":
      return "success";
    case "failed":
      return "error";
    case "cancelled":
      return "aborted";
    case "backgrounded":
      return "backgrounded";
    default:
      return "pending";
  }
}

function normalizePermission(raw: any): ToolPermission | null {
  if (!raw || typeof raw !== "object") return null;
  return {
    reason: String(raw.reason ?? ""),
    paths: Array.isArray(raw.paths) ? raw.paths.map(String) : [],
    category: String(raw.category ?? ""),
    level: Number(raw.level ?? 0),
    risk: String(raw.risk ?? ""),
    consequence: String(raw.consequence ?? ""),
  };
}

/** 旧字段回退:display 缺失时从 output 信封/diff 文本构造 ToolOutput。 */
export function legacyOutput(tool: Record<string, any>): ToolOutput | undefined {
  const rawOutput = typeof tool.output === "string" ? tool.output : "";
  const rawDiff = typeof tool.diff === "string" ? tool.diff : "";
  if (!rawOutput && !rawDiff) return undefined;
  let text = rawOutput;
  let stderr: string | undefined;
  let exitCode: number | undefined;
  const parsed = parseJsonish(rawOutput);
  if (parsed) {
    if (typeof parsed.output === "string") text = parsed.output;
    if (typeof parsed.stderr === "string" && parsed.stderr) stderr = parsed.stderr;
    if (typeof parsed.exit_code === "number") exitCode = parsed.exit_code;
  }
  return { text, stderr, exitCode, truncated: tool.progress_truncated === true, diffText: rawDiff || undefined };
}

export function parseJsonish(value: string | undefined): Record<string, any> | null {
  if (!value?.startsWith("{")) return null;
  try {
    return JSON.parse(value) as Record<string, any>;
  } catch {
    return null;
  }
}

/** display 投影(权威)→ ToolOutput;缺失回退旧字段。 */
export function outputFromDisplay(display: ToolDisplay | undefined, legacy: ToolOutput | undefined): ToolOutput | undefined {
  const body = display?.body;
  if (!body || typeof body !== "object") return legacy;
  switch (body.kind) {
    case "text":
      return { text: String(body.text ?? ""), truncated: body.truncated === true };
    case "diff":
      return { diffText: String(body.unified ?? ""), truncated: body.truncated === true };
    case "shell":
      return {
        text: String(body.output ?? ""),
        exitCode: typeof body.exit_code === "number" ? body.exit_code : undefined,
        truncated: body.truncated === true,
      };
    case "streams":
      return {
        text: String(body.stdout ?? ""),
        stderr: String(body.stderr ?? "") || undefined,
        exitCode: typeof body.exit_code === "number" ? body.exit_code : undefined,
        truncated: body.truncated === true,
      };
    case "subagent":
      return { text: `subagent:${String(body.name ?? "")} ${String(body.session_id ?? "")}` };
    default:
      return legacy;
  }
}

/**
 * 应用一条实时 timeline 条目。返回 state 本身(便于链式);`watermark` 由调用方
 * 在去重/缺口判定后推进。
 */
export function applyEntry(draft: SessionState, entry: TimelineEntryWire): SessionState {
  const seq = typeof entry.timeline_seq === "number" ? entry.timeline_seq : 0;
  const turn = ensureTurn(draft, entry);
  const event = entry.event ?? {};
  const ts = now();

  switch (event.type) {
    case "turn_opened": {
      turn.user.text = typeof event.user_text === "string" ? event.user_text : turn.user.text;
      break;
    }
    case "block_opened": {
      const block = event.block ?? {};
      const blockId = String(block.block_id ?? "");
      if (!blockId) break;
      if (block.kind === "reasoning") {
        ensureReasoning(turn, blockId, seq);
        draft.activeReasoningId = blockId;
      } else if (block.kind === "tool") {
        const step = ensureTool(turn, blockId, seq, block.tool);
        // 新工具/思考开始 → 之前的流式作答降级为中间叙述(spec 模型外的真实事件)。
        demoteAnswerIfStale(turn);
        if (step.kind === "tool" && step.status === "pending" && (block.tool?.state ?? "prepared") === "running") {
          step.status = "running";
        }
      } else if (block.kind === "text") {
        const step = ensureText(turn, blockId, seq);
        promoteAnswer(turn, step);
      }
      break;
    }
    case "text_delta": {
      const blockId = String(event.block_id ?? "");
      const step = findStep(turn, blockId);
      const delta = typeof event.delta === "string" ? event.delta : "";
      if (step?.kind === "thinking") {
        step.text += delta;
      } else if (step?.kind === "text") {
        step.text += delta;
        promoteAnswer(turn, step);
      }
      // tool 块没有 text_delta(输出走 tool_progress)。
      break;
    }
    case "block_checkpoint": {
      const blockId = String(event.block_id ?? "");
      const step = findStep(turn, blockId);
      if (!step) break;
      if (typeof event.text === "string" && event.text) {
        // 全量替换(丢失 delta 或文本旁移后的重对齐)。
        const target = step.kind === "thinking" || step.kind === "text" ? step : null;
        if (target != null) {
          target.text = event.text;
          if (target.kind === "text") promoteAnswer(turn, target);
        }
      } else if (typeof event.arg === "string" && event.arg) {
        const textStep = step.kind === "text" ? step : null;
        if (textStep != null) {
          textStep.text += event.arg;
          promoteAnswer(turn, textStep);
        }
      }
      break;
    }
    case "tool_updated": {
      const blockId = String(event.block_id ?? "");
      const step = findStep(turn, blockId);
      const rawTool = event.tool;
      if (step?.kind !== "tool" || !rawTool) break;
      step.name = String(rawTool.name ?? step.name);
      if (typeof rawTool.args_json === "string") step.argsJson = rawTool.args_json;
      step.display = (rawTool.display as ToolDisplay | undefined) ?? step.display;
      step.elapsedMs = typeof rawTool.metrics?.elapsed_ms === "number" ? rawTool.metrics.elapsed_ms : step.elapsedMs;
      step.permission = normalizePermission(rawTool.permission) ?? step.permission;
      const nextState = normalizeToolStatus(rawTool.state);
      if (nextState !== step.status) {
        step.status = nextState;
        if (nextState === "running" && step.startedAt == null) step.startedAt = ts;
        if (isTerminalToolStatus(nextState)) {
          step.endedAt = ts;
          step.output = outputFromDisplay(step.display, legacyOutput(rawTool));
          if (nextState === "error") {
            step.error = String(rawTool.failure?.message ?? rawTool.failure?.code ?? "");
          }
        }
      } else if (!step.output) {
        // 运行中也可能先到 display/output 快照。
        const output = outputFromDisplay(step.display, legacyOutput(rawTool));
        if (output) step.output = output;
      }
      break;
    }
    case "tool_progress": {
      const step = findStep(turn, String(event.block_id ?? ""));
      if (step?.kind !== "tool") break;
      const chunk = typeof event.chunk === "string" ? event.chunk : "";
      if (!chunk) break;
      const next = (step.progressTail ?? "") + chunk;
      step.progressTail = next.length > PROGRESS_WINDOW ? next.slice(-PROGRESS_WINDOW) : next;
      step.progressTruncated = step.progressTruncated || event.truncated === true;
      break;
    }
    case "block_sealed": {
      const blockId = String(event.block_id ?? "");
      const step = findStep(turn, blockId);
      if (step?.kind === "thinking") {
        step.endedAt = ts;
        if (draft.activeReasoningId === blockId) draft.activeReasoningId = null;
      }
      break;
    }
    case "turn_sealed": {
      turn.status =
        event.state === "failed" ? "failed" : event.state === "cancelled" ? "aborted" : "done";
      if (turn.status === "failed") {
        turn.error = {
          message: String(event.failure?.message ?? event.failure?.code ?? "回合失败"),
        };
      }
      if (turn.answer == null) {
        // 无最终作答(中断/失败):最后一个文本块仍原样保留为 step。
        turn.answer = null;
        turn.answerStepId = null;
      }
      break;
    }
    default:
      break;
  }
  return draft;
}

function isTerminalToolStatus(status: ToolStatus): boolean {
  return status === "success" || status === "error" || status === "aborted" || status === "backgrounded";
}

/** 文本块开始/增长 → 成为最终作答候选(折叠 step 区,流式展示)。
 *  仅当它仍是当前最后一个 step(其后没有新开的工具/思考)才提升,
 *  否则它是中间叙述,保持为普通 step。 */
function promoteAnswer(turn: Turn, step: Extract<Step, { kind: "text" }>): void {
  const last = turn.steps[turn.steps.length - 1];
  if (last == null || last.id !== step.id) return;
  turn.answerStepId = step.id;
  if (turn.answer == null) {
    turn.answer = { text: step.text, startedAt: now() };
  } else {
    turn.answer.text = step.text;
  }
}

/** 作答候选之后又开了新工具/思考 → 降级回中间叙述,step 区恢复实时展开。 */
function demoteAnswerIfStale(turn: Turn): void {
  if (turn.answerStepId == null) return;
  turn.answer = null;
  turn.answerStepId = null;
}

// ── 快照/翻页 ────────────────────────────────────────────────────────────────

/** 由快照回合记录构建 Turn(全新对象)。 */
export function buildTurn(raw: Record<string, unknown>, ts: number): Turn {
  const turnId = String(raw.turn_id ?? "");
  const turnIndex = typeof raw.turn_index === "number" ? raw.turn_index : null;
  const turn: Turn = {
    id: turnId,
    key: turnKeyOf(turnIndex, turnId),
    turnIndex,
    user: { text: String(raw.user_text ?? "") },
    steps: [],
    answer: null,
    answerStepId: null,
    status:
      raw.state === "running"
        ? "running"
        : raw.state === "failed"
          ? "failed"
          : raw.state === "cancelled"
            ? "aborted"
            : "done",
    workStartedAt: undefined,
    expanded: false,
  };
  if (turn.status === "failed") {
    const failure = (raw.failure ?? {}) as Record<string, any>;
    turn.error = { message: String(failure.message ?? failure.code ?? "回合失败") };
  }
  let firstStepAt: number | undefined;
  for (const rawRound of (raw.rounds as Array<Record<string, unknown>>) ?? []) {
    for (const rawBlock of (rawRound.blocks as Array<Record<string, unknown>>) ?? []) {
      const blockId = String(rawBlock.block_id ?? "");
      const kind = String(rawBlock.kind ?? "");
      const state = String(rawBlock.state ?? "sealed");
      const text = String(rawBlock.text ?? "");
      if (kind === "reasoning") {
        const step: Extract<Step, { kind: "thinking" }> = { kind: "thinking", id: blockId, seq: 0, text };
        // 快照里已 sealed 的思考块标为已结束;运行中的保持 open(供思考链恢复)。
        if (state === "sealed") step.endedAt = ts;
        turn.steps.push(step);
      } else if (kind === "tool") {
        const rawTool = (rawBlock.tool ?? {}) as Record<string, any>;
        const display = (rawTool.display as ToolDisplay | undefined) ?? undefined;
        const status = normalizeToolStatus(rawTool.state);
        const step: Extract<Step, { kind: "tool" }> = {
          kind: "tool",
          id: blockId,
          seq: 0,
          name: String(rawTool.name ?? ""),
          argsJson: typeof rawTool.args_json === "string" ? rawTool.args_json : rawTool.argsJson,
          status,
          display,
          elapsedMs: typeof rawTool.metrics?.elapsed_ms === "number" ? rawTool.metrics.elapsed_ms : undefined,
          permission: normalizePermission(rawTool.permission),
          progressTail: typeof rawTool.progress === "string" ? rawTool.progress : undefined,
          progressTruncated: rawTool.progress_truncated === true,
        };
        if (isTerminalToolStatus(status)) {
          step.output = outputFromDisplay(display, legacyOutput(rawTool));
          if (status === "error") {
            step.error = String(rawTool.failure?.message ?? rawTool.failure?.code ?? "");
          }
        }
        turn.steps.push(step);
      } else if (kind === "text") {
        turn.steps.push({ kind: "text", id: blockId, seq: 0, text });
      }
      firstStepAt ??= ts;
    }
  }
  turn.workStartedAt = firstStepAt;
  // 答案 = 最后一个文本块(快照里回合已定形)。
  for (let i = turn.steps.length - 1; i >= 0; i -= 1) {
    const step = turn.steps[i]!;
    if (step.kind === "text") {
      turn.answerStepId = step.id;
      turn.answer = { text: step.text, startedAt: turn.workStartedAt };
      break;
    }
  }
  // 失败/中断回合没有答案语义:错误块由 status 驱动渲染。
  if (turn.status !== "done" && turn.answer) {
    turn.answer = { ...turn.answer };
  }
  return turn;
}

/** 全量快照替换状态(attach/重连重对齐/回底部重拉)。 */
export function applySnapshot(
  draft: SessionState,
  page: {
    server_epoch?: string;
    snapshot?: { watermark?: number; turns?: Array<Record<string, unknown>> };
    has_more?: boolean;
    total_turns?: number;
    truncated_before?: boolean;
  },
  ts: number = now(),
): SessionState {
  draft.turns = {};
  draft.slots = [];
  for (const raw of page.snapshot?.turns ?? []) {
    const turn = buildTurn(raw, ts);
    draft.turns[turn.key] = turn;
    draft.slots.push({ kind: "turn", key: turn.key });
  }
  draft.watermark = page.snapshot?.watermark ?? 0;
  draft.serverEpoch = page.server_epoch ?? null;
  draft.hasMore = page.has_more === true;
  draft.truncatedBefore = page.truncated_before === true;
  draft.totalTurns = page.total_turns ?? draft.slots.length;
  const oldest = (page.snapshot?.turns ?? [])[0];
  draft.oldestIndex = typeof oldest?.turn_index === "number" ? oldest.turn_index : null;
  // 恢复正在流式的 reasoning(重连/切标签后思考链仍在)。
  draft.activeReasoningId = findOpenReasoningId(draft);
  return draft;
}

/** 翻页前插:按 `turn_index` 去重(后端明示 turn_id 会复用,不能当稳定键)。 */
export function prependPage(
  draft: SessionState,
  page: {
    snapshot?: { turns?: Array<Record<string, unknown>> };
    has_more?: boolean;
    total_turns?: number;
  },
  ts: number = now(),
): SessionState {
  const incoming = page.snapshot?.turns ?? [];
  const newSlots: SessionState["slots"] = [];
  let oldestAdded: number | null = null; // 本页实际新增的最旧回合 = 下一次翻页游标
  for (const raw of incoming) {
    const turnIndex = typeof raw.turn_index === "number" ? raw.turn_index : null;
    const turnId = String(raw.turn_id ?? "");
    const clash = Object.values(draft.turns).some(
      (turn) =>
        (turnIndex != null && turn.turnIndex === turnIndex) ||
        (turnIndex == null && turn.id === turnId),
    );
    if (clash) continue;
    const turn = buildTurn(raw, ts);
    draft.turns[turn.key] = turn;
    newSlots.push({ kind: "turn", key: turn.key });
    if (oldestAdded == null && turnIndex != null) oldestAdded = turnIndex;
  }
  draft.slots.unshift(...newSlots);
  draft.hasMore = page.has_more === true;
  if (page.total_turns != null) draft.totalTurns = page.total_turns;
  if (oldestAdded != null) {
    draft.oldestIndex = oldestAdded;
  } else if (incoming.length > 0 && newSlots.length === 0) {
    // 整页都是重复:游标保持不变,has_more 照页面如实记录。
  } else {
    draft.hasMore = false;
  }
  return draft;
}

/** 保留运行中回合;返回窗口里(非占位)回合数。 */
export function countLoadedTurns(draft: SessionState): number {
  return draft.slots.filter((slot) => slot.kind === "turn").length;
}

export function findOpenReasoningId(draft: SessionState): string | null {
  // 快照里 state==="running" 的 reasoning 块没有显式 open 标记;后端 snapshot
  // 的块只有 sealed/未 sealed —— 这里按「最后一个未完成回合中的 reasoning 块」
  // 近似:实时路径由 block_sealed 显式清除,快照路径由 applySnapshot 恢复。
  const turns = Object.values(draft.turns);
  for (let i = turns.length - 1; i >= 0; i -= 1) {
    const turn = turns[i]!;
    if (turn.status !== "running") continue;
    for (let j = turn.steps.length - 1; j >= 0; j -= 1) {
      const step = turn.steps[j]!;
      if (step.kind === "thinking" && step.endedAt == null) return step.id;
    }
  }
  return null;
}

export { emptySession };
