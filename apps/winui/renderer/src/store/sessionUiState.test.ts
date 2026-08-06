import { flush } from "solid-js";
import { expect, it } from "vitest";
import { createSessionUiState } from "./sessionUiState";

it("owns workspace and rejects a duplicate interaction submission", () => {
  const ui = createSessionUiState();
  ui.setWorkspace("F:\\repo-a");
  flush();
  expect(ui.workspace()).toBe("F:\\repo-a");
  expect(ui.beginInteractionSubmit("ask-1")).toBe(true);
  expect(ui.beginInteractionSubmit("ask-1")).toBe(false);
  ui.finishInteractionSubmit("ask-1");
  flush();
  expect(ui.submittingInteractionId()).toBeNull();
});
