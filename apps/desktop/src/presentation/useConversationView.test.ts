import { expect, it } from "vitest";
import type { Agent2Ui, TurnData } from "../lib/types";
import { viewsFromEvents, viewsFromRestore } from "./useConversationView";

it("projects restored history and equivalent live events identically", () => {
  const turn: TurnData = {
    turn_id: "turn-1",
    user_text: "hello",
    rounds: [{
      round_num: 0,
      is_final: true,
      thinking: "inspect",
      answer: "done",
      tool_calls: [],
      tool_results: [],
    }],
  };
  const events: Agent2Ui[] = [
    { type: "turn_start", turn_id: "turn-1", user_text: "hello" },
    {
      type: "round_complete", turn_id: "turn-1", round_num: 0,
      thinking: "inspect", answer: "done", tool_calls: [], blocks: [], is_final: true,
    },
    { type: "turn_end", turn_id: "turn-1" },
  ];

  expect(viewsFromEvents("seed-a", events, 100)).toEqual(viewsFromRestore("seed-a", [turn]));
});
