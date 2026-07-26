// @vitest-environment jsdom

import { request } from "../runtime/backendClient";
import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { createI18n, I18nCtx, type Lang } from "../i18n";
import SettingsView from "./SettingsView";

vi.mock("../runtime/backendClient", () => ({ request: vi.fn() }));
vi.mock("../runtime/desktopApi", () => ({ confirmDialog: vi.fn(), openDialog: vi.fn() }));

const invokeMock = vi.mocked(request);

type ThemeMode = "system" | "light" | "dark" | "dark-gray";

const sampleProviders = [
  {
    id: "deepseek",
    display: "DeepSeek",
    endpoints: [
      {
        id: "openai",
        display: "OpenAI Compat",
        base_url: "https://api.deepseek.com/v1",
        default_model: "deepseek-chat",
        models: ["deepseek-chat", "deepseek-reasoner"],
      },
    ],
  },
];

function cfg(overrides: Record<string, unknown> = {}) {
  return {
    api_key: "****",
    model: "deepseek-chat",
    base_url: "https://api.deepseek.com/v1",
    provider_id: "deepseek",
    endpoint: "openai",
    max_tokens: 16384,
    context_limit: 1000000,
    reasoning_effort: "high",
    compliance_enabled: true,
    database: { enabled: true },
    providers: sampleProviders,
    subagent: {
      model: "",
      base_url: "",
      api_key: "****",
      max_tokens: 4096,
      timeout_secs: 120,
      default_tools: ["read_file", "search"],
    },
    ...overrides,
  };
}

function setup(props: { lang?: () => Lang; theme?: () => ThemeMode } = {}) {
  const [lang] = createSignal<Lang>(props.lang?.() ?? "zh");
  const [theme] = createSignal<ThemeMode>(props.theme?.() ?? "dark");
  const i18n = createI18n(lang());
  const host = document.createElement("div");
  document.body.append(host);

  const dispose = render(
    () => (
      <I18nCtx value={i18n}>
        <SettingsView
          lang={lang}
          onLangChange={() => {}}
          theme={theme}
          onThemeChange={() => {}}
          permissionLevel={2}
          onPermissionLevelChange={() => {}}
        />
      </I18nCtx>
    ),
    host,
  );

  return { host, dispose, i18n };
}

/** Click a nav item by its label text. */
function clickNav(host: HTMLElement, label: string) {
  const btn = [...host.querySelectorAll<HTMLButtonElement>(".settings-nav-item")].find(
    (b) => b.textContent?.trim() === label,
  );
  if (!btn) throw new Error(`nav item "${label}" not found`);
  btn.click();
}

/** Wait for the config resource to resolve and the settings layout to appear. */
async function waitForLayout(host: HTMLElement) {
  await vi.waitFor(
    () => {
      const layout = host.querySelector(".settings-layout");
      if (!layout) throw new Error("layout not rendered");
    },
    { timeout: 2000 },
  );
  // Extra tick for effects
  await new Promise((r) => setTimeout(r, 10));
}

