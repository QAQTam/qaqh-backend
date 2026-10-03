/**
 * Timeline reducer:后端 timeline 事件/快照 → SessionState(spec §6 纯函数)。
 *
 * 单源原则:结构与顺序事实只来自后端(`timeline_seq`/事件),这里不发明任何
 * 领域事实;时钟采样仅用于耗时(见 lib/time.ts TS-1)。所有函数直接变更传入的
 * draft(Solid store setter 的 draft 语义),测试里就是普通对象。
 *
 * 因此快照回合并(`applySnapshot`/`prependPage`)刻意**不用** `reconcile`:那是
 * store 内部机制(要求 `$TARGET` 代理,普通对象上直接抛),而这里要保住「纯函数
 * + 普通对象可测」的契约。等价做法是「认得出同一个回合就保住原对象,只有内容真
 * 变了才整对象替换」——同一份权威数据再落地时,回合的订阅者一个都不重跑。
 */
import { emptySession, gapHeight, type SessionState, type Slot, type Step, type TimelineEntryWire, type ToolDisplay, type ToolOutput, type ToolPermission, type ToolStatus, type Turn } from "./types";
import type { TimelineSnapshot } from "../api/qaqh/TimelineSnapshot";
import type { TimelineTool } from "../api/qaqh/TimelineTool";
import type { TimelineTurn } from "../api/qaqh/TimelineTurn";
import { deepEqual } from "../lib/equal";
import { now } from "../lib/time";

/** 实时输出尾窗上限:运行中工具的 progress 只保留尾部 16KB。 */
const PROGRESS_WINDOW = 16 * 1024;

/**
 * 会改变派生答案(计数 / 活动回合 / 思考链锚点)的事件类型。
 * 只有这些事件之后才重算派生字段;`text_delta`/`tool_progress` 这类高频帧
 * 一个都不多读——「每帧遍历全表」就是当初流式期每帧 24ms 的来源。
 */
const DERIVED_EVENTS = new Set(["turn_opened", "turn_sealed", "block_opened", "block_sealed"]);

function turnKeyOf(turnIndex: number | null | undefined, turnId: string): string {
  return typeof turnIndex === "number" ? `#${turnIndex}` : turnId;
}

/**
 * 找到条目所属的本地回合。两条路径的 key 不同:实时条目只有 `turn_id`,权威页
 * 带全局序号(键是 `#i`)。快照先落地、同一个回合的后续条目后到达时,按 id 再认
 * 一次——否则会给同一个回合开第二行(上面那行冻结在快照内容,下面这行在流式)。
 * 只认**运行中**的回合:`turn_id` 会复用,而 reused 出来的旧回合早就密封了。
 */
function resolveTurn(draft: SessionState, entry: TimelineEntryWire): Turn | undefined {
  const direct = draft.turns[entry.turn_id];
  if (direct != null) return direct;
  for (let i = draft.slots.length - 1; i >= 0; i -= 1) {
    const slot = draft.slots[i]!;
    if (slot.kind !== "turn") continue;
    const turn = draft.turns[slot.key];
    if (turn != null && turn.status === "running" && turn.id === entry.turn_id) return turn;
  }
  return undefined;
}

