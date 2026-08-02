import type { RawRound, RawSessionState, RawTurn, TurnStatus } from "./rawSession";
import type { TimelineSnapshot, TimelineTurn } from "./timelineProtocol";

function status(state: TimelineTurn["state"]): TurnStatus {
  return state;
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