describe("SettingsView – API Key behavior", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("1. shows configured status when api_key is ****", async () => {
    invokeMock.mockResolvedValue(cfg());

    const { host, dispose } = setup();
    await waitForLayout(host);

    // Navigate to API category
    clickNav(host, "API 与凭据");

    await vi.waitFor(
      () => {
        expect(host.textContent).toContain("已配置");
      },
      { timeout: 1000 },
    );

    dispose();
    host.remove();
  });

  it("2. configured state does NOT put **** into an editable input", async () => {
    invokeMock.mockResolvedValue(cfg());

    const { host, dispose } = setup();
    await waitForLayout(host);

    clickNav(host, "API 与凭据");

    await vi.waitFor(
      () => {
        expect(host.textContent).toContain("已配置");
      },
      { timeout: 1000 },
    );

    // There should be no visible "****" in the DOM
    expect(host.textContent).not.toContain("****");

    // The API key input (if any) should not show "****"
    const inputs = host.querySelectorAll<HTMLInputElement>('input[type="password"], input[type="text"]');
    for (const inp of inputs) {
      if (inp.value.includes("*")) {
        throw new Error("input contains masked key value");
      }
    }

    dispose();
    host.remove();
  });

  it("3. click Replace reveals a new key input", async () => {
    invokeMock.mockResolvedValue(cfg());

    const { host, dispose } = setup();
    await waitForLayout(host);

    clickNav(host, "API 与凭据");

    await vi.waitFor(
      () => {
        expect(host.textContent).toContain("已配置");
      },
      { timeout: 1000 },
    );

    // Find the "替换" (Replace) button
    const replaceBtn = [...host.querySelectorAll("button")].find(
      (b) => b.textContent?.includes("替换"),
    );
    expect(replaceBtn, "replace button exists").toBeDefined();
    replaceBtn!.click();

    // After clicking replace, a password input should appear
    await vi.waitFor(
      () => {
        const pwInput = host.querySelector<HTMLInputElement>(
          "input[type=password]",
        );
        expect(pwInput, "password input appears after replace").toBeTruthy();
      },
      { timeout: 1000 },
    );

    dispose();
    host.remove();
  });

  it("4. save requests config.save with all original fields intact", async () => {
    invokeMock
      .mockResolvedValueOnce(cfg()) // load
      .mockResolvedValueOnce([])  // skills.list_tools
      .mockResolvedValueOnce({ pending: 0 }) // config.database_migration_count
      .mockResolvedValue(undefined); // config.save + any extra

    // For English i18n to get exact button text
    const [lang] = createSignal<Lang>("en");
    const [theme] = createSignal<ThemeMode>("dark");
    const i18n = createI18n("en");
    const host = document.createElement("div");
    document.body.append(host);

    const dispose = render(
      () => (
        <I18nCtx value={i18n}>
          <SettingsView
            lang={lang}
            onLangChange={() => {}}
            theme={theme}
            onThemeChange={() => {}}
            permissionLevel={2}
            onPermissionLevelChange={() => {}}
          />
        </I18nCtx>
      ),
      host,
    );

    // Wait for layout to render (means config loaded)
    await waitForLayout(host);
    // Give the createEffect time to propagate signals
    await new Promise((r) => setTimeout(r, 20));

    // Click the Save button
    const saveBtn = [...host.querySelectorAll("button")].find(
      (b) => b.textContent?.includes("Save"),
    );
    expect(saveBtn).toBeDefined();
    saveBtn!.click();

    // Flush microtasks so the save invoke fires
    await new Promise((r) => setTimeout(r, 50));

    const saveCalls = invokeMock.mock.calls.filter(
      (c) => c[0] === "config.save",
    );
    expect(saveCalls.length).toBeGreaterThanOrEqual(1);

    const saveArgs = saveCalls[0]![1] as Record<string, unknown>;

    // All original fields should be present
    expect(saveArgs.model).toBe("deepseek-chat");
    expect(saveArgs.baseUrl).toBe("https://api.deepseek.com/v1");
    expect(saveArgs.providerId).toBe("deepseek");
    expect(saveArgs.endpoint).toBe("openai");
    expect(saveArgs.maxTokens).toBe(16384);
    expect(saveArgs.contextLimit).toBe(1000000);
    expect(saveArgs.reasoningEffort).toBe("high");
    expect(saveArgs.lang).toBe("en");

    // apiKey should NOT be "****"
    expect(saveArgs.apiKey).not.toBe("****");

    // subagent fields
    expect(saveArgs.subagentMaxTokens).toBe(4096);
    expect(saveArgs.subagentTimeoutSecs).toBe(120);

    // subagent apiKey should NOT be "****"
    expect(saveArgs.subagentApiKey).not.toBe("****");

    // database
    expect(saveArgs.databaseEnabled).toBe(true);

    dispose();
    host.remove();
  });

  it("5. category switch does not lose current form data", async () => {
    invokeMock.mockResolvedValue(cfg());

    const { host, dispose } = setup();
    await waitForLayout(host);

    // Verify we're on the first category ("模型与提供商")
    // The model input should show the loaded value
    const modelInput = host.querySelector<HTMLInputElement>(
      'input[list="model-suggestions"]',
    );
    expect(modelInput?.value).toBe("deepseek-chat");

    // Click a different category
    clickNav(host, "API 与凭据");
    await new Promise((r) => setTimeout(r, 20));

    // Click back to first
    clickNav(host, "模型与提供商");
    await new Promise((r) => setTimeout(r, 20));

    // Model should still be populated
    const modelInputAfter = host.querySelector<HTMLInputElement>(
      'input[list="model-suggestions"]',
    );
    expect(modelInputAfter?.value).toBe("deepseek-chat");

    dispose();
    host.remove();
  });

  it("6. load failure shows a recoverable error, not permanent blank", async () => {
    invokeMock.mockRejectedValue(new Error("connection refused"));

    const { host, dispose } = setup();

    // Wait for the error state
    await vi.waitFor(
      () => {
        // Should show some error indication
        const hasError =
          host.textContent?.includes("错误") ||
          host.textContent?.includes("Error") ||
          host.textContent?.includes("retry") ||
          host.textContent?.includes("Retry") ||
          host.textContent?.includes("重试") ||
          host.querySelector(".settings-error");
        expect(hasError).toBeTruthy();
      },
      { timeout: 2000 },
    );

    // Should show error UI (not permanently blank)
    const errorEl = host.querySelector(".settings-error");
    expect(errorEl).toBeTruthy();
    expect(errorEl!.textContent?.trim().length).toBeGreaterThan(0);

    dispose();
    host.remove();
  });

  it("7. clears a replacement key from the form after save succeeds", async () => {
    invokeMock.mockImplementation(async (command) => {
      if (command === "config.load") return cfg();
      if (command === "skills.list_tools") return [];
      if (command === "config.database_migration_count") return { pending: 0 };
      return undefined;
    });

    const { host, dispose } = setup();
    await waitForLayout(host);
    clickNav(host, "API 与凭据");
    await new Promise((r) => setTimeout(r, 20));

    const replace = [...host.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("替换"),
    )!;
    replace.click();
    await new Promise((r) => setTimeout(r, 20));

    const keyInput = host.querySelector<HTMLInputElement>("input[type=password]")!;
    keyInput.value = "sk-new-secret";
    keyInput.dispatchEvent(new Event("input", { bubbles: true }));

    const save = [...host.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("保存"),
    )!;
    save.click();
    await vi.waitFor(() => {
      expect(invokeMock.mock.calls.some((call) => call[0] === "config.save")).toBe(true);
    });

    await vi.waitFor(() => {
      expect(host.textContent).toContain("已配置");
      expect(host.querySelector<HTMLInputElement>("input[type=password]")?.value ?? "").not.toBe("sk-new-secret");
    });

    dispose();
    host.remove();
  });

  it("8. persists and applies the database toggle immediately", async () => {
    invokeMock.mockImplementation(async (command) => {
      if (command === "config.load") return cfg({ database: { enabled: false } });
      if (command === "skills.list_tools") return [];
      if (command === "config.database_migration_count") return { pending: 0 };
      return undefined;
    });

    const { host, dispose } = setup();
    await waitForLayout(host);
    clickNav(host, "数据与存储");
    await new Promise((r) => setTimeout(r, 20));

    const toggle = host.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
    toggle.checked = true;
    toggle.dispatchEvent(new Event("change", { bubbles: true }));

    await vi.waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("config.set_database_enabled", { enabled: true });
    });

    dispose();
    host.remove();
  });
});
