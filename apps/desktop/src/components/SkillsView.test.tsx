// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { createI18n, I18nCtx } from "../i18n";
import SkillsView from "./SkillsView";
import type { SkillInfo, SkillRuntimeInfo } from "../lib/types";

function skill(overrides: Partial<SkillInfo> = {}): SkillInfo {
  return {
    name: "test-skill",
    description: "A test skill for testing",
    scope: "project",
    source: "skills/test/test-skill",
    ...overrides,
  };
}

const sampleSkills: SkillInfo[] = [
  skill({ name: "deepx-debug", description: "Systematic debugging methodology", scope: "project", source: "skills/deepx/deepx-debug" }),
  skill({ name: "deepx-refactor", description: "Safe refactoring workflow", scope: "project", source: "skills/deepx/deepx-refactor" }),
  skill({ name: "my-custom-skill", description: "A user-defined skill for code review", scope: "user", source: "skills/my-custom-skill" }),
  skill({ name: "another-user-skill", description: "Another user skill", scope: "user", source: "skills/another-user-skill" }),
];

function setup(props: {
  seed?: string;
  available?: SkillInfo[];
  active?: string[];
  onActivate?: (name: string) => Promise<void>;
  onUnload?: (name: string) => Promise<void>;
  onReload?: () => Promise<void>;
} = {}) {
  const i18n = createI18n("zh");
  const host = document.createElement("div");
  document.body.append(host);

  const dispose = render(
    () => (
      <I18nCtx value={i18n}>
        <SkillsView
          seed={props.seed ?? "test-seed"}
          available={props.available ?? sampleSkills}
          active={props.active ?? []}
          onActivate={props.onActivate ?? vi.fn()}
          onUnload={props.onUnload ?? vi.fn()}
          onReload={props.onReload ?? vi.fn()}
        />
      </I18nCtx>
    ),
    host,
  );

  // Allow effects to settle
  return { host, dispose };
}

function flush() {
  return new Promise((r) => setTimeout(r, 20));
}

