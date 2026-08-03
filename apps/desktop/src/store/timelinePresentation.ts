import type { RawRound, RawSessionState, RawTurn, TurnStatus } from "./rawSession";
import type { TimelineSnapshot, TimelineTurn } from "./timelineProtocol";

function status(state: TimelineTurn["state"]): TurnStatus {
  return state;
}

/** 解析 "t12" → 12（t 前缀 + 十进制序号）。 */
function turnSeq(turnId: string): number {
  const parsed = /^t(\d+)$/.exec(turnId);
  return parsed ? Number(parsed[1]) : Number.MAX_SAFE_INTEGER;
}

/**
 * Merges the authoritative Ringing conversation store into the timeline
 * projection. The timeline snapshot is a best-effort async checkpoint: a
 * daemon restart can lose its tail (crash between wake and persist), leaving
 * the snapshot behind the message store — the last turns then disappear from
 * the transcript while the session-list title (driven by the independent
 * meta.last_summary path) keeps updating. Timeline entries win when both
 * sources know a turn (richer block data); store-only turns are appended so
 * the conversation stays visible and the gap self-heals once the timeline
 * catches up on the next snapshot.
 */
export function mergeTimelinePresentation(
  seed: string,
  snapshot: TimelineSnapshot,
  fallback: RawSessionState,
  revisionFor?: (turnId: string) => number,
): RawSessionState {
  const projected = selectTimelinePresentation(seed, snapshot, fallback, revisionFor);
  const timelineIds = new Set(projected.turns.map(turn => turn.turnId));
  const missing = fallback.turns.filter(turn => !timelineIds.has(turn.turnId));
  if (missing.length === 0) return projected;
  const turns = [...projected.turns, ...missing].sort(
    (left, right) => turnSeq(left.turnId) - turnSeq(right.turnId),
  );
  return {
    ...projected,
    turns,
    session: { ...projected.session, totalTurns: turns.length },
  };
}

type CachedTurn = { signature: string; value: RawTurn };
const turnCaches = new WeakMap<TimelineSnapshot, Map<string, CachedTurn>>();

/**
 * Projects only the transcript portion of RawSessionState from Ringing V1 timeline.
 * Control/dashboard/interaction data remains owned by their native control
 * paths during the staged protocol replacement.
 */
export function selectTimelinePresentation(
  seed: string,
  snapshot: TimelineSnapshot,
  fallback: RawSessionState,
  revisionFor?: (turnId: string) => number,
): RawSessionState {
  const cache = turnCaches.get(snapshot) ?? new Map<string, CachedTurn>();
  turnCaches.set(snapshot, cache);
  // Timeline 快照不携带时间字段。对 running turn 以投影时刻作为近似活跃
  // 时间；投影对象被 WeakMap 按 signature 缓存，turn 内容不变时时间戳
  // 不会刷新——空闲的僵尸 turn 因此会自然老化并触发卡死检测（4 分钟），
  // 而真实流式中的 turn 因每次 delta 都重建而持续刷新时间戳。
  const projectedAt = Date.now();
  const turns: RawTurn[] = snapshot.turns.map(turn => {
    // The materialized snapshot mutates in place for live entries. A compact
    // per-turn signature lets sealed/history turns retain their object
    // identity while only the changed turn is rebuilt. A new authoritative
    // snapshot is a new WeakMap key and therefore replaces the cache.
    // The monitor tracks the authoritative Timeline turn revision. Avoid
    // serializing the full accumulated answer for every frame; retain the
    // JSON fallback for isolated callers that do not own that revision map.
    const signature = revisionFor
      ? String(revisionFor(turn.turn_id))
      : JSON.stringify(turn);
    const cached = cache.get(turn.turn_id);
    if (cached?.signature === signature) return cached.value;
    const value: RawTurn = {
    turnId: turn.turn_id,
    userText: turn.user_text,
    status: status(turn.state),
    failure: turn.failure,
    startedAt: turn.state === "running" ? projectedAt : undefined,
    lastActivityAt: turn.state === "running" ? projectedAt : undefined,
    rounds: turn.rounds.map(round => {
      const tools = round.blocks.filter(block => block.kind === "tool" && block.tool);
      // Timeline entries append text while a block is open.  Block sealing is
      // a lifecycle/finality marker, not a presentation barrier: filtering on
      // it kept every `text_delta` out of the active transcript until the
      // terminal event sealed the block.
      const text = (kind: "reasoning" | "text") => round.blocks
        .filter(block => block.kind === kind)
        .map(block => block.text ?? "")
        .join("");
      const activeTool = tools.some(block => block.tool!.state === "prepared" || block.tool!.state === "running");
      const blocks = round.blocks
        .slice()
        .sort((a, b) => a.block_order - b.block_order)
        .map((block) => {
          const meta = {
            blockId: block.block_id,
            blockOrder: block.block_order,
            state: block.state,
          } as const;
          if (block.kind === "reasoning") return { type: "reasoning" as const, content: block.text ?? "", ...meta };
          if (block.kind === "text") return { type: "text" as const, content: block.text ?? "", ...meta };
          if (block.kind === "notice") return { type: "notice" as const, message: block.text ?? "", ...meta };
          if (!block.tool) return null;
          return {
            type: "tool" as const,
            card: {
              id: block.tool.tool_call_id,
              name: block.tool.name,
              args_display: block.tool.args_json ?? "{}",
              args_json: block.tool.args_json ?? "{}",
            },
            ...meta,
          };
        })
        .filter((block): block is NonNullable<typeof block> => block !== null);
      const value: RawRound = {
        roundNum: round.round_num,
        isFinal: round.is_final,
        // Sealed is the Markdown visibility boundary. This deliberately
        // prevents an incomplete token fragment from becoming rendered MD.
        thinking: text("reasoning"),
        answer: text("text"),
        blocks,
        toolCalls: tools.map(block => ({
          id: block.tool!.tool_call_id,
          name: block.tool!.name,
          args_display: block.tool!.args_json ?? "{}",
          args_json: block.tool!.args_json ?? "{}",
        })),
        toolResults: Object.fromEntries(tools
          .filter(block => block.tool!.state === "succeeded" || block.tool!.state === "failed")
          .map(block => [block.tool!.tool_call_id, {
            status: block.tool!.state === "succeeded" ? "ok" as const : "error" as const,
            summary: block.tool!.summary ?? block.tool!.output ?? "",
            data: {},
            model: {
              text: block.tool!.output ?? block.tool!.summary ?? "",
              truncated: false,
              total_tokens: 0,
            },
            output_ref: null,
            error: null,
          }])),
        progress: Object.fromEntries(tools
          .filter(block => Boolean(block.tool!.progress))
          .map(block => [block.tool!.tool_call_id, {
            chunks: [{ stream: "stdout", seq: 0, chunk: block.tool!.progress ?? "" }],
          }])),
        phase: round.sealed ? "complete" : activeTool ? "tool_calling" : "answering",
      };
      return value;
    }),
    interactions: [],
    };
    cache.set(turn.turn_id, { signature, value });
    return value;
  });
  return {
    ...fallback,
    seed,
    turns,
    session: { ...fallback.session, totalTurns: turns.length },
  };
}
