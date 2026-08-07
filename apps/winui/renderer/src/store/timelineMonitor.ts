import { createSignal } from "solid-js";
import type { TimelineEntry, TimelineSnapshot, TimelineSnapshotResponse } from "./timelineProtocol";

/**
 * Renderer-side owner for native transcript recovery. It neither interprets
 * Ringing envelopes nor merges channels: an entry is accepted only at the
 * next timeline sequence, otherwise the caller must request a new snapshot.
 */
export function createTimelineMonitor() {
  const snapshots = new Map<string, TimelineSnapshot>();
  const turnRevisions = new Map<string, Map<string, number>>();
  const [version, setVersion] = createSignal(0);
  const dirtyTurns = new Map<string, Set<string>>();
  let frame: number | undefined;

  const notifyFrame = () => {
    if (frame !== undefined) return;
    const schedule = typeof requestAnimationFrame === "function"
      ? requestAnimationFrame
      : (callback: FrameRequestCallback) => setTimeout(() => callback(Date.now()), 0) as unknown as number;
    frame = schedule(() => {
      frame = undefined;
      dirtyTurns.clear();
      setVersion(value => value + 1);
    });
  };

  const cancelFrame = () => {
    if (frame === undefined) return;
    if (typeof cancelAnimationFrame === "function") cancelAnimationFrame(frame);
    else clearTimeout(frame);
    frame = undefined;
  };

  function handleSnapshot(response: TimelineSnapshotResponse): void {
    if (response.schema !== "deepx.Ringing" || response.version !== 1) return;
    if (!Number.isSafeInteger(response.snapshot?.watermark) || response.snapshot.watermark < 0) return;
    const existing = snapshots.get(response.seed);
    // 防回退：daemon 崩溃窗口（异步 checkpoint 落后于已发出的 entry）会让
    // 重启后恢复的快照 watermark 低于 renderer 内存态。无条件替换会回退
    // cursor，后续新 entry 被 handleEntry 的 `<= watermark` 判为重复而静默
    // 丢弃——新 turn 的内容从此消失。保留更新的内存快照，让 entry 流继续
    // 从现有 watermark 续传（daemon 的 timeline_seq 持久化单调，不会倒退）。
    if (existing && response.snapshot.watermark < existing.watermark) return;
    cancelFrame();
    snapshots.set(response.seed, structuredClone(response.snapshot));
    turnRevisions.set(
      response.seed,
      new Map(response.snapshot.turns.map(turn => [turn.turn_id, 0])),
    );
    setVersion(value => value + 1);
  }

  function handleEntry(seed: string, entry: TimelineEntry): boolean {
    const snapshot = snapshots.get(seed);
    if (!snapshot || !Number.isSafeInteger(entry.timeline_seq)) return false;
    if (entry.timeline_seq <= snapshot.watermark) return true; // duplicate replay
    if (entry.timeline_seq !== snapshot.watermark + 1) return false;
    if (!applyEntry(snapshot, entry)) return false;
    snapshot.watermark = entry.timeline_seq;
    const revisions = turnRevisions.get(seed) ?? new Map<string, number>();
    revisions.set(entry.turn_id, (revisions.get(entry.turn_id) ?? 0) + 1);
    turnRevisions.set(seed, revisions);
    const turns = dirtyTurns.get(seed) ?? new Set<string>();
    turns.add(entry.turn_id);
    dirtyTurns.set(seed, turns);
    notifyFrame();
    return true;
  }

  return {
    version,
    snapshotFor: (seed: string) => snapshots.get(seed),
    dirtyTurnIdsFor: (seed: string) => dirtyTurns.get(seed) ?? new Set<string>(),
    turnRevisionFor: (seed: string, turnId: string) =>
      turnRevisions.get(seed)?.get(turnId) ?? 0,
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
      if (turn) {
        // Reopen allowance (mirrors the daemon's TimelineAppender): the
        // message store is the authoritative history, so after a daemon
        // restart the worker's next input can reuse an id the timeline
        // already sealed — orphan-Cancelled by the sealer, or Completed when
        // the restored turn counter lagged further (observed: t14 reused as
        // Completed). Any sealed turn is terminal history; a fresh TurnOpened
        // for the same id resets it in place. Otherwise every subsequent
        // delta is dropped and the transcript stays blank.
        if (turn.sealed) {
          turn.user_text = event.user_text;
          turn.sealed = false;
          turn.state = "running";
          turn.failure = undefined;
          turn.rounds = [];
          return true;
        }
        return false;
      }
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
      if (block.state === "sealed") return false;
      block.text = `${block.text ?? ""}${event.delta}`;
      return true;
      }
    case "tool_updated":
      if (!round) return false;
      {
      const block = round.blocks.find(value => value.block_id === event.block_id);
      if (!block) return false;
      if (block.kind !== "tool") return false;
      if (block.state === "sealed") return false;
      block.tool = event.tool;
      return true;
      }
    case "tool_progress":
      if (!round) return false;
      {
      const block = round.blocks.find(value => value.block_id === event.block_id);
      if (!block) return false;
      if (!block.tool) return false;
      if (block.state === "sealed") return false;
      block.tool.progress = `${block.tool.progress ?? ""}${event.chunk}`;
      return true;
      }
    case "block_sealed":
      if (!round) return false;
      {
      const block = round.blocks.find(value => value.block_id === event.block_id);
      if (!block) return false;
      if (block.state === "sealed") return false;
      block.state = "sealed";
      return true;
      }
    default:
      return false;
  }
}
