/**
 * Timeline reducer: wire entries + snapshots → block tree.
 *
 * Text content is NOT stored here. Text/reasoning blocks own an imperative
 * renderer (streaming-md); the store only carries structure (kind/state/tool),
 * which is what needs to be reactive. `seedText` is the text at block creation
 * (snapshot or checkpoint full replacement) so a fresh renderer can catch up.
 */
export type ToolStateName = "prepared" | "running" | "succeeded" | "failed" | "cancelled" | "backgrounded";

export type ToolPermission = {
  reason: string;
  paths: string[];
  category: string;
  level: number;
  risk: string;
  consequence: string;
};

export type ToolInfo = {
  toolCallId: string;
  name: string;
  state: ToolStateName;
  summary?: string;
  argsJson?: string;
  output?: string;
  diff?: string;
  progress?: string;
  progressTruncated?: boolean;
  /** Newer daemon display projection; the current daemon may omit it. */
  display?: any;
  permission?: ToolPermission | null;
  failure?: { code: string; message: string } | null;
};

export type BlockKind = "text" | "reasoning" | "tool" | "notice";

export type Block = {
  id: string;
  kind: BlockKind;
  state: "open" | "sealed";
  /** Catch-up text for markdown-rendered blocks (text/notice). */
  seedText: string;
  /**
   * Live reasoning text (reasoning blocks only). Rendered as a single-line
   * ticker — the view reads the tail after the last newline, so memory is
   * O(block) but DOM cost is O(1): one text node, updated in place.
   */
  text?: string;
  tool: ToolInfo | null;
};

export type Round = { num: number; blocks: Block[] };

export type TurnState = "running" | "completed" | "failed" | "cancelled";

export type Turn = {
  id: string;
  userText: string;
  state: TurnState;
  failure: { code: string; message: string } | null;
  rounds: Round[];
};

export type TranscriptState = {
  turns: Record<string, Turn>;
  order: string[];
  watermark: number;
  hasMore: boolean;
  totalTurns: number;
};

export function emptyTranscript(): TranscriptState {
  return { turns: {}, order: [], watermark: 0, hasMore: false, totalTurns: 0 };
}

function ensureTurn(draft: TranscriptState, turnId: string): Turn {
  let turn = draft.turns[turnId];
  if (!turn) {
    turn = { id: turnId, userText: "", state: "running", failure: null, rounds: [] };
    draft.turns[turnId] = turn;
    draft.order.push(turnId);
  }
  return turn;
}

function ensureRound(turn: Turn, num: number): Round {
  let round = turn.rounds.find((r) => r.num === num);
  if (!round) {
    round = { num, blocks: [] };
    turn.rounds.push(round);
  }
  return round;
}

function normalizeTool(rawTool: any): ToolInfo {
  const tool = rawTool as ToolInfo;
  const raw = rawTool as Record<string, any>;
  return {
    ...tool,
    argsJson: tool.argsJson ?? raw.args_json,
    progressTruncated: tool.progressTruncated ?? raw.progress_truncated,
  };
}

function findBlock(turn: Turn, blockId: string): Block | undefined {
  for (const round of turn.rounds) {
    const block = round.blocks.find((b) => b.id === blockId);
    if (block) return block;
  }
  return undefined;
}

/** Keep only the live reasoning tail in memory; sealed reasoning is not rendered. */
function reasoningTailText(value: string): string {
  const start = value.lastIndexOf("\n") + 1;
  return value.slice(start);
}

/** Build the tree from a fully materialized timeline page. */
export function loadSnapshot(draft: TranscriptState, page: {
  snapshot?: { watermark?: number; turns?: Array<Record<string, unknown>> };
  has_more?: boolean;
  total_turns?: number;
}): void {
  draft.turns = {};
  draft.order = [];
  for (const raw of page.snapshot?.turns ?? []) {
    const turnId = String(raw.turn_id);
    const turn: Turn = {
      id: turnId,
      userText: String(raw.user_text ?? ""),
      state: (raw.state as TurnState) ?? "completed",
      failure: (raw.failure as Turn["failure"]) ?? null,
      rounds: [],
    };
    for (const rawRound of (raw.rounds as Array<Record<string, unknown>>) ?? []) {
      const round: Round = { num: Number(rawRound.round_num ?? 0), blocks: [] };
      for (const rawBlock of (rawRound.blocks as Array<Record<string, unknown>>) ?? []) {
        round.blocks.push({
          id: String(rawBlock.block_id),
          kind: rawBlock.kind as BlockKind,
          state: (rawBlock.state as Block["state"]) ?? "sealed",
          seedText: rawBlock.kind === "reasoning" ? "" : String(rawBlock.text ?? ""),
          text: rawBlock.kind === "reasoning" ? reasoningTailText(String(rawBlock.text ?? "")) : undefined,
          tool: rawBlock.tool ? normalizeTool(rawBlock.tool) : null,
        });
      }
      turn.rounds.push(round);
    }
    draft.turns[turnId] = turn;
    draft.order.push(turnId);
  }
  draft.watermark = page.snapshot?.watermark ?? 0;
  draft.hasMore = page.has_more === true;
  draft.totalTurns = page.total_turns ?? 0;
}

