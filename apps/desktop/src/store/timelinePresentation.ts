import type { RawRound, RawSessionState, RawTurn, TurnStatus } from "./rawSession";
import type { TimelineSnapshot, TimelineTurn } from "./timelineProtocol";

function status(state: TimelineTurn["state"]): TurnStatus {
  return state;
}

/**
 * Projects only the transcript portion of RawSessionState from Timeline v3.
 * Control/dashboard/interaction data remains owned by their native control
 * paths during the staged protocol replacement.
 */
export function selectTimelinePresentation(
  seed: string,
  snapshot: TimelineSnapshot,
  fallback: RawSessionState,
): RawSessionState {
  const turns: RawTurn[] = snapshot.turns.map(turn => ({
    turnId: turn.turn_id,
    userText: turn.user_text,
    status: status(turn.state),
    rounds: turn.rounds.map(round => {
      const tools = round.blocks.filter(block => block.kind === "tool" && block.tool);
      const text = (kind: "reasoning" | "text") => round.blocks
        .filter(block => block.kind === kind && block.state === "sealed")
        .map(block => block.text ?? "")
        .join("");
      const activeTool = tools.some(block => block.tool!.state === "prepared" || block.tool!.state === "running");
      const value: RawRound = {
        roundNum: round.round_num,
        isFinal: round.is_final,
        // Sealed is the Markdown visibility boundary. This deliberately
        // prevents an incomplete token fragment from becoming rendered MD.
        thinking: text("reasoning"),
        answer: text("text"),
        blocks: [],
        toolCalls: tools.map(block => ({
          id: block.tool!.tool_call_id,
          name: block.tool!.name,
          args_display: block.tool!.args_json ?? "{}",
          args_json: block.tool!.args_json ?? "{}",
        })),
        toolResults: Object.fromEntries(tools
          .filter(block => block.tool!.state === "succeeded" || block.tool!.state === "failed")
          .map(block => [block.tool!.tool_call_id, {
            tool_call_id: block.tool!.tool_call_id,
            output: block.tool!.output ?? block.tool!.summary ?? "",
            success: block.tool!.state === "succeeded",
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
  }));
  return {
    ...fallback,
    seed,
    turns,
    session: { ...fallback.session, totalTurns: turns.length },
  };
}