describe("SkillsView", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("1. groups available skills by project/user scope", async () => {
    const { host, dispose } = setup();
    await flush();

    const text = host.textContent ?? "";

    // Should show scope labels
    expect(text).toContain("项目技能");
    expect(text).toContain("用户技能");

    // Project skills should appear
    expect(text).toContain("deepx-debug");
    expect(text).toContain("deepx-refactor");

    // User skills should appear
    expect(text).toContain("my-custom-skill");
    expect(text).toContain("another-user-skill");

    dispose();
    host.remove();
  });

  it("2. shows active skill names as enabled", async () => {
    const { host, dispose } = setup({
      active: ["deepx-debug"],
    });
    await flush();

    // Active skill should have an "enabled" indicator or toggle state
    const rows = host.querySelectorAll<HTMLElement>(".skill-row");
    let foundActive = false;
    for (const row of rows) {
      const text = row.textContent ?? "";
      if (text.includes("deepx-debug")) {
        // Should have some enabled indicator
        const toggle = row.querySelector(".skill-toggle");
        expect(toggle).toBeTruthy();
        const input = toggle?.querySelector("input");
        expect(input?.checked).toBe(true);
        foundActive = true;
      }
    }
    expect(foundActive).toBe(true);

    dispose();
    host.remove();
  });

  it("3. search filters by name and description", async () => {
    const { host, dispose } = setup();
    await flush();

    // Find the search input
    const searchInput = host.querySelector<HTMLInputElement>(
      ".skill-search input, input[placeholder*='搜索'], input[placeholder*='Search']",
    );
    expect(searchInput).toBeTruthy();

    // Type "debug"
    searchInput!.value = "debug";
    searchInput!.dispatchEvent(new Event("input", { bubbles: true }));
    await flush();

    const text = host.textContent ?? "";
    expect(text).toContain("deepx-debug");
    expect(text).not.toContain("deepx-refactor");
    expect(text).not.toContain("my-custom-skill");

    // Clear and search by description
    searchInput!.value = "code review";
    searchInput!.dispatchEvent(new Event("input", { bubbles: true }));
    await flush();

    const text2 = host.textContent ?? "";
    expect(text2).toContain("my-custom-skill");
    expect(text2).not.toContain("deepx-debug");

    dispose();
    host.remove();
  });

  it("4. enable calls onActivate with the skill name", async () => {
    const onActivate = vi.fn().mockResolvedValue(undefined);
    const { host, dispose } = setup({
      available: [skill({ name: "test-skill", scope: "project" })],
      onActivate,
    });
    await flush();

    // Find the toggle/switch and enable
    const toggle = host.querySelector<HTMLElement>(".skill-toggle");
    expect(toggle).toBeTruthy();
    const checkbox = toggle!.querySelector("input");
    expect(checkbox).toBeTruthy();
    // Simulate checking (currently unchecked)
    checkbox!.checked = true;
    checkbox!.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();

    expect(onActivate).toHaveBeenCalledWith("test-skill");
    expect(onActivate).toHaveBeenCalledTimes(1);

    dispose();
    host.remove();
  });

  it("5. disable calls onUnload with the skill name", async () => {
    const onUnload = vi.fn().mockResolvedValue(undefined);
    const { host, dispose } = setup({
      available: [skill({ name: "test-skill", scope: "project" })],
      active: ["test-skill"],
      onUnload,
    });
    await flush();

    // Find the toggle — it should be checked since skill is active
    const toggle = host.querySelector<HTMLElement>(".skill-toggle");
    const checkbox = toggle!.querySelector<HTMLInputElement>("input");
    expect(checkbox!.checked).toBe(true);

    // Uncheck it
    checkbox!.checked = false;
    checkbox!.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();

    expect(onUnload).toHaveBeenCalledWith("test-skill");
    expect(onUnload).toHaveBeenCalledTimes(1);

    dispose();
    host.remove();
  });

  it("6. pending prevents duplicate submit on same skill", async () => {
    // Create a promise we control
    let resolve: (() => void) | undefined;
    const onActivate = vi.fn().mockImplementation(() => {
      return new Promise<void>((r) => {
        resolve = r;
      });
    });

    const { host, dispose } = setup({
      available: [skill({ name: "test-skill", scope: "project" })],
      onActivate,
    });
    await flush();

    // Click toggle once to start the pending operation
    const toggle = host.querySelector<HTMLElement>(".skill-toggle")!;
    const checkbox = toggle.querySelector<HTMLInputElement>("input")!;
    checkbox.checked = true;
    checkbox.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();

    // While pending, clicking again should be ignored
    checkbox.checked = false;
    checkbox.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();

    // onActivate should still only have been called once
    expect(onActivate).toHaveBeenCalledTimes(1);

    // Resolve
    resolve!();
    await flush();

    dispose();
    host.remove();
  });

  it("7. action reject shows error and restores button", async () => {
    const onActivate = vi.fn().mockRejectedValue(new Error("skill not found"));
    const { host, dispose } = setup({
      available: [skill({ name: "test-skill", scope: "project" })],
      onActivate,
    });
    await flush();

    const toggle = host.querySelector<HTMLElement>(".skill-toggle")!;
    const checkbox = toggle.querySelector<HTMLInputElement>("input")!;
    checkbox.checked = true;
    checkbox.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();
    // Wait for the rejection to propagate
    await new Promise((r) => setTimeout(r, 50));

    // Should show some error text
    const text = host.textContent ?? "";
    expect(text).toContain("skill not found");

    // Toggle should be unchecked again (restored) — re-query after re-render
    const toggleAfter = host.querySelector<HTMLElement>(".skill-toggle")!;
    const checkboxAfter = toggleAfter.querySelector<HTMLInputElement>("input")!;
    expect(checkboxAfter.checked).toBe(false);

    dispose();
    host.remove();
  });

  it("8. empty seed disables all write operations", async () => {
    const onActivate = vi.fn();
    const onUnload = vi.fn();
    const onReload = vi.fn();

    const { host, dispose } = setup({
      seed: "",
      available: [skill({ name: "test-skill", scope: "project" })],
      onActivate,
      onUnload,
      onReload,
    });
    await flush();

    // Should show "no session" message
    const text = host.textContent ?? "";
    expect(text).toContain("打开一个任务");

    // Toggles should be disabled
    const toggle = host.querySelector<HTMLElement>(".skill-toggle");
    if (toggle) {
      const checkbox = toggle.querySelector<HTMLInputElement>("input");
      expect(checkbox?.disabled).toBe(true);
    }

    // Refresh button should be disabled
    const refreshBtn = [...host.querySelectorAll("button")].find(
      (b) => b.textContent?.includes("刷新") || b.textContent?.includes("Refresh"),
    );
    if (refreshBtn) {
      expect(refreshBtn.disabled).toBe(true);
    }

    dispose();
    host.remove();
  });

  it("9. page does not show fake install/marketplace capabilities", async () => {
    const { host, dispose } = setup();
    await flush();

    const text = host.textContent ?? "";

    // Must NOT show fake capabilities
    expect(text).not.toContain("安装");
    expect(text).not.toContain("Install");
    expect(text).not.toContain("市场");
    expect(text).not.toContain("Marketplace");
    expect(text).not.toContain("Download");
    expect(text).not.toContain("删除");
    expect(text).not.toContain("Delete");

    dispose();
    host.remove();
  });

  it("10. keeps activation pending until the authoritative active list updates", async () => {
    const i18n = createI18n("zh");
    const host = document.createElement("div");
    document.body.append(host);
    const [active, setActive] = createSignal<string[]>([]);
    const onActivate = vi.fn().mockResolvedValue(undefined);

    const dispose = render(
      () => (
        <I18nCtx value={i18n}>
          <SkillsView
            seed="test-seed"
            available={[skill({ name: "test-skill" })]}
            active={active()}
            onActivate={onActivate}
            onUnload={vi.fn()}
            onReload={vi.fn()}
          />
        </I18nCtx>
      ),
      host,
    );
    await flush();

    const checkbox = host.querySelector<HTMLInputElement>(".skill-toggle input")!;
    checkbox.checked = true;
    checkbox.dispatchEvent(new Event("change", { bubbles: true }));
    await flush();

    expect(onActivate).toHaveBeenCalledTimes(1);
    expect(host.querySelector(".skill-spinner")).not.toBeNull();

    setActive(["test-skill"]);
    await flush();

    expect(host.querySelector(".skill-spinner")).toBeNull();
    expect(host.querySelector<HTMLInputElement>(".skill-toggle input")?.checked).toBe(true);

    dispose();
    host.remove();
  });

  it("11. shows a recoverable error when catalog refresh fails", async () => {
    const onReload = vi.fn().mockRejectedValue(new Error("agent unavailable"));
    const { host, dispose } = setup({ onReload });
    await flush();

    const refresh = [...host.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("刷新"),
    )!;
    refresh.click();
    await flush();

    expect(onReload).toHaveBeenCalledTimes(1);
    expect(host.textContent).toContain("agent unavailable");
    expect(refresh.disabled).toBe(false);

    dispose();
    host.remove();
  });

  it("12. keeps refresh pending until a new catalog snapshot arrives", async () => {
    const i18n = createI18n("zh");
    const host = document.createElement("div");
    document.body.append(host);
    const [available, setAvailable] = createSignal<SkillInfo[]>(sampleSkills);
    const onReload = vi.fn().mockResolvedValue(undefined);

    const dispose = render(
      () => (
        <I18nCtx value={i18n}>
          <SkillsView
            seed="test-seed"
            available={available()}
            active={[]}
            onActivate={vi.fn()}
            onUnload={vi.fn()}
            onReload={onReload}
          />
        </I18nCtx>
      ),
      host,
    );
    await flush();

    const refresh = [...host.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("刷新"),
    )!;
    refresh.click();
    await flush();

    expect(onReload).toHaveBeenCalledTimes(1);
    expect(refresh.disabled).toBe(true);

    setAvailable([...sampleSkills]);
    await flush();

    expect(refresh.disabled).toBe(false);

    dispose();
    host.remove();
  });

  it("13. renders the five authoritative runtime columns", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const runtime: SkillRuntimeInfo[] = ["catalog", "requested", "active", "review_due", "unavailable"].map((state, index) => ({
      name: `skill-${index}`, description: state, state, source: "test", token_count: index,
    }));
    const dispose = render(() => <I18nCtx value={createI18n("en")}>
      <SkillsView seed="seed" available={[]} active={[]} runtime={runtime}
        onActivate={vi.fn()} onUnload={vi.fn()} onReload={vi.fn()} />
    </I18nCtx>, host);
    await flush();
    for (const label of ["Catalog", "Requested", "Enabled", "Review Due", "Unavailable"]) {
      expect(host.textContent).toContain(label);
    }
    expect(host.querySelectorAll(".skill-column")).toHaveLength(5);
    dispose();
    host.remove();
  });
});