/** Apply one live timeline entry (structure only; text routing is external). */
export function applyEntry(draft: TranscriptState, entry: {
  timeline_seq: number;
  turn_id: string;
  round_num?: number;
  event: Record<string, any>;
}): void {
  if (typeof entry.timeline_seq !== "number") return;
  const turn = ensureTurn(draft, entry.turn_id);
  const event = entry.event ?? {};
  switch (event.type) {
    case "turn_opened":
      turn.userText = event.user_text ?? turn.userText;
      break;
    case "block_opened": {
      const raw = event.block ?? {};
      const round = ensureRound(turn, entry.round_num ?? 0);
      if (!round.blocks.some((b) => b.id === raw.block_id)) {
        round.blocks.push({
          id: String(raw.block_id),
          kind: raw.kind as BlockKind,
          state: (raw.state as Block["state"]) ?? "open",
          seedText: raw.kind === "reasoning" ? "" : String(raw.text ?? ""),
          text: raw.kind === "reasoning" ? reasoningTailText(String(raw.text ?? "")) : undefined,
          tool: raw.tool ? normalizeTool(raw.tool) : null,
        });
      }
      break;
    }
    case "block_checkpoint": {
      const block = findBlock(turn, event.block_id);
      if (block && event.text) block.seedText = event.text;
      break;
    }
    case "tool_updated": {
      const block = findBlock(turn, event.block_id);
      if (block) block.tool = event.tool ? normalizeTool(event.tool) : block.tool;
      break;
    }
    case "tool_progress": {
      const block = findBlock(turn, event.block_id);
      if (block?.tool) {
        // Bounded tail: a running tool can stream tens of KB of output; keeping
        // the whole thing as one growing string is O(n) memory and O(n^2) copy.
        // The live view is a tail window (TUI parity); the sealed block's full
        // output arrives via tool_updated/sealed snapshot.
        const PROGRESS_WINDOW = 16 * 1024;
        const next = (block.tool.progress ?? "") + (event.chunk ?? "");
        block.tool.progress = next.length > PROGRESS_WINDOW ? next.slice(-PROGRESS_WINDOW) : next;
        block.tool.progressTruncated = block.tool.progressTruncated || event.truncated === true || next.length > PROGRESS_WINDOW;
      }
      break;
    }
    case "block_sealed": {
      const block = findBlock(turn, event.block_id);
      if (block) block.state = "sealed";
      break;
    }
    case "turn_sealed":
      turn.state = event.state ?? "completed";
      turn.failure = event.failure ?? null;
      break;
    default:
      break;
  }
}

/** Set the seed text for a checkpoint full replacement (store side only). */
export function setSeedText(draft: TranscriptState, blockId: string, text: string): void {
  for (const turnId of draft.order) {
    const block = findBlock(draft.turns[turnId]!, blockId);
    if (block) {
      block.seedText = text;
      if (block.kind === "reasoning") block.text = reasoningTailText(text);
      return;
    }
  }
}

/** Append live reasoning text (reasoning blocks are rendered as a ticker). */
export function appendBlockText(draft: TranscriptState, blockId: string, delta: string): void {
  for (const turnId of draft.order) {
    const block = findBlock(draft.turns[turnId]!, blockId);
    if (block) {
      block.text = reasoningTailText((block.text ?? "") + delta);
      return;
    }
  }
}

/** Find a block anywhere in the tree (read-only lookup for routing). */
export function findBlockById(state: TranscriptState, blockId: string): Block | undefined {
  for (const turnId of state.order) {
    const block = findBlock(state.turns[turnId]!, blockId);
    if (block) return block;
  }
  return undefined;
}

/** After a snapshot load: the (at most one) reasoning block still streaming. */
export function findOpenReasoningId(state: TranscriptState): string | null {
  for (let i = state.order.length - 1; i >= 0; i -= 1) {
    const turn = state.turns[state.order[i]!];
    if (!turn) continue;
    for (let r = turn.rounds.length - 1; r >= 0; r -= 1) {
      for (const block of turn.rounds[r]!.blocks) {
        if (block.kind === "reasoning" && block.state === "open") return block.id;
      }
    }
  }
  return null;
}
