import { describe, expect, it } from "vitest";
import type { RawTurn } from "../store/rawSession";
import { projectTurn } from "./turnProjection";

function rawTurn(): RawTurn {
  return {
    turnId: "turn-1",
    userText: "fix it",
    status: "completed",
    startedAt: 100,
    endedAt: 900,
    interactions: [],
    rounds: [{
      roundNum: 0,
      isFinal: false,
      thinking: "",
      answer: "stale fallback",
      blocks: [
        { type: "text", content: "before tool" },
        { type: "tool", card: { id: "read-1", name: "read", args_display: "App.tsx", args_json: "{}" } },
        { type: "text", content: "after tool" },
      ],
      toolCalls: [{ id: "read-1", name: "read", args_display: "App.tsx", args_json: "{}" }],
      toolResults: { "read-1": { tool_call_id: "read-1", output: "source", success: true } },
      progress: {},
      phase: "complete",
    }],
  };
}

describe("turn projection", () => {
  it("preserves text, tool, text order as independent assistant and process entries", () => {
    const view = projectTurn(rawTurn());
    const entries = view.rounds[0]!.entries;

    expect(entries.map(entry => entry.kind)).toEqual(["assistant", "process", "assistant"]);
    expect(entries[0]).toMatchObject({ kind: "assistant", markdown: "before tool", streaming: false });
    expect(entries[1]).toMatchObject({ kind: "process", hasTools: true });
    expect(entries[2]).toMatchObject({ kind: "assistant", markdown: "after tool", streaming: false });
    if (entries[1]?.kind === "process") {
      expect(entries[1].items).toContainEqual(expect.objectContaining({
        kind: "tool", id: "read-1", output: "source", success: true,
      }));
    }

    expect(view.elapsedMs).toBe(800);
    expect(view.status).toBe("completed");
  });

  it("uses answer once as the legacy fallback when ordered blocks are absent", () => {
    const turn = rawTurn();
    turn.rounds[0]!.blocks = [];
    turn.rounds[0]!.thinking = "inspect";
    turn.rounds[0]!.answer = "legacy answer";

    const entries = projectTurn(turn).rounds[0]!.entries;
    expect(entries.map(entry => entry.kind)).toEqual(["process", "assistant"]);
    expect(entries[1]).toMatchObject({ kind: "assistant", markdown: "legacy answer", streaming: false });
  });

  it("does not render answer fallback beside authoritative text blocks", () => {
    const turn = rawTurn();
    turn.rounds[0]!.blocks = [{ type: "text", content: "authoritative" }];
    turn.rounds[0]!.toolCalls = [];
    turn.rounds[0]!.toolResults = {};

    const assistantEntries = projectTurn(turn).rounds[0]!.entries
      .filter((entry): entry is Extract<typeof entry, { kind: "assistant" }> => entry.kind === "assistant");
    expect(assistantEntries).toEqual([expect.objectContaining({ markdown: "authoritative" })]);
  });

  it("projects an active answer-only stream as one transient assistant entry", () => {
    const turn = rawTurn();
    turn.status = "running";
    turn.rounds[0]!.blocks = [];
    turn.rounds[0]!.answer = "forming conclusion";

    const entries = projectTurn(turn).rounds[0]!.entries;
    const assistant = entries.find((entry): entry is Extract<typeof entry, { kind: "assistant" }> =>
      entry.kind === "assistant",
    );
    expect(assistant).toMatchObject({ markdown: "forming conclusion", streaming: true });
  });

  it("hides stale thinking while a live tool preview is active", () => {
    const turn = rawTurn();
    turn.status = "running";
    turn.rounds[0]!.blocks = [];
    turn.rounds[0]!.thinking = "internal analysis";
    turn.rounds[0]!.phase = "tool_calling";

    const process = projectTurn(turn).rounds[0]!.entries.find(entry => entry.kind === "process");
    expect(process).toMatchObject({ kind: "process" });
    if (process?.kind === "process") {
      expect(process.items.some(item => item.kind === "reasoning")).toBe(false);
      expect(process.items.some(item => item.kind === "tool")).toBe(true);
    }
  });

  it("does not expose permission audit resolutions as process items", () => {
    const turn = rawTurn();
    turn.interactions = [
      { id: "exec-1", kind: "permission", resolution: "approved", at: 500 },
    ];

    const view = projectTurn(turn);
    expect(view.interactions).toEqual([]);
  });

  it("builds a review receipt only from successful file mutations", () => {
    const turn = rawTurn();
    turn.rounds[0]!.toolCalls = [
      { id: "edit-1", name: "edit", args_display: "src/a.ts", args_json: '{"path":"src/a.ts"}' },
      { id: "edit-2", name: "edit", args_display: "src/b.ts", args_json: '{"path":"src/b.ts"}' },
    ];
    turn.rounds[0]!.toolResults = {
      "edit-1": {
        tool_call_id: "edit-1",
        output: "[OK] src/a.ts:8 +3 -1 | edit_file\n\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -8 +8 @@\n-old\n+new",
        success: true,
      },
      "edit-2": { tool_call_id: "edit-2", output: "[ERROR] denied", success: false },
    };

    expect(projectTurn(turn).changes).toEqual([expect.objectContaining({
      path: "src/a.ts", added: 3, removed: 1,
      diff: expect.stringContaining("@@ -8 +8 @@"),
    })]);
  });
});
