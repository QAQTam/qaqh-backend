// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { describe, expect, it } from "vitest";
import type { ProcessItem } from "../../presentation/processAggregation";
import ProcessTimeline from "./ProcessTimeline";

describe("ProcessTimeline", () => {
  it("renders an aggregate as one row and keeps failures separate", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const items: ProcessItem[] = [
      {
        kind: "group", id: "reads", family: "read", label: "Viewed 4 files",
        children: [],
      },
      {
        kind: "tool", id: "build", family: "exec", toolName: "exec_run",
        summary: "Frontend build failed", success: false,
      },
    ];
    const dispose = render(() => <ProcessTimeline items={items} />, host);
    expect(host.textContent).toContain("Viewed 4 files");
    expect(host.textContent).toContain("Frontend build failed");
    expect(host.querySelectorAll("[data-process-row]")).toHaveLength(2);
    dispose();
    host.remove();
  });

  it("preserves an open tool detail when streaming output changes", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [items, setItems] = createSignal<ProcessItem[]>([{
      kind: "tool", id: "exec-1", family: "exec", toolName: "exec_run",
      summary: "pnpm test", progress: [{ stream: "stdout", seq: 1, chunk: "first" }],
    }]);
    const dispose = render(() => <ProcessTimeline items={items()} />, host);
    (host.querySelector("[data-process-row] button") as HTMLButtonElement).click();
    await Promise.resolve();
    expect(host.querySelector("[data-process-row]")?.getAttribute("aria-expanded")).toBe("true");
    setItems(current => current.map(item => item.kind === "tool" ? {
      ...item,
      progress: [...(item.progress ?? []), { stream: "stdout", seq: 2, chunk: "next chunk" }],
    } : item));
    await Promise.resolve();
    expect(host.querySelector("[data-process-row]")?.getAttribute("aria-expanded")).toBe("true");
    expect(host.textContent).toContain("next chunk");
    dispose();
    host.remove();
  });
});
