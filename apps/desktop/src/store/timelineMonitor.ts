import { createSignal } from "solid-js";
import type { TimelineEntry, TimelineSnapshot, TimelineSnapshotResponse } from "./timelineProtocol";

/**
 * Renderer-side owner for native transcript recovery. It neither interprets
 * Ringing envelopes nor merges channels: an entry is accepted only at the
 * next timeline sequence, otherwise the caller must request a new snapshot.
 */
export function createTimelineMonitor() {
  const snapshots = new Map<string, TimelineSnapshot>();
  const [version, setVersion] = createSignal(0);

  function handleSnapshot(response: TimelineSnapshotResponse): void {
    if (response.schema !== "deepx.Timeline" || response.version !== 3) return;
    if (!Number.isSafeInteger(response.snapshot?.watermark) || response.snapshot.watermark < 0) return;
    snapshots.set(response.seed, structuredClone(response.snapshot));
    setVersion(value => value + 1);
  }

  function handleEntry(seed: string, entry: TimelineEntry): boolean {
    const snapshot = snapshots.get(seed);
    if (!snapshot || !Number.isSafeInteger(entry.timeline_seq)) return false;
    if (entry.timeline_seq <= snapshot.watermark) return true; // duplicate replay
    if (entry.timeline_seq !== snapshot.watermark + 1) return false;
    if (!applyEntry(snapshot, entry)) return false;
    snapshot.watermark = entry.timeline_seq;
    setVersion(value => value + 1);
    return true;
  }

  return {
    version,
    snapshotFor: (seed: string) => snapshots.get(seed),
    hasSnapshot: (seed: string) => snapshots.has(seed),
    handleSnapshot,
    handleEntry,
  };
}

function applyEntry(snapshot: TimelineSnapshot, entry: TimelineEntry): boolean {
  const turn = snapshot.turns.find(value => value.turn_id === entry.turn_id);
  const event = entry.event;
  switch (event.type) {
    case "turn_opened":
      if (turn) return false;
      snapshot.turns.push({
        turn_id: entry.turn_id,
        user_text: event.user_text,
        sealed: false,
        state: "running",
        rounds: [],
      });
      return true;
    case "turn_sealed":
      if (!turn) return false;
      turn.sealed = true;
      turn.state = event.state;
      turn.failure = event.failure;
      return true;
    default:
      if (!turn || entry.round_num === undefined) return false;
  }
  const round = turn.rounds.find(value => value.round_num === entry.round_num);
  switch (event.type) {
    case "block_opened": {
      const opening = event.block;
      if (round?.blocks.some(block => block.block_id === opening.block_id)) return false;
      if (round) round.blocks.push(opening);
      else turn.rounds.push({ round_num: entry.round_num, sealed: false, is_final: false, blocks: [opening] });
      return true;
    }
    case "round_sealed":
      if (!round) return false;
      round.sealed = true;
      round.is_final = event.is_final;
      return true;
    case "text_delta":
      if (!round) return false;
      {
      const block = round.blocks.find(value => value.block_id === event.block_id);
      if (!block) return false;
      if (block.kind !== "text" && block.kind !== "reasoning") return false;
      block.text = `${block.text ?? ""}${event.delta}`;
      return true;
      }
    case "tool_updated":
      if (!round) return false;
      {
      const block = round.blocks.find(value => value.block_id === event.block_id);
      if (!block) return false;
      if (block.kind !== "tool") return false;
      block.tool = event.tool;
      return true;
      }
    case "tool_progress":
      if (!round) return false;
      {
      const block = round.blocks.find(value => value.block_id === event.block_id);
      if (!block) return false;
      if (!block.tool) return false;
      block.tool.progress = `${block.tool.progress ?? ""}${event.chunk}`;
      return true;
      }
    case "block_sealed":
      if (!round) return false;
      {
      const block = round.blocks.find(value => value.block_id === event.block_id);
      if (!block) return false;
      block.state = "sealed";
      return true;
      }
    default:
      return false;
  }
}
