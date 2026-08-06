// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { expect, it, vi } from "vitest";
import ChangeReviewPanel from "./ChangeReviewPanel";

vi.mock("../i18n", () => ({
  useI18n: () => ({
    t: () => ({ review: {
      title: "Review changes", changedFiles: "Changed {n} files", close: "Close",
      changedFilesNav: "Changed files", noPatch: "No patch",
    } }),
  }),
}));

it("opens after initially rendering closed", async () => {
  const host = document.createElement("div");
  const [open, setOpen] = createSignal(false);
  const dispose = render(() => <ChangeReviewPanel
    open={open()}
    changes={[{ path: "src/example.ts", added: 1, removed: 0, diff: "--- a/src/example.ts\n+++ b/src/example.ts\n@@ -1 +1 @@\n-old\n+new" }]}
    onClose={() => setOpen(false)}
  />, host);

  expect(host.querySelector(".change-review-overlay")).toBeNull();
  setOpen(true);
  await vi.waitFor(() => expect(host.querySelector(".change-review-overlay")).not.toBeNull());
  expect(host.textContent).toContain("src/example.ts");
  dispose();
});
