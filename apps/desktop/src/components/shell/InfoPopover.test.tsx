// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createRawSessionState, reduceAgentEvent } from "../../store/sessionEventReducer";
import { createI18n, I18nCtx } from "../../i18n";
import InfoPopover from "./InfoPopover";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.innerHTML = ""; });

describe("InfoPopover", () => {
  it("updates token and cache data as soon as usage arrives", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [session, setSession] = createSignal(createRawSessionState("seed-1"));
    dispose = render(() => (
      <I18nCtx value={createI18n("zh")}><InfoPopover
        session={session()}
        workspace="F:/repo"
      /></I18nCtx>
    ), host);

    expect(host.textContent).toContain("等待用量数据");
    setSession(reduceAgentEvent(session(), {
      type: "usage_updated",
      turn_id: "t1",
      round_num: 0,
      model: "deepseek-v4-pro",
      context_limit: 1_000_000,
      usage: {
        prompt_tokens: 100_000,
        completion_tokens: 5_000,
        total_tokens: 105_000,
        prompt_cache_hit_tokens: 80_000,
        prompt_cache_miss_tokens: 20_000,
        reasoning_tokens: 3_000,
        cache_usage_reported: true,
      },
    }, 100));
    await Promise.resolve();

    expect(host.textContent).toContain("deepseek-v4-pro");
    expect(host.textContent).toContain("80.0%");
    expect(host.textContent).toContain("100.0K");
    expect(host.textContent).toContain("105.0K");
    expect(host.textContent).toContain("缓存覆盖 1/1 次请求");
  });

  it("distinguishes a reported zero-percent hit rate from missing cache data", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [session, setSession] = createSignal(createRawSessionState("seed-cache-zero"));
    dispose = render(() => (
      <I18nCtx value={createI18n("zh")}><InfoPopover
        session={session()}
        workspace="F:/repo"
      /></I18nCtx>
    ), host);
    const usage = {
      prompt_tokens: 100,
      completion_tokens: 10,
      total_tokens: 110,
      prompt_cache_hit_tokens: 0,
      prompt_cache_miss_tokens: 100,
      reasoning_tokens: 0,
    };

    setSession(reduceAgentEvent(session(), {
      type: "usage_updated",
      turn_id: "t1",
      round_num: 0,
      model: "deepseek-chat",
      context_limit: 128_000,
      usage: { ...usage, cache_usage_reported: true },
    }, 100));
    await Promise.resolve();
    expect(host.querySelector(".info-cache-label strong")?.textContent).toBe("0.0%");

    setSession(reduceAgentEvent(session(), {
      type: "usage_updated",
      turn_id: "t1",
      round_num: 0,
      model: "provider-without-cache-data",
      context_limit: 128_000,
      usage,
    }, 101));
    await Promise.resolve();
    expect(host.querySelector(".info-cache")).toBeNull();
  });

  it("normalizes changed files and opens their Git diff", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const onOpenDiff = vi.fn();
    const session = createRawSessionState("seed-1");
    session.environment.changedFiles = ["F:\\repo\\src\\feature\\Panel.tsx"];
    dispose = render(() => (
      <I18nCtx value={createI18n("zh")}><InfoPopover
        session={session} workspace={"F:\\repo"} onOpenDiff={onOpenDiff}
      /></I18nCtx>
    ), host);

    const file = host.querySelector<HTMLButtonElement>(".environment-file")!;
    expect(file.textContent).toBe("src/feature/Panel.tsx");
    file.click();
    expect(onOpenDiff).toHaveBeenCalledWith("src/feature/Panel.tsx");
  });
});
