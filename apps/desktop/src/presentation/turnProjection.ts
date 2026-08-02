import type { RawRound, RawTurn, ToolCallDef } from "../store/rawSession";
import type { ToolStatus } from "../lib/types/ringing/ToolResult";
import { aggregateProcessItems, type ProcessItem } from "./processAggregation";

export type RoundRenderEntry =
  | { kind: "assistant"; id: string; markdown: string; streaming: boolean }
  | { kind: "process"; id: string; items: ProcessItem[]; hasTools: boolean };

export type RoundViewModel = {
  roundNum: number;
  isFinal: boolean;
  entries: RoundRenderEntry[];
};

export type TurnViewModel = {
  turnId: string;
  userPrompt: string;
  status: string;
  /** Total elapsed time across all rounds, or undefined until complete. */
  elapsedMs?: number;
  rounds: RoundViewModel[];
  /** Resolved interactions (ask/plan), shown at turn level. */
  interactions: ProcessItem[];
  /** Total tokens used in this turn, from API usage info. */
  totalTokens?: number;
  /** Approximate tokens per second (total tokens / total elapsed). */
  tokensPerSec?: number;
  /** Files successfully mutated during this turn, for the post-answer review receipt. */
  changes?: ChangeReviewFile[];
};

export type ChangeReviewFile = {
  path: string;
  added: number;
  removed: number;
  /** Exact tool patch when available; undefined means the review panel shows a receipt only. */
  diff?: string;
};

export function toolFamily(name: string): string {
  if (["read", "list", "diff"].includes(name)) return "read";
  if (["write", "edit", "edit_block", "delete"].includes(name)) return "write";
  if (["web", "web_search", "web_fetch"].includes(name)) return "web";
  if (["exec", "spawn_subagent"].includes(name)) return "exec";
  return "tool";
}

function reasoningItem(turnId: string, roundNum: number, ordinal: number, content: string, state?: "open" | "sealed"): ProcessItem {
  return {
    kind: "reasoning",
    id: `${turnId}-reasoning-${roundNum}-${ordinal}`,
    content,
    state,
  };
}

function toolItem(round: RawRound, call: ToolCallDef): Extract<ProcessItem, { kind: "tool" }> {
  const result = round.toolResults[call.id];
  return {
    kind: "tool",
    id: call.id,
    family: toolFamily(call.name),
    toolName: call.name,
    summary: call.args_display || call.name,
    argsJson: call.args_json,
    output: result?.model.text,
    progress: round.progress[call.id]?.chunks,
    status: result?.status,
  };
}

const MUTATING_TOOLS = new Set(["write", "edit", "edit_block", "delete"]);

function extractUnifiedDiff(output: string): string | undefined {
  const start = output.search(/^--- (?:a\/|\/|\S)/m);
  return start >= 0 ? output.slice(start).trim() : undefined;
}

function syntheticNewFileDiff(path: string, argsJson: string): string | undefined {
  try {
    const content = JSON.parse(argsJson).content;
    if (typeof content !== "string" || !content) return undefined;
    const lines = content.replace(/\r\n?/g, "\n").split("\n");
    if (lines[lines.length - 1] === "") lines.pop();
    return `--- /dev/null\n+++ b/${path}\n@@ -0,0 +1,${lines.length} @@\n${lines.map(line => `+${line}`).join("\n")}`;
  } catch {
    return undefined;
  }
}

function changeCount(output: string): { added: number; removed: number } {
  const match = output.match(/\+(\d+)\s+-(\d+)/);
  return match ? { added: Number(match[1]), removed: Number(match[2]) } : { added: 0, removed: 0 };
}

function projectChanges(rawTurn: RawTurn): ChangeReviewFile[] {
  const files = new Map<string, ChangeReviewFile>();
  for (const round of rawTurn.rounds) {
    for (const call of round.toolCalls) {
      if (!MUTATING_TOOLS.has(call.name)) continue;
      const result = round.toolResults[call.id];
      if (!result || !isSuccessfulToolStatus(result.status)) continue;
      let path: string | undefined;
      try { path = JSON.parse(call.args_json).path; } catch { /* tool output remains available */ }
      if (typeof path !== "string" || !path) continue;
      const output = result.model.text;
      const counts = changeCount(output);
      const diff = extractUnifiedDiff(output) ??
        (call.name === "write" ? syntheticNewFileDiff(path, call.args_json) : undefined);
      const previous = files.get(path);
      files.set(path, {
        path,
        added: (previous?.added ?? 0) + counts.added,
        removed: (previous?.removed ?? 0) + counts.removed,
        diff: previous?.diff && diff ? `${previous.diff}\n\n${diff}` : diff ?? previous?.diff,
      });
    }
  }
  return [...files.values()];
}

