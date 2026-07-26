import type { Agent2Ui } from "../lib/types";

export type ApplySessionEvent = (event: Agent2Ui) => void;

export interface SessionReplayBuffer {
  begin(seed: string): void;
  handleLive(seed: string, event: Agent2Ui, apply: ApplySessionEvent): void;
  complete(seed: string, replayed: Agent2Ui[], apply: ApplySessionEvent): void;
  abort(seed: string, apply: ApplySessionEvent): void;
  clear(): void;
}

function eventSignature(event: Agent2Ui): string {
  return JSON.stringify(event, (_key, value) =>
    typeof value === "bigint" ? { $deepxBigInt: value.toString() } : value,
  );
}

export function createSessionReplayBuffer(): SessionReplayBuffer {
  const replayingSeeds = new Set<string>();
  const bufferedLiveEvents = new Map<string, Agent2Ui[]>();

  return {
    begin(seed) {
      if (replayingSeeds.has(seed)) return;
      replayingSeeds.add(seed);
      bufferedLiveEvents.set(seed, []);
    },
    handleLive(seed, event, apply) {
      if (!replayingSeeds.has(seed)) {
        apply(event);
        return;
      }
      bufferedLiveEvents.get(seed)?.push(event);
    },
    complete(seed, replayed, apply) {
      const replayCounts = new Map<string, number>();
      for (const event of replayed) {
        const signature = eventSignature(event);
        replayCounts.set(signature, (replayCounts.get(signature) ?? 0) + 1);
        apply(event);
      }

      const buffered = bufferedLiveEvents.get(seed) ?? [];
      bufferedLiveEvents.delete(seed);
      replayingSeeds.delete(seed);
      for (const event of buffered) {
        const signature = eventSignature(event);
        const remaining = replayCounts.get(signature) ?? 0;
        if (remaining > 0) {
          replayCounts.set(signature, remaining - 1);
          continue;
        }
        apply(event);
      }
    },
    abort(seed, apply) {
      this.complete(seed, [], apply);
    },
    clear() {
      replayingSeeds.clear();
      bufferedLiveEvents.clear();
    },
  };
}
