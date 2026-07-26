// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { describe, expect, it, vi } from "vitest";
import ThreadHeader from "./ThreadHeader";

describe("ThreadHeader", () => {
  it("shows explicit workspace and compaction actions", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const changeWorkspace = vi.fn();
    const compact = vi.fn();
    const dispose = render(() => (
      <ThreadHeader
        title="Task"
        workspace="F:/DeepX-Fork"
        compacting={false}
        environmentOpen={false}
        onToggleEnvironment={vi.fn()}
        statsOpen={false}
        onToggleStats={vi.fn()}
        onOpenLocation={vi.fn()}
        onChangeWorkspace={changeWorkspace}
        onCompact={compact}
        undoDisabled={false}
        onUndo={vi.fn()}
        petEnabled={false}
        onTogglePet={vi.fn()}
      />
    ), host);

    expect(host.textContent).toContain("DeepX-Fork");
    expect(host.textContent).toContain("整理上下文");
    host.querySelector<HTMLButtonElement>("[data-change-workspace]")!.click();
    expect(changeWorkspace).toHaveBeenCalledOnce();
    dispose();
    host.remove();
  });

  it("exposes authoritative undo and disables it while streaming", () => {
    const onUndo = vi.fn();
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => <ThreadHeader
      title="Task" environmentOpen={false} statsOpen={false} workspace="F:/repo"
      compacting={false} undoDisabled={false}
      onToggleEnvironment={vi.fn()} onToggleStats={vi.fn()} onOpenLocation={vi.fn()}
      onChangeWorkspace={vi.fn()} onCompact={vi.fn()} onUndo={onUndo}
      petEnabled={false} onTogglePet={vi.fn()}
    />, host);
    host.querySelector<HTMLButtonElement>('[data-undo-turn]')!.click();
    expect(onUndo).toHaveBeenCalledOnce();
    dispose();
  });
});
