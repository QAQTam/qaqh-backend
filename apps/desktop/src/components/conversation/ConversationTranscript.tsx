import { createEffect, createSignal, For, onCleanup, onSettled, Show } from "solid-js";
import type { TurnViewModel } from "../../presentation/turnProjection";
import type { ChangeReviewFile } from "../../presentation/turnProjection";
import TurnGroup from "./TurnGroup";

const BOTTOM_THRESHOLD = 120;

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
  let followingTail = true;
  const [followTail, setFollowTail] = createSignal(true);

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
    const remaining = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    followingTail = remaining < BOTTOM_THRESHOLD;
    setFollowTail(followingTail);
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
    () => props.turns,
    () => queueMicrotask(scheduleScrollToBottom),
  );

  onSettled(() => {
    if (typeof ResizeObserver === "undefined") return;
    resizeObserver = new ResizeObserver(() => scheduleScrollToBottom());
    resizeObserver.observe(transcript);
  });

  onCleanup(() => {
    if (scrollFrame !== undefined) cancelAnimationFrame(scrollFrame);
    resizeObserver?.disconnect();
  });

  return (
    <div class="conversation-scroll" ref={scroller} onScroll={measure}>
      <Show when={props.hasMore && props.onLoadMore}>
        <button
          type="button"
          data-load-more
          class="load-more-turns"
          onClick={() => void loadOlder()}
        >加载更早消息</button>
      </Show>
      <main ref={transcript} class="conversation-transcript" aria-live="polite">
        <For each={props.turns} keyed={false}>{(turn) => <TurnGroup turn={turn()} onReviewChanges={props.onReviewChanges} />}</For>
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
