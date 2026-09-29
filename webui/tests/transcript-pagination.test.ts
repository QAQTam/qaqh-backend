import { expect, test } from "bun:test";
import {
  applyCompactMarker,
  emptyTranscript,
  loadSnapshot,
  prependHistoryPage,
} from "../src/lib/transcript";

function page(startIndex: number, count: number, state = "completed") {
  const turns = Array.from({ length: count }, (_, offset) => ({
    turn_id: `t${startIndex + offset}`,
    turn_index: startIndex + offset,
    user_text: `turn ${startIndex + offset}`,
    state,
    rounds: [],
  }));
  return {
    snapshot: { watermark: startIndex + count, turns },
    has_more: startIndex > 0,
    total_turns: startIndex + count,
  };
}

test("loadSnapshot records the oldest turn_index as the before_index cursor", () => {
  const draft = emptyTranscript();
  loadSnapshot(draft, page(40, 10));
  expect(draft.oldestIndex).toBe(40);
  expect(draft.hasMore).toBe(true);
  expect(draft.order).toEqual(["t40", "t41", "t42", "t43", "t44", "t45", "t46", "t47", "t48", "t49"]);
});

test("prependHistoryPage inserts the older page at the top and advances the cursor", () => {
  const draft = emptyTranscript();
  loadSnapshot(draft, page(40, 10));
  const loaded = prependHistoryPage(draft, page(20, 20), 400);
  void loaded;
  expect(draft.order[0]).toBe("t20");
  expect(draft.order[20]).toBe("t40");
  expect(draft.oldestIndex).toBe(20);
  expect(draft.hasMore).toBe(true);
});

test("prependHistoryPage evicts the tail beyond the cap and marks truncation", () => {
  const draft = emptyTranscript();
  loadSnapshot(draft, page(40, 10));
  // 上限 20：前插 20 条后窗口 30 条 → 淘汰尾部 10 条（t40..t49）。
  prependHistoryPage(draft, page(20, 20), 20);
  expect(draft.order.length).toBe(20);
  expect(draft.turns["t49"]).toBeUndefined();
  expect(draft.turns["t20"]).toBeDefined();
  expect(draft.tailTruncated).toBe(true);
});

test("prependHistoryPage never evicts a running turn", () => {
  const draft = emptyTranscript();
  loadSnapshot(draft, page(40, 9));
  loadSnapshot(draft, {
    snapshot: {
      watermark: 50,
      turns: [...page(40, 9).snapshot!.turns!, { turn_id: "t50", turn_index: 50, user_text: "live", state: "running", rounds: [] }],
    },
    has_more: true,
    total_turns: 51,
  });
  prependHistoryPage(draft, page(20, 20), 20);
  expect(draft.turns["t50"]).toBeDefined();
  expect(draft.turns["t50"]!.state).toBe("running");
});

test("compact marker anchors after the newest turn and survives cursor-less pages", () => {
  const draft = emptyTranscript();
  loadSnapshot(draft, page(40, 10));
  applyCompactMarker(draft, "ckpt_1", 3);
  expect(draft.compactMarker).toEqual({ checkpointId: "ckpt_1", contextRevision: 3, afterTurnId: "t49" });

  // 锚点被淘汰 → 分隔移到顶部（顶部渲染条件 afterTurnId === null）。
  prependHistoryPage(draft, page(20, 20), 20);
  expect(draft.compactMarker?.afterTurnId).toBeNull();
});

test("loadSnapshot resets the compact marker (divider is a live-session affordance)", () => {
  const draft = emptyTranscript();
  loadSnapshot(draft, page(40, 10));
  applyCompactMarker(draft, "ckpt_1", 3);
  loadSnapshot(draft, page(50, 10));
  expect(draft.compactMarker).toBeNull();
});
