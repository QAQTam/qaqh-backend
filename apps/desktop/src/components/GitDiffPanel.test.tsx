// @vitest-environment jsdom

import { request } from "../runtime/backendClient";
import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { createI18n, I18nCtx } from "../i18n";
import GitDiffPanel, { type GitFileEntry } from "./GitDiffPanel";

vi.mock("../runtime/backendClient", () => ({ request: vi.fn() }));

const invokeMock = vi.mocked(request);

function file(overrides: Partial<GitFileEntry> = {}): GitFileEntry {
  return {
    path: "src/main.rs",
    change: "modified",
    lines_added: 3,
    lines_removed: 1,
    ...overrides,
  };
}

const sampleFiles: GitFileEntry[] = [
  file({ path: "src/main.rs", change: "modified", lines_added: 3, lines_removed: 1 }),
  file({ path: "src/lib.rs", change: "added", lines_added: 42, lines_removed: 0 }),
  file({ path: "Cargo.toml", change: "modified", lines_added: 2, lines_removed: 2 }),
  file({ path: "old.txt", change: "deleted", lines_added: 0, lines_removed: 10 }),
];

const sampleBranches = [
  { name: "main", current: true },
  { name: "feature/foo", current: false },
];

/** Set up the backend request mock to dispatch by domain method. */
function mockInvoke(opts: {
  diff?: GitFileEntry[] | Error;
  branches?: { name: string; current: boolean }[];
  fileDiff?: string | Error;
  commit?: string | Error;
} = {}) {
  invokeMock.mockImplementation(async (cmd: string, args?: any) => {
    switch (cmd) {
      case "git.diff": {
        const val = opts.diff ?? sampleFiles;
        if (val instanceof Error) throw val;
        return val;
      }
      case "git.branches": {
        const val = opts.branches ?? sampleBranches;
        return val;
      }
      case "git.file_diff": {
        const val = opts.fileDiff;
        if (val instanceof Error) throw val;
        return val ?? "";
      }
      case "git.commit": {
        const val = opts.commit;
        if (val instanceof Error) throw val;
        return val ?? "ok";
      }
      case "git.switch_branch":
        return "switched";
      default:
        return undefined;
    }
  });
}

function setup() {
  const i18n = createI18n("zh");
  const host = document.createElement("div");
  document.body.append(host);

  const [open, setOpen] = createSignal(true);
  const onClose = vi.fn(() => setOpen(false));

  const dispose = render(
    () => (
      <I18nCtx value={i18n}>
        <GitDiffPanel open={open()} seed="test-seed" onClose={onClose} />
      </I18nCtx>
    ),
    host,
  );

  return { host, dispose, open, onClose };
}

function flush() {
  return new Promise((r) => setTimeout(r, 40));
}

