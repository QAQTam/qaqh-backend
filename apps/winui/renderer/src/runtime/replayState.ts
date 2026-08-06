/** True while session replay is in progress — suppress side-effect toasts. */
let replaying = false;

export function setReplaying(value: boolean) {
  replaying = value;
}

export function isReplaying() {
  return replaying;
}

/** Accumulate error messages seen during replay to avoid duplicates on re-switch. */
const seenReplayErrors = new Set<string>();

export function registerReplayError(message: string) {
  seenReplayErrors.add(message);
}

export function hasReplayError(message: string): boolean {
  return seenReplayErrors.has(message);
}

export function clearReplayErrors() {
  seenReplayErrors.clear();
}
