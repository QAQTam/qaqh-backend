import type { SessionActivity } from "../lib/types";
import {
  mergeSessionActivitySnapshot,
  parseSessionActivity,
  upsertSessionActivity,
  type SessionActivityMap,
} from "./sessionActivityStore";

export async function startSessionActivityClient(options: {
  listen: (handler: (payload: unknown) => void) => Promise<() => void>;
  loadSnapshot: () => Promise<unknown[]>;
  onChange: (activities: SessionActivityMap) => void;
  onError?: (error: unknown) => void;
}): Promise<() => void> {
  let current: SessionActivityMap = {};
  const publish = (next: SessionActivityMap) => {
    if (next === current) return;
    current = next;
    options.onChange(current);
  };
  const applyLive = (payload: unknown) => {
    try {
      publish(upsertSessionActivity(current, parseSessionActivity(payload)));
    } catch (error) {
      options.onError?.(error);
    }
  };

  const stop = await options.listen(applyLive);
  try {
    const snapshot: SessionActivity[] = [];
    for (const payload of await options.loadSnapshot()) {
      try {
        snapshot.push(parseSessionActivity(payload));
      } catch (error) {
        options.onError?.(error);
      }
    }
    publish(mergeSessionActivitySnapshot(current, snapshot));
  } catch (error) {
    options.onError?.(error);
  }
  return stop;
}
