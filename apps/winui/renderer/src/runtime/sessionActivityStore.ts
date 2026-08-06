import type { SessionActivity, SessionActivityState } from "../lib/types";

export type SessionActivityMap = Record<string, SessionActivity>;

const ACTIVITY_STATES: ReadonlySet<SessionActivityState> = new Set([
  "starting",
  "idle",
  "working",
  "waiting_user",
  "disconnected",
]);

export function parseSessionActivity(value: unknown): SessionActivity {
  if (!value || typeof value !== "object") throw new Error("session activity must be an object");
  const candidate = value as Partial<SessionActivity>;
  if (typeof candidate.seed !== "string") throw new Error("invalid session activity seed");
  if (!ACTIVITY_STATES.has(candidate.state as SessionActivityState)) {
    throw new Error("invalid session activity state");
  }
  if (typeof candidate.seq !== "number" || typeof candidate.updated_at !== "number") {
    throw new Error("invalid session activity sequence");
  }
  if (candidate.turn_id !== undefined && typeof candidate.turn_id !== "string") {
    throw new Error("invalid session activity turn_id");
  }
  return candidate as SessionActivity;
}

export function upsertSessionActivity(
  current: SessionActivityMap,
  next: SessionActivity,
): SessionActivityMap {
  const previous = current[next.seed];
  if (previous && previous.seq >= next.seq) return current;
  return { ...current, [next.seed]: next };
}

export function mergeSessionActivitySnapshot(
  current: SessionActivityMap,
  snapshot: SessionActivity[],
): SessionActivityMap {
  return snapshot.reduce(upsertSessionActivity, current);
}
