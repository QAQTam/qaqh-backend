import { expect, it, vi } from "vitest";
import { startSessionActivityClient } from "./sessionActivityClient";

it("subscribes before loading a snapshot and keeps the newest sequence", async () => {
  const order: string[] = [];
  let pushLive: ((payload: unknown) => void) | undefined;
  const onChange = vi.fn();

  const stop = await startSessionActivityClient({
    listen: async handler => {
      order.push("listen");
      pushLive = handler;
      return () => {};
    },
    loadSnapshot: async () => {
      order.push("snapshot");
      pushLive?.({ seed: "a", state: "working", seq: 3, updated_at: 3 });
      return [{ seed: "a", state: "idle", seq: 2, updated_at: 2 }];
    },
    onChange,
  });

  expect(order).toEqual(["listen", "snapshot"]);
  expect(onChange.mock.calls[onChange.mock.calls.length - 1]?.[0].a.state).toBe("working");
  stop();
});

it("keeps valid session rows when one snapshot entry is malformed", async () => {
  const onChange = vi.fn();
  const onError = vi.fn();

  await startSessionActivityClient({
    listen: async () => () => {},
    loadSnapshot: async () => [
      { seed: "a", state: "idle", seq: 1, updated_at: 1 },
      { seed: "b", state: "future", seq: 1, updated_at: 1 },
    ],
    onChange,
    onError,
  });

  expect(onChange.mock.calls[onChange.mock.calls.length - 1]?.[0].a.state).toBe("idle");
  expect(onError).toHaveBeenCalledOnce();
});
