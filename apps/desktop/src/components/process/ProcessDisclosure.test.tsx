// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { describe, expect, it } from "vitest";
import ProcessDisclosure from "./ProcessDisclosure";

describe("ProcessDisclosure", () => {
  it("starts collapsed when defaultOpen is false", () => {
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => <ProcessDisclosure status="running" defaultOpen={false} />, host);

    expect(host.querySelector("button")?.getAttribute("aria-expanded")).toBe("false");
    dispose();
    host.remove();
  });

  it("keeps a user expansion while the process remains active", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const [revision, setRevision] = createSignal(0);
    const dispose = render(() => (
      <ProcessDisclosure status="running" defaultOpen={false}>
        <span>{revision()}</span>
      </ProcessDisclosure>
    ), host);

    const trigger = host.querySelector<HTMLButtonElement>("button")!;
    trigger.click();
    await Promise.resolve();
    expect(trigger.getAttribute("aria-expanded")).toBe("true");

    setRevision(1);
    await Promise.resolve();
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    dispose();
    host.remove();
  });

  it.each(["completed", "failed", "cancelled"] as const)(
    "closes when status becomes %s",
    async (terminal) => {
      const host = document.createElement("div");
      document.body.append(host);
      const [status, setStatus] = createSignal<"running" | typeof terminal>("running");
      const dispose = render(() => (
        <ProcessDisclosure status={status()} defaultOpen={false} />
      ), host);

      const trigger = host.querySelector<HTMLButtonElement>("button")!;
      trigger.click();
      await Promise.resolve();
      expect(trigger.getAttribute("aria-expanded")).toBe("true");

      setStatus(terminal);
      await Promise.resolve();
      expect(trigger.getAttribute("aria-expanded")).toBe("false");
      dispose();
      host.remove();
    },
  );
});
