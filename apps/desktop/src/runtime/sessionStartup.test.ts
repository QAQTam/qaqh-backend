import { expect, it } from "vitest";
import { createRawSessionState } from "../store/sessionEventReducer";
import { hasRestorableTranscript, shouldAttemptSavedResume } from "./sessionStartup";

it("keeps a saved session recoverable when listing or resume temporarily fails", () => {
  expect(shouldAttemptSavedResume("seed-a", [], false)).toBe(true);
  expect(shouldAttemptSavedResume("seed-a", [], true)).toBe(false);

  const restored = createRawSessionState("seed-a");
  restored.turns.push({
    turnId: "turn-1", userText: "visible after refresh", status: "completed",
    rounds: [], interactions: [],
  });
  expect(hasRestorableTranscript(restored)).toBe(true);
  expect(hasRestorableTranscript(undefined)).toBe(false);
});