function isSuccessfulToolStatus(status: ToolStatus): boolean {
  return status === "ok" || status === "backgrounded";
}

function projectRoundEntries(
  turn: RawTurn,
  round: RawRound,
  streaming: boolean,
): RoundRenderEntry[] {
  const entries: RoundRenderEntry[] = [];
  let processItems: ProcessItem[] = [];
  let ordinal = 0;

  const flushProcess = () => {
    if (processItems.length === 0) return;
    const items = aggregateProcessItems(processItems);
    entries.push({
      kind: "process",
      id: `${turn.turnId}-round-${round.roundNum}-process-${ordinal++}`,
      items,
      hasTools: processItems.some(item => item.kind === "tool"),
    });
    processItems = [];
  };

  if (round.blocks.length > 0) {
    for (const block of round.blocks) {
      switch (block.type) {
        case "reasoning":
          if (block.content.trim()) {
            processItems.push(reasoningItem(turn.turnId, round.roundNum, processItems.length, block.content, block.state));
          }
          break;
        case "tool":
          processItems.push(toolItem(round, block.card));
          break;
        case "web_search":
          processItems.push({
            kind: "notice",
            id: `${turn.turnId}-round-${round.roundNum}-search-${processItems.length}`,
            level: "info",
            message: `🔍 搜索: ${block.action}`,
          });
          break;
        case "notice":
          processItems.push({
            kind: "notice",
            id: `${turn.turnId}-round-${round.roundNum}-notice-${processItems.length}`,
            level: "info",
            message: block.message,
          });
          break;
        case "text":
          if (!block.content.trim()) break;
          flushProcess();
          entries.push({
            kind: "assistant",
            id: `${turn.turnId}-round-${round.roundNum}-assistant-${ordinal++}`,
            markdown: block.content,
            // Timeline text lifecycle is authoritative. A completed turn may
            // still contain an open text block after a reconnect, and a
            // running turn may already have sealed an earlier answer block.
            streaming: block.state === undefined ? streaming : block.state === "open",
          });
          break;
      }
    }
    flushProcess();
    return entries;
  }

  // Streaming previews are intentionally phase-exclusive. A provider may emit
  // reasoning deltas before its tool-call preview; retaining both makes the UI
  // look as though it is thinking and executing at once.
  if (round.phase !== "tool_calling" && round.thinking.trim()) {
    processItems.push(reasoningItem(turn.turnId, round.roundNum, 0, round.thinking, "open"));
  }
  if (round.phase !== "thinking") {
    for (const call of round.toolCalls) {
      processItems.push(toolItem(round, call));
    }
  }
  flushProcess();

  if (round.answer.trim()) {
    entries.push({
      kind: "assistant",
      id: `${turn.turnId}-round-${round.roundNum}-assistant-${ordinal}`,
      markdown: round.answer,
      streaming,
    });
  }

  return entries;
}

export function projectTurn(rawTurn: RawTurn): TurnViewModel {
  // streaming 是 round 级而非 turn 级：round_complete 到达（phase=complete）
  // 后该轮正文立即全量渲染，不必等整个 turn 结束（修复 markdown 延迟到
  // 流式结束才渲染的问题）。
  const rounds = rawTurn.rounds.map((round) => ({
    roundNum: round.roundNum,
    isFinal: round.isFinal,
    entries: projectRoundEntries(
      rawTurn,
      round,
      rawTurn.status === "running" && round.phase !== "complete",
    ),
  }));

  // Resolved interactions (ask/plan) belong to the turn, not a specific round.
  const interactions: ProcessItem[] = [];
  for (const interaction of rawTurn.interactions) {
    if (interaction.kind === "permission") continue;
    interactions.push({
      kind: "interaction",
      id: interaction.id,
      label: interaction.kind,
      resolution: interaction.resolution,
    });
  }

  const elapsedMs = rawTurn.startedAt !== undefined && rawTurn.endedAt !== undefined
    ? Math.max(0, rawTurn.endedAt - rawTurn.startedAt)
    : undefined;

  const totalTokens = rawTurn.usage?.total_tokens;
  const tokensPerSec = totalTokens !== undefined && elapsedMs !== undefined && elapsedMs > 0
    ? Math.round(totalTokens / (elapsedMs / 1000))
    : undefined;

  return {
    turnId: rawTurn.turnId,
    userPrompt: rawTurn.userText,
    status: rawTurn.status,
    elapsedMs,
    rounds,
    interactions,
    totalTokens,
    tokensPerSec,
    changes: projectChanges(rawTurn),
  };
}
