import { createEffect, createMemo, createSignal, For, onSettled, Show } from "solid-js";
import type { TurnViewModel } from "../../presentation/turnProjection";
import type { ChangeReviewFile } from "../../presentation/turnProjection";
import VirtualTurn from "./VirtualTurn";

const BOTTOM_THRESHOLD = 120;
const ESTIMATED_TURN_HEIGHT = 120;

export default function ConversationTranscript(props: {
  turns: TurnViewModel[];
  hasMore?: boolean;
  onLoadMore?: () => void | Promise<void>;
  onReviewChanges?: (changes: ChangeReviewFile[]) => void;
}) {
  let scroller!: HTMLDivElement;
  let transcript!: HTMLElement;
  let scrollFrame: number | undefined;
  let resizeObserver: ResizeObserver | undefined;
  let observedTail: Element | null = null;
  let observeTailNow: (() => void) | undefined;
  let followingTail = true;
  const [followTail, setFollowTail] = createSignal(true);
  const [heightVersion, setHeightVersion] = createSignal(0);
  const measuredHeights = new Map<string, number>();

  const scrollToBottom = () => {
    if (typeof scroller?.scrollTo === "function") scroller.scrollTo({ top: scroller.scrollHeight });
    else if (scroller) scroller.scrollTop = scroller.scrollHeight;
  };

  const scheduleScrollToBottom = () => {
    if (!followingTail) return;
    if (scrollFrame !== undefined) cancelAnimationFrame(scrollFrame);
    scrollFrame = requestAnimationFrame(() => {
      scrollFrame = undefined;
      if (followingTail) scrollToBottom();
    });
  };

  const measure = () => {
    if (!followingTail) {
      setFollowTail(false);
      return;
    }
    const remaining = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    if (remaining >= BOTTOM_THRESHOLD) followingTail = false;
    setFollowTail(followingTail);
  };

  const captureAnchor = () => {
    if (!scroller || !transcript) return null;
    const scrollerTop = scroller.getBoundingClientRect().top;
    const elements = [...transcript.querySelectorAll<HTMLElement>("[data-turn]")];
    const anchor = elements.find((element) => element.getBoundingClientRect().bottom >= scrollerTop);
    if (!anchor) return null;
    return { id: anchor.dataset.turn ?? "", top: anchor.getBoundingClientRect().top - scrollerTop };
  };

  const compensateAnchor = (anchor: { id: string; top: number } | null) => {
    if (!anchor?.id) return;
    const element = [...transcript.querySelectorAll<HTMLElement>("[data-turn]")]
      .find(candidate => candidate.dataset.turn === anchor.id);
    if (!element) return;
    const current = element.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
    const delta = current - anchor.top;
    if (Math.abs(delta) > 0.5) scroller.scrollTop += delta;
  };

  const measured = (turnId: string, height: number, updatePlaceholders = true) => {
    const previous = measuredHeights.get(turnId);
    if (!height || previous === height) return;
    if (!updatePlaceholders) {
      measuredHeights.set(turnId, height);
      return;
    }
    const anchor = captureAnchor();
    measuredHeights.set(turnId, height);
    setHeightVersion(version => version + 1);
    const elements = [...transcript.querySelectorAll<HTMLElement>("[data-turn]")];
    const targetIndex = elements.findIndex(element => element.dataset.turn === turnId);
    const anchorIndex = anchor ? elements.findIndex(element => element.dataset.turn === anchor.id) : -1;
    const correction = anchorIndex > targetIndex && targetIndex >= 0 ? height - (previous ?? ESTIMATED_TURN_HEIGHT) : 0;
    if (correction) queueMicrotask(() => { scroller.scrollTop += correction; });
  };

  async function loadOlder() {
    if (!props.onLoadMore) return;
    const distanceFromBottom = scroller.scrollHeight - scroller.scrollTop;
    await props.onLoadMore();
    queueMicrotask(() => {
      scroller.scrollTop = Math.max(0, scroller.scrollHeight - distanceFromBottom);
    });
  }

  createEffect(
    () => props.turns.map(turn => `${turn.turnId}:${turn.rounds.length}`).join("|"),
    () => queueMicrotask(scheduleScrollToBottom),
  );

  onSettled(() => {
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      const tailOnly = entries.length > 0
        && entries.every(entry => (entry.target as HTMLElement).dataset.tail === "true");
      const anchor = tailOnly ? null : captureAnchor();
      for (const entry of entries) {
        const element = entry.target as HTMLElement;
        const id = element.dataset.turn;
        if (id) measured(id, element.offsetHeight || entry.contentRect.height, !tailOnly);
      }
      queueMicrotask(() => compensateAnchor(anchor));
      // Empty entries are retained for deterministic test doubles; browsers
      // only auto-follow an observed tail wrapper.
      if (entries.length === 0 || entries.some(entry => (entry.target as HTMLElement).dataset.tail === "true")) {
        scheduleScrollToBottom();
      }
    });
    resizeObserver = observer;
    const observeTail = () => {
      const tail = transcript.querySelector<HTMLElement>("[data-tail='true']");
      if (tail === observedTail) return;
      if (observedTail) observer.unobserve(observedTail);
      observedTail = tail;
      if (tail) observer.observe(tail);
    };
    observeTailNow = observeTail;
    observeTail();
    return () => {
      observer.disconnect();
      if (scrollFrame !== undefined) cancelAnimationFrame(scrollFrame);
    };
  });

  createEffect(
    () => props.turns.map(turn => turn.turnId).join("|"),
    () => queueMicrotask(() => observeTailNow?.()),
  );

  return (
    <div class="conversation-scroll" ref={scroller} onScroll={measure} onWheel={(event) => {
      if (event.deltaY < 0) {
        followingTail = false;
        setFollowTail(false);
      }
    }}>
      <Show when={props.hasMore && props.onLoadMore}>
        <button
          type="button"
          data-load-more
          class="load-more-turns"
          onClick={() => void loadOlder()}
        >加载更早消息</button>
      </Show>
      <main ref={transcript} class="conversation-transcript" aria-live="polite">
        <For each={props.turns} keyed={t => t.turnId}>{(turn) => {
          const isLast = createMemo(() => {
            return props.turns.at(-1)?.turnId === turn().turnId;
          });
          return <VirtualTurn
            turn={turn()}
            root={scroller}
            tail={isLast()}
            // Read the signal so a remounted placeholder receives the cached
            // border-box height instead of returning to the 120px estimate.
            estimatedHeight={heightVersion() >= 0
              ? measuredHeights.get(turn().turnId) ?? ESTIMATED_TURN_HEIGHT
              : ESTIMATED_TURN_HEIGHT}
            onMeasured={measured}
            onReviewChanges={props.onReviewChanges}
          />;
        }}</For>
      </main>
      <Show when={!followTail()}>
        <button
          type="button"
          class="jump-to-bottom"
          aria-label="跳到最新消息"
          onClick={() => {
            followingTail = true;
            setFollowTail(true);
            queueMicrotask(scheduleScrollToBottom);
          }}
        >↓</button>
      </Show>
    </div>
  );
}
