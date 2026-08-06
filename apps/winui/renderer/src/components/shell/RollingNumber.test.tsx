// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it } from "vitest";
import RollingNumber, { formatCompactNumber } from "./RollingNumber";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
});

describe("RollingNumber", () => {
  it("rolls only changed digit positions and exposes one accessible value", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [value, setValue] = createSignal(1234);
    dispose = render(() => <RollingNumber value={value()} />, host);

    setValue(1284);
    await Promise.resolve();

    const number = host.querySelector<HTMLElement>(".rolling-number")!;
    expect(number.dataset.value).toBe("1.3K");
    expect(number.getAttribute("aria-label")).toBe("1.3K");
    expect(host.querySelectorAll(".rolling-up")).toHaveLength(1);
    expect(host.querySelector(".rolling-number-old")?.textContent).toBe("2");
  });

  it("rolls downward when a value decreases", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [value, setValue] = createSignal(980);
    dispose = render(() => <RollingNumber value={value()} />, host);

    setValue(970);
    await Promise.resolve();

    expect(host.querySelectorAll(".rolling-down")).toHaveLength(1);
  });

  it("uses a stable non-rolling transition across compact format boundaries", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [value, setValue] = createSignal(999);
    dispose = render(() => <RollingNumber value={value()} />, host);

    setValue(1000);
    await Promise.resolve();

    expect(host.querySelector<HTMLElement>(".rolling-number")?.dataset.value).toBe("1.0K");
    expect(host.querySelectorAll(".rolling-up, .rolling-down")).toHaveLength(0);
    expect(host.querySelectorAll(".rolling-format-shift")).toHaveLength(4);
  });

  it("formats compact token values consistently", () => {
    expect(formatCompactNumber(999)).toBe("999");
    expect(formatCompactNumber(12_500)).toBe("12.5K");
    expect(formatCompactNumber(1_250_000)).toBe("1.25M");
  });
});