describe("GitDiffPanel", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("1. renders file list with change types, paths, and stats", async () => {
    mockInvoke();

    const { host, dispose } = setup();
    await flush();

    const text = host.textContent ?? "";
    expect(text).toContain("src/main.rs");
    expect(text).toContain("src/lib.rs");
    expect(text).toContain("Cargo.toml");
    expect(text).toContain("old.txt");
    expect(text).toContain("+3");
    expect(text).toContain("-1");
    expect(text).toContain("+42");

    dispose();
    host.remove();
  });

  it("2. clicking a file requests git.file_diff", async () => {
    mockInvoke({ fileDiff: "<div>diff</div>" });

    const { host, dispose } = setup();
    await flush();

    const fileRows = host.querySelectorAll(".git-file-item");
    expect(fileRows.length).toBeGreaterThan(0);
    (fileRows[0] as HTMLElement).click();
    await flush();

    const diffCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === "git.file_diff",
    );
    expect(diffCalls.length).toBeGreaterThanOrEqual(1);
    expect(diffCalls[0]?.[1]).toMatchObject({
      seed: "test-seed",
      filePath: "src/main.rs",
    });

    dispose();
    host.remove();
  });

  it("3. load failure shows the actual error and a retry action", async () => {
    mockInvoke({ diff: new Error("not a git repository"), branches: [] });

    const { host, dispose } = setup();
    await flush();

    expect(host.textContent).toContain("not a git repository");
    expect(host.querySelector(".git-workspace-error button")).not.toBeNull();

    dispose();
    host.remove();
  });

  it("4. commit submit is disabled when message is empty", async () => {
    mockInvoke();

    const { host, dispose } = setup();
    await flush();

    const submitBtn = host.querySelector<HTMLButtonElement>(".git-commit-submit");
    expect(submitBtn).toBeTruthy();
    expect(submitBtn!.disabled).toBe(true);

    dispose();
    host.remove();
  });

  it("5. does NOT offer stage/unstage/discard/reset operations", async () => {
    mockInvoke();

    const { host, dispose } = setup();
    await flush();

    const text = host.textContent ?? "";
    expect(text).not.toContain("stage");
    expect(text).not.toContain("unstage");
    expect(text).not.toContain("暂存");

    const branchSelect = host.querySelector<HTMLSelectElement>(".git-workspace-branch-select")!;
    branchSelect.value = "feature/foo";
    branchSelect.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();

    expect(host.textContent).not.toContain("Discard");
    expect(host.textContent).not.toContain("丢弃");

    const resetBtn = [...host.querySelectorAll("button")].find(
      (b) => b.textContent?.includes("reset") || b.textContent?.includes("Reset"),
    );
    expect(resetBtn).toBeUndefined();

    dispose();
    host.remove();
  });

  it("6. non-git workspace is distinguished from a clean repository", async () => {
    mockInvoke({ diff: new Error("not a git repository"), branches: [] });

    const { host, dispose } = setup();
    await flush();

    expect(host.textContent).toContain("not a git repository");

    dispose();
    host.remove();
  });

  it("7. close button calls onClose", async () => {
    mockInvoke();

    const { host, dispose, onClose } = setup();
    await flush();

    const closeBtn = host.querySelector<HTMLButtonElement>(
      'button[aria-label="Close"]',
    );
    expect(closeBtn).toBeTruthy();
    closeBtn!.click();
    expect(onClose).toHaveBeenCalled();

    dispose();
    host.remove();
  });

  it("8. commit success requests git.commit with message", async () => {
    mockInvoke();

    const { host, dispose } = setup();
    await flush();

    const input = host.querySelector<HTMLInputElement>(".git-commit-input");
    expect(input).toBeTruthy();
    input!.value = "fix: update modules";
    input!.dispatchEvent(new Event("input", { bubbles: true }));
    await flush();

    const submitBtn = host.querySelector<HTMLButtonElement>(".git-commit-submit");
    expect(submitBtn).toBeTruthy();
    expect(submitBtn!.disabled).toBe(false);
    submitBtn!.click();
    await flush();
    await flush();

    const commitCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === "git.commit",
    );
    expect(commitCalls.length).toBeGreaterThanOrEqual(1);
    expect(commitCalls[0]?.[1]).toMatchObject({
      seed: "test-seed",
      message: "fix: update modules",
    });

    dispose();
    host.remove();
  });

  it("9. when open=false the overlay is not rendered", async () => {
    mockInvoke();

    const i18n = createI18n("zh");
    const host = document.createElement("div");
    document.body.append(host);
    const [open, setOpen] = createSignal(false);

    const dispose = render(
      () => (
        <I18nCtx value={i18n}>
          <GitDiffPanel open={open()} seed="test-seed" onClose={() => {}} />
        </I18nCtx>
      ),
      host,
    );
    await flush();

    const overlay = host.querySelector(".git-workspace-overlay");
    expect(overlay).toBeNull();

    dispose();
    host.remove();
  });

  it("10. shows unified as active and split as explicitly unavailable", async () => {
    mockInvoke();
    const { host, dispose } = setup();
    await flush();

    const unified = host.querySelector<HTMLButtonElement>('[aria-label="Unified diff"]');
    const split = host.querySelector<HTMLButtonElement>('[aria-label="Split diff"]');
    expect(unified).not.toBeNull();
    expect(unified?.disabled).toBe(false);
    expect(unified?.classList.contains("active")).toBe(true);
    expect(split).not.toBeNull();
    expect(split?.disabled).toBe(true);

    dispose();
    host.remove();
  });

  it("11. switches branch directly when the worktree is clean", async () => {
    mockInvoke({ diff: [] });
    const { host, dispose } = setup();
    await flush();

    const select = host.querySelector<HTMLSelectElement>(".git-workspace-branch-select")!;
    select.value = "feature/foo";
    select.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();

    expect(invokeMock.mock.calls).toContainEqual([
      "git.switch_branch",
      { seed: "test-seed", branch: "feature/foo", stash: false },
    ]);
    dispose();
    host.remove();
  });
});
