import { expect, it, vi } from "vitest";
import { cleanupViewResources } from "./viewLifecycle";

it("flushes runtimes before removing listeners and never receives a backend closer", () => {
  const order: string[] = [];
  const runtime = { dispose: vi.fn(() => order.push("runtime")) };
  const listener = vi.fn(() => order.push("listener"));
  const theme = vi.fn(() => order.push("theme"));

  cleanupViewResources([runtime], [listener], theme);

  expect(order).toEqual(["runtime", "listener", "theme"]);
});
