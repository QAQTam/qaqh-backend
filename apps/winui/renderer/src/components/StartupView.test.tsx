// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import StartupView from "./StartupView";
import { createI18n, I18nCtx } from "../i18n";

describe("StartupView", () => {
  afterEach(() => {
    document.body.replaceChildren();
  });

  it("opens Chromium DevTools from the home page", () => {
    const openDevTools = vi.fn(() => Promise.resolve(true));
    const previousDeepx = window.deepx;
    Object.defineProperty(window, "deepx", {
      configurable: true,
      value: { desktop: { openDevTools } },
    });

    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => (
      <I18nCtx value={createI18n("en")}>
        <StartupView sessions={[]} onResume={vi.fn()} showHeatmap={false} />
      </I18nCtx>
    ), host);

    host.querySelector<HTMLButtonElement>("[data-open-devtools]")!.click();
    expect(openDevTools).toHaveBeenCalledOnce();

    dispose();
    Object.defineProperty(window, "deepx", { configurable: true, value: previousDeepx });
  });
});