function ensureTurn(draft: SessionState, entry: TimelineEntryWire): Turn {
  const existing = resolveTurn(draft, entry);
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

function ensureTool(turn: Turn, blockId: string, seq: number, tool: TimelineTool | undefined): Step {
  const existing = findStep(turn, blockId);
  if (existing) return existing;
  const step = {
    kind: "tool" as const,
    id: blockId,
    seq,
    startedAt: now(),
    name: tool?.name ?? "",
    argsJson: tool?.args_json ?? undefined,
    status: normalizeToolStatus(tool?.state ?? "prepared"),
    display: tool?.display ?? undefined,
    // metrics 挂在 display 上(TimelineTool 顶层没有 metrics 槽)。
    elapsedMs: tool?.display?.metrics?.elapsed_ms,
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

function normalizePermission(raw: unknown): ToolPermission | null {
  if (!raw || typeof raw !== "object") return null;
  const slot = raw as Partial<ToolPermission>;
  return {
    reason: String(slot.reason ?? ""),
    paths: Array.isArray(slot.paths) ? slot.paths.map(String) : [],
    category: String(slot.category ?? ""),
    level: Number(slot.level ?? 0),
    level_name: String(slot.level_name ?? ""),
    risk: String(slot.risk ?? ""),
    consequence: String(slot.consequence ?? ""),
  };
}

/** 旧字段回退:display 缺失时从 output 信封/diff 文本构造 ToolOutput。 */
export function legacyOutput(tool: TimelineTool): ToolOutput | undefined {
  const rawOutput = tool.output ?? "";
  const rawDiff = tool.diff ?? "";
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
  if (!body) return legacy;
  switch (body.kind) {
    case "text":
      return { text: body.text, truncated: body.truncated };
    case "diff":
      return { diffText: body.unified };
    case "shell":
      return { text: body.output, exitCode: body.exit_code ?? undefined, truncated: body.truncated };
    case "streams":
      return {
        text: body.stdout,
        stderr: body.stderr || undefined,
        exitCode: body.exit_code ?? undefined,
        truncated: body.truncated,
      };
    case "subagent":
      return { text: `subagent:${body.name} ${body.session_id}` };
    default:
      return legacy;
  }
}

/**
 * 失败槽文案。后端 `TimelineFailure.message` 在线上恒存在(无结构化 error 时是
 * **空串**),且契约把这种违约情况的显示交给 client 用裸 code(`tool_failure_of`)。
 * 因此回落链必须是 `message || code || fallback`——用 `??` 会让空串吃掉 code。
 */
function failureText(failure: unknown, fallback: string): string {
  const slot = (failure ?? {}) as { code?: unknown; message?: unknown };
  return String(slot.message || slot.code || fallback);
}

/**
 * 工具输出投影:display 权威、旧字段回退;退出码优先读顶层 `exit_code` 槽
 * (契约切片 2026-10-03,与 body 同源),legacy JSON 只服务没有 display 的工具。
 */
function deriveOutput(tool: TimelineTool, display: ToolDisplay | undefined): ToolOutput | undefined {
  const output = outputFromDisplay(display, legacyOutput(tool));
  const exitCode = tool.exit_code ?? undefined;
  return output == null || exitCode == null ? output : { ...output, exitCode };
}

/**
 * 应用一条实时 timeline 条目。返回 state 本身(便于链式);`watermark` 由调用方
 * 在去重/缺口判定后推进。
 */
export function applyEntry(draft: SessionState, entry: TimelineEntryWire): SessionState {
  const seq = entry.timeline_seq;
  const isNewTurn = resolveTurn(draft, entry) == null;
  const turn = ensureTurn(draft, entry);
  const event = entry.event;
  const ts = now();

  switch (event.type) {
    case "turn_opened": {
      turn.user.text = event.user_text;
      break;
    }
    case "block_opened": {
      const block = event.block;
      if (!block.block_id) break;
      if (block.kind === "reasoning") {
        ensureReasoning(turn, block.block_id, seq);
      } else if (block.kind === "tool") {
        const step = ensureTool(turn, block.block_id, seq, block.tool ?? undefined);
        // 新工具/思考开始 → 之前的流式作答降级为中间叙述(spec 模型外的真实事件)。
        demoteAnswerIfStale(turn);
        if (step.kind === "tool" && step.status === "pending" && (block.tool?.state ?? "prepared") === "running") {
          step.status = "running";
        }
      } else if (block.kind === "text") {
        const step = ensureText(turn, block.block_id, seq);
        promoteAnswer(turn, step);
      }
      break;
    }
    case "text_delta": {
      const step = findStep(turn, event.block_id);
      if (step?.kind === "thinking") {
        step.text += event.delta;
      } else if (step?.kind === "text") {
        step.text += event.delta;
        promoteAnswer(turn, step);
      }
      // tool 块没有 text_delta(输出走 tool_progress)。
      break;
    }
    case "block_checkpoint": {
      const step = findStep(turn, event.block_id);
      const target = step?.kind === "thinking" || step?.kind === "text" ? step : null;
      if (target == null) break;
      if (event.text) {
        // 全量替换(丢失 delta 或文本旁移后的重对齐)。
        target.text = event.text;
        if (target.kind === "text") promoteAnswer(turn, target);
      } else if (event.arg && target.kind === "text") {
        target.text += event.arg;
        promoteAnswer(turn, target);
      }
      break;
    }
    case "tool_updated": {
      const step = findStep(turn, event.block_id);
      const rawTool = event.tool;
      if (step?.kind !== "tool") break;
      step.name = rawTool.name;
      if (rawTool.args_json != null) step.argsJson = rawTool.args_json;
      step.display = rawTool.display ?? step.display;
      step.elapsedMs = rawTool.display?.metrics?.elapsed_ms ?? step.elapsedMs;
      step.permission = normalizePermission(rawTool.permission) ?? step.permission;
      const nextState = normalizeToolStatus(rawTool.state);
      if (nextState !== step.status) {
        step.status = nextState;
        if (nextState === "running" && step.startedAt == null) step.startedAt = ts;
        if (isTerminalToolStatus(nextState)) {
          // 终态时刻优先用后端发射时盖的 epoch ms(Tauri 侧 daemon 与 webview 同机,
          // 比「客户端收到」准);归档 rebuild 不带该槽 → 回退本地采样。
          step.endedAt = rawTool.completed_at_ms ?? ts;
          step.output = deriveOutput(rawTool, step.display);
          if (nextState === "error") step.error = failureText(rawTool.failure, "");
        }
      } else if (!step.output) {
        // 运行中也可能先到 display/output 快照。
        const output = deriveOutput(rawTool, step.display);
        if (output) step.output = output;
      }
      break;
    }
    case "tool_progress": {
      const step = findStep(turn, event.block_id);
      if (step?.kind !== "tool") break;
      const chunk = event.chunk;
      if (!chunk) break;
      const next = (step.progressTail ?? "") + chunk;
      step.progressTail = next.length > PROGRESS_WINDOW ? next.slice(-PROGRESS_WINDOW) : next;
      step.progressTruncated = step.progressTruncated || event.truncated === true;
      break;
    }
    case "block_sealed": {
      const step = findStep(turn, event.block_id);
      if (step?.kind === "thinking") step.endedAt = ts;
      break;
    }
    case "turn_sealed": {
      turn.status =
        event.state === "failed" ? "failed" : event.state === "cancelled" ? "aborted" : "done";
      if (turn.status === "failed") {
        turn.error = { message: failureText(event.failure, "回合失败") };
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
  // 只有会改变派生答案的事件(以及新回合入场)才重算;高频帧零遍历。
  if (isNewTurn || DERIVED_EVENTS.has(event.type)) recomputeDerived(draft);
  return draft;
}

function isTerminalToolStatus(status: ToolStatus): boolean {
  return status === "success" || status === "error" || status === "aborted" || status === "backgrounded";
}

/**
 * 派生字段的唯一计算点:`runningTurns` / `failedTurns` / `activeTurnKey` /
 * `activeReasoningId` + `activeReasoningTurnKey`。视图侧一律读这几个单值,绝不
 * 遍历 `turns`——遍历等于让每一次流式写入把整条链重算一遍。
 *
 * 这些值原先靠各事件里的 +/- 记账(6 个写入点要同步,漏一个就是静默错值),现在
 * 集中派生。代价只是每次结构事件多走一遍槽位表(≤ 窗口大小,且不在高频帧上)。
 */
export function recomputeDerived(draft: SessionState): void {
  let running = 0;
  let failed = 0;
  let activeTurnKey: string | null = null;
  for (const slot of draft.slots) {
    if (slot.kind !== "turn") continue;
    const turn = draft.turns[slot.key];
    if (turn == null) continue;
    if (turn.status === "running") {
      running += 1;
      activeTurnKey = slot.key; // 继续往后扫:要最新的运行中回合
    } else if (turn.status === "failed") {
      failed += 1;
    }
  }
  draft.runningTurns = running;
  draft.failedTurns = failed;
  draft.activeTurnKey = activeTurnKey;
  const open = findOpenReasoning(draft);
  draft.activeReasoningId = open?.id ?? null;
  draft.activeReasoningTurnKey = open?.turnKey ?? null;
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
export function buildTurn(raw: TimelineTurn, ts: number): Turn {
  const turnId = raw.turn_id;
  const turnIndex = raw.turn_index ?? null;
  const turn: Turn = {
    id: turnId,
    key: turnKeyOf(turnIndex, turnId),
    turnIndex,
    user: { text: raw.user_text },
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
    turn.error = { message: failureText(raw.failure, "回合失败") };
  }
  let firstStepAt: number | undefined;
  for (const round of raw.rounds) {
    for (const block of round.blocks) {
      const blockId = block.block_id;
      const kind = block.kind;
      const state = block.state ?? "sealed";
      const text = block.text ?? "";
      if (kind === "reasoning") {
        const step: Extract<Step, { kind: "thinking" }> = { kind: "thinking", id: blockId, seq: 0, text };
        // 快照里已 sealed 的思考块标为已结束;运行中的保持 open(供思考链恢复)。
        if (state === "sealed") step.endedAt = ts;
        turn.steps.push(step);
      } else if (kind === "tool") {
        const rawTool = block.tool;
        const display = rawTool?.display ?? undefined;
        const status = normalizeToolStatus(rawTool?.state ?? "prepared");
        const step: Extract<Step, { kind: "tool" }> = {
          kind: "tool",
          id: blockId,
          seq: 0,
          name: rawTool?.name ?? "",
          argsJson: rawTool?.args_json ?? undefined,
          status,
          display,
          elapsedMs: display?.metrics?.elapsed_ms,
          permission: normalizePermission(rawTool?.permission),
          progressTail: rawTool?.progress ?? undefined,
          progressTruncated: rawTool?.progress_truncated === true,
        };
        if (isTerminalToolStatus(status) && rawTool != null) {
          step.output = deriveOutput(rawTool, display);
          if (status === "error") step.error = failureText(rawTool.failure, "");
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

/**
 * 全量快照回合并(attach / 重连重对齐 / 回底部重拉)。
 *
 * 权威页**不是整表清空重建**:同一个回合保住同一个对象与同一个槽 key。清空重建
 * 会让每个回合的订阅者全部重跑(TurnSlot memo → TurnView → Markdown 重解析),实测
 * 一次重对齐重建 50 个回合子树(0.21 s);内容未变时这里是 0。
 */
export function applySnapshot(
  draft: SessionState,
  page: {
    server_epoch?: string;
    snapshot?: TimelineSnapshot | null;
    has_more?: boolean;
    total_turns?: number;
    truncated_before?: boolean;
  },
  ts: number = now(),
): SessionState {
  // 陈旧页整页丢弃:同一 server_epoch 下 watermark 只能前进。宿主流那一侧的 cursor
  // 也只向前(`seq <= cursor` 直接拒发,见 qaqh-client/src/timeline.rs),所以把
  // watermark 往回写等于让那段 delta 永远拿不回来——表现是流式文本回滚后再不自愈。
  // 换 epoch 时 seq 可能从小数重新开始,那一刻必须允许重设基线。
  const incomingWatermark = page.snapshot?.watermark ?? 0;
  const epochChanged = page.server_epoch != null && page.server_epoch !== draft.serverEpoch;
  if (!epochChanged && incomingWatermark < draft.watermark) return draft;
  const raw = page.snapshot?.turns ?? [];
  const keys = installTurns(draft, raw.map((record) => buildTurn(record, ts)));
  const claimed = new Set(keys);
  const slots: Slot[] = keys.map((key) => ({ kind: "turn", key }));
  for (const slot of draft.slots) {
    if (slot.kind !== "turn" || claimed.has(slot.key)) continue;
    // 页面里没有、但仍在运行的回合留在尾部:它是这份快照取完之后才开的,权威页
    // 里不可能有它——把它连数据一起删掉,表现就是正在流式的回合闪断。
    if (draft.turns[slot.key]?.status === "running") slots.push({ kind: "turn", key: slot.key });
  }
  for (const key of Object.keys(draft.turns)) {
    if (!claimed.has(key) && draft.turns[key]?.status !== "running") delete draft.turns[key];
  }
  mergeSlots(draft, slots);
  draft.watermark = incomingWatermark;
  draft.serverEpoch = page.server_epoch ?? null;
  draft.hasMore = page.has_more === true;
  draft.truncatedBefore = page.truncated_before === true;
  draft.totalTurns = page.total_turns ?? draft.slots.length;
  const oldest = raw[0];
  draft.oldestIndex = typeof oldest?.turn_index === "number" ? oldest.turn_index : null;
  recomputeDerived(draft);
  return draft;
}

/** 本地回合表的认人方式:全局序号(`#i` 键)与实时路径留下的裸 turn_id。 */
function localTable(draft: SessionState): { byIndex: Map<number, string>; byId: Map<string, string> } {
  const byIndex = new Map<number, string>();
  const byId = new Map<string, string>();
  for (const [key, turn] of Object.entries(draft.turns)) {
    if (turn.turnIndex == null) byId.set(turn.id, key);
    else byIndex.set(turn.turnIndex, key);
  }
  return { byIndex, byId };
}

/** 一条权威回合记录对应本地哪个 key:序号优先(turn_id 会复用,当不了稳定键)。 */
function locateLocal(table: { byIndex: Map<number, string>; byId: Map<string, string> }, turnIndex: number | null, turnId: string): string | null {
  const byIndex = turnIndex == null ? undefined : table.byIndex.get(turnIndex);
  return byIndex ?? table.byId.get(turnId) ?? null;
}

/**
 * 权威回合并入本地表,返回页面顺序对应的本地 key。本地没有就按权威 key 入表;
 * 认得出同一个回合就保住原对象(只有内容真变了才整对象替换),并把实时路径
 * 拿不到的全局序号补记上去——否则同一次会话里同一个回合会有两个 key,重对齐时
 * 尾部回合整棵重挂。
 */
function installTurns(draft: SessionState, built: Turn[]): string[] {
  const table = localTable(draft);
  const keys: string[] = [];
  const used = new Set<string>();
  for (const turn of built) {
    const known = locateLocal(table, turn.turnIndex, turn.id);
    const key = known ?? turn.key;
    if (used.has(key)) continue; // 一页不会给同一个回合发两条记录;真发生了也只挂一条槽(重复 key 会让 `<For>` 行为未定)
    used.add(key);
    if (known == null) {
      draft.turns[turn.key] = turn;
      if (turn.turnIndex != null) table.byIndex.set(turn.turnIndex, turn.key);
      else table.byId.set(turn.id, turn.key);
      keys.push(turn.key);
      continue;
    }
    const current = draft.turns[known]!;
    turn.key = known; // 表键与 turn.key 必须一致(TurnView 用 turn.key 开关展开)
    if (turn.turnIndex != null) {
      table.byIndex.set(turn.turnIndex, known);
      if (current.turnIndex !== turn.turnIndex) current.turnIndex = turn.turnIndex;
    }
    carryLocalFacts(current, turn);
    if (!deepEqual(current, turn)) draft.turns[known] = turn;
    keys.push(known);
  }
  return keys;
}

/**
 * 后端条目不带 epoch-ms(契约缺口 TS-1),快照重建会把本地采样到的耗时清零,
 * 还会把用户展开的时间线收起。本地已有的事实带过去,不算发明数据。
 */
function carryLocalFacts(prev: Turn, next: Turn): void {
  next.expanded = prev.expanded;
  next.workStartedAt = prev.workStartedAt ?? next.workStartedAt;
  if (prev.answer != null && next.answer != null && prev.answerStepId === next.answerStepId && prev.answer.text === next.answer.text) {
    next.answer = { ...next.answer, startedAt: prev.answer.startedAt };
  }
  for (const step of next.steps) {
    const before = prev.steps.find((candidate) => candidate.id === step.id);
    if (before == null) continue;
    step.startedAt = before.startedAt ?? step.startedAt;
    step.endedAt = before.endedAt ?? step.endedAt;
    if (step.seq === 0 && before.seq !== 0) step.seq = before.seq;
    if (step.kind === "tool" && before.kind === "tool") {
      // 快照没带 progress 时保留实时攒下的尾窗,别把它清空。
      step.progressTail = step.progressTail ?? before.progressTail;
    }
  }
}

/**
 * 顺序并入槽位表:内容没变(kind + key,占位再比高度)的下标一个都不写。
 * 整表替换会让 `slots` 的每个订阅者重跑;逐位写只通知真动了的那几位,而
 * `<For keyed>` 按 key 认行——排列变化对它是搬家,不是重建。
 */
function mergeSlots(draft: SessionState, next: Slot[]): void {
  for (let index = 0; index < next.length; index += 1) {
    const want = next[index]!;
    const current = draft.slots[index];
    if (current == null) {
      draft.slots.push(want);
    } else if (!sameSlot(current, want)) {
      draft.slots[index] = want;
    }
  }
  if (draft.slots.length > next.length) draft.slots.splice(next.length);
}

function sameSlot(a: Slot, b: Slot): boolean {
  if (a.kind !== b.kind || a.key !== b.key) return false;
  // 权威页永远不会带 gap 槽(gap 只由本地淘汰产生),所以这里实际只会走到
  // 「kinds 不同」的分支;留着是为了并排比较时不会误判成相同而漏写。
  if (a.kind === "gap" && b.kind === "gap") return gapHeight(a.spans) === gapHeight(b.spans);
  return true;
}

/** 翻页前插:按 `turn_index` 认人(后端明示 turn_id 会复用,不能当稳定键)。 */
export function prependPage(
  draft: SessionState,
  page: {
    snapshot?: TimelineSnapshot | null;
    has_more?: boolean;
    total_turns?: number;
  },
  ts: number = now(),
): SessionState {
  const table = localTable(draft);
  const incoming = page.snapshot?.turns ?? [];
  const fresh: Slot[] = [];
  let oldestAdded: number | null = null; // 本页实际新增的最旧回合 = 下一次翻页游标
  for (const raw of incoming) {
    const turn = buildTurn(raw, ts);
    // 本地已有这一回合就跳过:不重复挂行,也不动它(挂过的行有自己的身份与滚动位置)。
    if (locateLocal(table, turn.turnIndex, turn.id) != null) continue;
    draft.turns[turn.key] = turn;
    if (turn.turnIndex != null) table.byIndex.set(turn.turnIndex, turn.key);
    else table.byId.set(turn.id, turn.key);
    fresh.push({ kind: "turn", key: turn.key });
    if (oldestAdded == null && turn.turnIndex != null) oldestAdded = turn.turnIndex;
  }
  if (fresh.length > 0) draft.slots.unshift(...fresh);
  draft.hasMore = page.has_more === true;
  if (page.total_turns != null) draft.totalTurns = page.total_turns;
  if (oldestAdded != null) {
    draft.oldestIndex = oldestAdded;
  } else if (incoming.length > 0 && fresh.length === 0) {
    // 整页都是重复:游标保持不变,has_more 照页面如实记录。
  } else {
    draft.hasMore = false;
  }
  recomputeDerived(draft);
  return draft;
}

/** 保留运行中回合;返回窗口里(非占位)回合数。 */
export function countLoadedTurns(draft: SessionState): number {
  return draft.slots.filter((slot) => slot.kind === "turn").length;
}

/** 当前未封闭的 reasoning 块:从最新的运行中回合往前找(思考链只显示一行)。
 *  快照里 sealed 的块在 buildTurn 已带 endedAt,因此这条判据对实时/快照两条路径
 *  都成立;回合一旦不再运行,它的思考链也就不再显示。 */
function findOpenReasoning(draft: SessionState): { id: string; turnKey: string } | null {
  for (let i = draft.slots.length - 1; i >= 0; i -= 1) {
    const slot = draft.slots[i]!;
    if (slot.kind !== "turn") continue;
    const turn = draft.turns[slot.key];
    if (turn == null || turn.status !== "running") continue;
    for (let j = turn.steps.length - 1; j >= 0; j -= 1) {
      const step = turn.steps[j]!;
      if (step.kind === "thinking" && step.endedAt == null) return { id: step.id, turnKey: turn.key };
    }
  }
  return null;
}

export { emptySession };
