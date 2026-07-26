import type { RawSessionState } from "../store/rawSession";

export function shouldAttemptSavedResume(
  savedSeed: string | null,
  sessions: Array<{ seed: string }>,
  listingSucceeded: boolean,
): boolean {
  return !!savedSeed && (!listingSucceeded || sessions.some(session => session.seed === savedSeed));
}

export function hasRestorableTranscript(state: RawSessionState | undefined): boolean {
  return (state?.turns.length ?? 0) > 0;
}
